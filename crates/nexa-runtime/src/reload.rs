use std::collections::VecDeque;
use std::fmt;

use crate::machines::reload;
use crate::scheduler::SchedulerCheckpoint;
use crate::stateful::MigrationLimitError;
use crate::task::TaskExecution;
use crate::{HostCompletionDelivery, ModuleHandle, RuntimeMessage, TaskHandle, TaskSnapshot};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReloadError {
    InvalidState,
    EpochNotNewer,
    HostHashMismatch,
    StagingCapacity,
    MigrationNoOutput,
    MigrationNotFinished,
    MissingForwarding,
    DuplicateForwarding,
    InvalidStateHandle,
    MigrationLimit(MigrationLimitError),
    CompletionBufferCapacity,
    Migration(RuntimeMessage),
    GraphCheck,
    QuiesceTimeout,
    Activation(RuntimeMessage),
}

pub fn validate_reload_completion_capacity(
    capacity: usize,
    buffered: usize,
) -> Result<(), ReloadError> {
    if buffered >= capacity {
        Err(ReloadError::CompletionBufferCapacity)
    } else {
        Ok(())
    }
}

pub fn invoke_reload_activation(
    activation: impl FnOnce() -> Result<(), RuntimeMessage>,
) -> Result<(), ReloadError> {
    activation().map_err(ReloadError::Activation)
}

#[derive(Debug)]
pub(crate) struct ReloadTransaction {
    pub(crate) old_module: ModuleHandle,
    pub(crate) candidate: ModuleHandle,
    pub(crate) paused_tasks: Vec<PausedTask>,
    pub(crate) completions: ReloadCompletionBuffer,
}

#[derive(Debug)]
pub(crate) struct ReloadCompletionBuffer {
    entries: VecDeque<HostCompletionDelivery>,
    capacity: usize,
}

impl ReloadCompletionBuffer {
    #[must_use]
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub(crate) fn push(&mut self, delivery: HostCompletionDelivery) -> Result<(), ReloadError> {
        validate_reload_completion_capacity(self.capacity, self.entries.len())?;
        self.entries.push_back(delivery);
        Ok(())
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn drain_ordered(&mut self) -> impl Iterator<Item = HostCompletionDelivery> + '_ {
        self.entries
            .make_contiguous()
            .sort_by_key(|delivery| delivery.terminal_sequence);
        self.entries.drain(..)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PausedTask {
    pub(crate) handle: TaskHandle,
    pub(crate) snapshot: TaskSnapshot,
    pub(crate) execution: TaskExecution,
    pub(crate) scheduler: SchedulerCheckpoint,
}

#[derive(Debug, Default)]
pub(crate) struct ReloadCoordinator {
    transaction: Option<ReloadTransaction>,
    state: Option<reload::State>,
    staging_heap: i64,
}

impl ReloadCoordinator {
    pub(crate) fn begin(&mut self, transaction: ReloadTransaction) -> Result<(), ReloadError> {
        if self.transaction.is_some() {
            return Err(ReloadError::InvalidState);
        }
        let preparing = reload::apply(reload::State::Planned, reload::Event::Start, |_| true)
            .map_err(|_| ReloadError::InvalidState)?;
        let quiescing = reload::apply(preparing.state, reload::Event::PrepareSucceeded, |_| true)
            .map_err(|_| ReloadError::InvalidState)?;
        self.staging_heap = apply_reload_deltas(0, preparing.deltas)?;
        self.staging_heap = apply_reload_deltas(self.staging_heap, quiescing.deltas)?;
        self.state = Some(quiescing.state);
        self.transaction = Some(transaction);
        Ok(())
    }

    pub(crate) fn quiesced(&mut self) -> Result<(), ReloadError> {
        self.apply(reload::Event::QuiesceSucceeded)
    }

    pub(crate) fn staged(&mut self) -> Result<(), ReloadError> {
        self.apply(reload::Event::StageSucceeded)
    }

    pub(crate) fn publish(&mut self) -> Result<(), ReloadError> {
        self.apply(reload::Event::Publish)
    }

    pub(crate) fn begin_activation(&mut self) -> Result<(), ReloadError> {
        self.apply(reload::Event::BeginActivation)
    }

    pub(crate) fn activation_succeeded(&mut self) -> Result<(), ReloadError> {
        self.apply(reload::Event::ActivationSucceeded)
    }

    pub(crate) fn activation_failed(&mut self) -> Result<(), ReloadError> {
        self.apply(reload::Event::ActivationFailed)
    }

    pub(crate) fn rollback(&mut self) -> Result<(), ReloadError> {
        match self.state.ok_or(ReloadError::InvalidState)? {
            reload::State::Quiescing => self.apply(reload::Event::QuiesceFailed),
            reload::State::Staging | reload::State::Committing => {
                self.apply(reload::Event::StageFailed)
            }
            _ => Err(ReloadError::InvalidState),
        }
    }

    pub(crate) fn transaction(&self) -> Result<&ReloadTransaction, ReloadError> {
        self.transaction.as_ref().ok_or(ReloadError::InvalidState)
    }

    pub(crate) fn transaction_mut(&mut self) -> Result<&mut ReloadTransaction, ReloadError> {
        self.transaction.as_mut().ok_or(ReloadError::InvalidState)
    }

    pub(crate) fn finish(&mut self) -> Result<ReloadTransaction, ReloadError> {
        let transaction = self.transaction.take().ok_or(ReloadError::InvalidState)?;
        self.state = None;
        self.staging_heap = 0;
        Ok(transaction)
    }

    #[must_use]
    pub(crate) fn active(&self) -> bool {
        self.transaction.is_some()
    }

    #[cfg(any(test, feature = "model-adapter"))]
    #[must_use]
    pub(crate) const fn inspection_state(&self) -> Option<reload::State> {
        self.state
    }

    fn apply(&mut self, event: reload::Event) -> Result<(), ReloadError> {
        let outcome = reload::apply(self.state.ok_or(ReloadError::InvalidState)?, event, |_| {
            true
        })
        .map_err(|_| ReloadError::InvalidState)?;
        self.staging_heap = apply_reload_deltas(self.staging_heap, outcome.deltas)?;
        reload::check_invariants(outcome.state, |_| self.staging_heap)
            .map_err(|_| ReloadError::GraphCheck)?;
        self.state = Some(outcome.state);
        Ok(())
    }
}

fn apply_reload_deltas(
    mut staging_heap: i64,
    deltas: &[reload::ResourceDelta],
) -> Result<i64, ReloadError> {
    for delta in deltas {
        if delta.resource != "staging_heap" {
            return Err(ReloadError::GraphCheck);
        }
        staging_heap = staging_heap
            .checked_add(delta.amount)
            .ok_or(ReloadError::GraphCheck)?;
    }
    Ok(staging_heap)
}

impl fmt::Display for ReloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ReloadError {}
