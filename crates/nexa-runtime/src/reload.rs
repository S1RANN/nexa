use std::fmt;

use crate::machines::reload;
use crate::stateful::MigrationLimitError;
use crate::{ModuleHandle, RuntimeMessage};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReloadError {
    InvalidState,
    EpochNotNewer,
    HostContractIdMismatch,
    StagingCapacity,
    MigrationNoOutput,
    MigrationNotFinished,
    MissingForwarding,
    DuplicateForwarding,
    InvalidStateHandle,
    MigrationLimit(MigrationLimitError),
    Migration(RuntimeMessage),
    GraphCheck,
    QuiesceTimeout,
    Activation(RuntimeMessage),
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
    pub(crate) old_module_id: u32,
    pub(crate) old_epoch: u64,
    pub(crate) cancelled_task_count: usize,
    pub(crate) detached_request_count: usize,
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
