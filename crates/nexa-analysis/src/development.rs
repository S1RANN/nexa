//! Runtime-free development coordination shared by Engine and CLI frontends.

use std::collections::{BTreeMap, VecDeque};

use crate::{BuildFingerprint, CandidateIdentity, FreshnessOutcome, PackageId, ResolvedBuildInput};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevelopmentCoordinatorConfig {
    pub stable_scan_count: u8,
    pub queue_capacity: usize,
    pub retain_terminal_generations: usize,
}

impl Default for DevelopmentCoordinatorConfig {
    fn default() -> Self {
        Self {
            stable_scan_count: 2,
            queue_capacity: 16,
            retain_terminal_generations: 128,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevelopmentTerminalKind {
    Compiled,
    CompileFailed,
    VerifyFailed,
    SupersededBeforeCompile,
    SupersededInFlight,
    CancelledByInvalidation,
    CancelledBySourceRemoval,
    CancelledByShutdown,
    RejectedHostContractChange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevelopmentTerminal {
    pub identity: CandidateIdentity,
    pub kind: DevelopmentTerminalKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct DevelopmentObservation {
    pub identity: Option<CandidateIdentity>,
    pub change_detected: bool,
    pub stable: bool,
    pub matched_active: bool,
    pub matched_terminal: bool,
    pub stable_scans: u8,
    pub terminals: Vec<DevelopmentTerminal>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevelopmentQueueOutcome {
    Accepted,
    AlreadyQueued,
    Backpressured(CandidateIdentity),
    Stale(CandidateIdentity),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevelopmentStartOutcome {
    Started,
    AlreadyInFlight,
    NotQueued,
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevelopmentCompletionKind {
    Compiled,
    CompileFailed,
    VerifyFailed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevelopmentCompletionOutcome {
    Accepted(DevelopmentTerminal),
    Rejected {
        terminal: DevelopmentTerminal,
        freshness: FreshnessOutcome,
    },
    AlreadyTerminal {
        identity: CandidateIdentity,
        kind: DevelopmentTerminalKind,
    },
    Stale(CandidateIdentity),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevelopmentInvalidation {
    Transient,
    SourceRemoval,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DevelopmentPackageInspection {
    pub latest_generation: u64,
    pub terminal_generations: u64,
    pub duplicate_terminals: u64,
    pub generations_without_terminal: u64,
    pub desired_build_fingerprint: Option<BuildFingerprint>,
    pub active_build_fingerprint: Option<BuildFingerprint>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DevelopmentCoordinatorInspection {
    pub packages: BTreeMap<PackageId, DevelopmentPackageInspection>,
    pub queued_candidates: usize,
    pub in_flight_candidates: usize,
    pub backpressure_count: u64,
    pub created_generations: u64,
    pub terminal_generations: u64,
    pub duplicate_terminals: u64,
    pub generations_without_terminal: u64,
}

#[derive(Clone, Debug, Default)]
struct PackageState {
    desired: Option<BuildFingerprint>,
    observed: Option<BuildFingerprint>,
    active: Option<BuildFingerprint>,
    terminal_fingerprint: Option<BuildFingerprint>,
    stable_scans: u8,
    stable_emitted_generation: Option<u64>,
    latest_generation: u64,
    live: Option<CandidateIdentity>,
    terminal_generations: BTreeMap<u64, DevelopmentTerminalKind>,
    terminal_generation_count: u64,
    evicted_through_generation: u64,
    duplicate_terminals: u64,
}

/// Coordinates source observations, bounded compile admission, freshness, and exactly-once
/// Generation terminal accounting without depending on a Runtime or compiler implementation.
#[derive(Clone, Debug)]
pub struct DevelopmentCoordinator {
    config: DevelopmentCoordinatorConfig,
    packages: BTreeMap<PackageId, PackageState>,
    pending_order: VecDeque<PackageId>,
    pending: BTreeMap<PackageId, CandidateIdentity>,
    in_flight: BTreeMap<PackageId, CandidateIdentity>,
    backpressure_count: u64,
    stopped: bool,
}

impl DevelopmentCoordinator {
    #[must_use]
    pub fn new(config: DevelopmentCoordinatorConfig) -> Self {
        Self {
            config: DevelopmentCoordinatorConfig {
                stable_scan_count: config.stable_scan_count.max(1),
                queue_capacity: config.queue_capacity.max(1),
                retain_terminal_generations: config.retain_terminal_generations.max(1),
            },
            packages: BTreeMap::new(),
            pending_order: VecDeque::new(),
            pending: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            backpressure_count: 0,
            stopped: false,
        }
    }

    /// Records the currently retained product candidate without creating a development
    /// Generation. This is used for an initial load and after a successful commit.
    pub fn retain_active(&mut self, package_id: PackageId, fingerprint: BuildFingerprint) {
        let state = self.packages.entry(package_id).or_default();
        state.active = Some(fingerprint);
        state.desired = Some(fingerprint);
        state.observed = Some(fingerprint);
        state.terminal_fingerprint = None;
        state.stable_scans = 0;
        state.stable_emitted_generation = None;
    }

    /// Begins an explicit build attempt without write-stability suppression.
    ///
    /// Unlike [`Self::observe`], an explicit attempt always receives a new monotonic Generation,
    /// including retries of a fingerprint that previously reached a terminal state.
    #[must_use]
    pub fn begin(
        &mut self,
        package_id: PackageId,
        fingerprint: BuildFingerprint,
    ) -> (CandidateIdentity, Vec<DevelopmentTerminal>) {
        if self.stopped {
            let state = self.packages.entry(package_id.clone()).or_default();
            state.desired = Some(fingerprint);
            state.observed = Some(fingerprint);
            state.stable_scans = 0;
            state.stable_emitted_generation = None;
            state.latest_generation = state.latest_generation.saturating_add(1).max(1);
            let identity = CandidateIdentity::new(package_id, state.latest_generation, fingerprint)
                .expect("DevelopmentCoordinator generations are non-zero");
            state.live = Some(identity.clone());
            let terminal = self
                .record_terminal(
                    identity.clone(),
                    DevelopmentTerminalKind::CancelledByShutdown,
                )
                .expect("a post-shutdown Generation is immediately terminal");
            return (identity, vec![terminal]);
        }
        let terminals = self.terminalize_live(
            &package_id,
            DevelopmentTerminalKind::SupersededBeforeCompile,
            DevelopmentTerminalKind::SupersededInFlight,
        );
        let state = self.packages.entry(package_id.clone()).or_default();
        state.desired = Some(fingerprint);
        state.observed = Some(fingerprint);
        state.stable_scans = self.config.stable_scan_count;
        state.latest_generation = state.latest_generation.saturating_add(1).max(1);
        let identity = CandidateIdentity::new(package_id, state.latest_generation, fingerprint)
            .expect("DevelopmentCoordinator generations are non-zero");
        state.live = Some(identity.clone());
        state.stable_emitted_generation = Some(identity.generation);
        (identity, terminals)
    }

    /// Observes one complete immutable `BuildFingerprint`.
    ///
    /// A changed observation creates exactly one persistent Generation. Repeated observations make
    /// that Generation stable. Replacing or reverting an unstable Generation terminalizes it
    /// before returning, including queued and in-flight work.
    #[must_use]
    pub fn observe(
        &mut self,
        package_id: PackageId,
        fingerprint: BuildFingerprint,
    ) -> DevelopmentObservation {
        if self.stopped {
            let matches_active = self
                .packages
                .get(&package_id)
                .is_some_and(|state| state.active == Some(fingerprint));
            let matches_observed = self
                .packages
                .get(&package_id)
                .is_some_and(|state| state.observed == Some(fingerprint));
            if matches_active || matches_observed {
                return DevelopmentObservation {
                    identity: None,
                    change_detected: false,
                    stable: false,
                    matched_active: matches_active,
                    matched_terminal: !matches_active && matches_observed,
                    stable_scans: 0,
                    terminals: Vec::new(),
                };
            }
            let (identity, terminals) = self.begin(package_id, fingerprint);
            return DevelopmentObservation {
                identity: Some(identity),
                change_detected: true,
                stable: false,
                matched_active: false,
                matched_terminal: false,
                stable_scans: 0,
                terminals,
            };
        }
        let same_observation = self
            .packages
            .get(&package_id)
            .is_some_and(|state| state.observed == Some(fingerprint));
        if same_observation {
            return self.observe_repeated(&package_id, fingerprint);
        }

        let terminals = self.terminalize_live(
            &package_id,
            DevelopmentTerminalKind::SupersededBeforeCompile,
            DevelopmentTerminalKind::SupersededInFlight,
        );
        let state = self.packages.entry(package_id.clone()).or_default();
        state.desired = Some(fingerprint);
        state.observed = Some(fingerprint);
        state.stable_scans = 1;
        state.stable_emitted_generation = None;

        if state.active == Some(fingerprint) {
            state.live = None;
            return DevelopmentObservation {
                identity: None,
                change_detected: false,
                stable: false,
                matched_active: true,
                matched_terminal: false,
                stable_scans: state.stable_scans,
                terminals,
            };
        }
        if state.terminal_fingerprint == Some(fingerprint) {
            state.live = None;
            return DevelopmentObservation {
                identity: None,
                change_detected: false,
                stable: false,
                matched_active: false,
                matched_terminal: true,
                stable_scans: state.stable_scans,
                terminals,
            };
        }

        state.latest_generation = state.latest_generation.saturating_add(1).max(1);
        let identity = CandidateIdentity::new(package_id, state.latest_generation, fingerprint)
            .expect("DevelopmentCoordinator generations are non-zero");
        state.live = Some(identity.clone());
        let stable = self.config.stable_scan_count <= 1;
        if stable {
            state.stable_emitted_generation = Some(identity.generation);
        }
        DevelopmentObservation {
            identity: Some(identity),
            change_detected: true,
            stable,
            matched_active: false,
            matched_terminal: false,
            stable_scans: state.stable_scans,
            terminals,
        }
    }

    fn observe_repeated(
        &mut self,
        package_id: &PackageId,
        fingerprint: BuildFingerprint,
    ) -> DevelopmentObservation {
        let state = self
            .packages
            .get_mut(package_id)
            .expect("a repeated observation has Package state");
        if state.active == Some(fingerprint) && state.live.is_none() {
            return DevelopmentObservation {
                identity: None,
                change_detected: false,
                stable: false,
                matched_active: true,
                matched_terminal: false,
                stable_scans: state.stable_scans,
                terminals: Vec::new(),
            };
        }
        if state.terminal_fingerprint == Some(fingerprint) && state.live.is_none() {
            return DevelopmentObservation {
                identity: None,
                change_detected: false,
                stable: false,
                matched_active: false,
                matched_terminal: true,
                stable_scans: state.stable_scans,
                terminals: Vec::new(),
            };
        }
        state.stable_scans = state.stable_scans.saturating_add(1);
        let identity = state.live.clone();
        let stable = identity.as_ref().is_some_and(|identity| {
            state.stable_scans >= self.config.stable_scan_count
                && state.stable_emitted_generation != Some(identity.generation)
        });
        if stable {
            state.stable_emitted_generation = identity.as_ref().map(|identity| identity.generation);
        }
        DevelopmentObservation {
            identity,
            change_detected: false,
            stable,
            matched_active: false,
            matched_terminal: false,
            stable_scans: state.stable_scans,
            terminals: Vec::new(),
        }
    }

    #[must_use]
    pub fn enqueue(&mut self, identity: CandidateIdentity) -> DevelopmentQueueOutcome {
        if self.stopped || !self.is_live(&identity) {
            return DevelopmentQueueOutcome::Stale(identity);
        }
        if self.pending.get(&identity.package_id) == Some(&identity)
            || self.in_flight.get(&identity.package_id) == Some(&identity)
        {
            return DevelopmentQueueOutcome::AlreadyQueued;
        }
        if self.pending.len() >= self.config.queue_capacity {
            self.backpressure_count = self.backpressure_count.saturating_add(1);
            return DevelopmentQueueOutcome::Backpressured(identity);
        }
        self.pending_order.push_back(identity.package_id.clone());
        self.pending.insert(identity.package_id.clone(), identity);
        DevelopmentQueueOutcome::Accepted
    }

    #[must_use]
    pub fn start_next(&mut self) -> Option<CandidateIdentity> {
        while let Some(package_id) = self.pending_order.pop_front() {
            let Some(identity) = self.pending.remove(&package_id) else {
                continue;
            };
            if !self.is_live(&identity) {
                continue;
            }
            self.in_flight.insert(package_id, identity.clone());
            return Some(identity);
        }
        None
    }

    /// Marks a particular admitted Candidate as in flight.
    ///
    /// This variant lets an external Worker preserve its own scheduling loop while the shared
    /// coordinator remains authoritative for queue and terminal accounting.
    #[must_use]
    pub fn start(&mut self, identity: &CandidateIdentity) -> DevelopmentStartOutcome {
        if self.in_flight.get(&identity.package_id) == Some(identity) {
            return DevelopmentStartOutcome::AlreadyInFlight;
        }
        if !self.is_live(identity) {
            return DevelopmentStartOutcome::Stale;
        }
        if self.pending.get(&identity.package_id) != Some(identity) {
            return DevelopmentStartOutcome::NotQueued;
        }
        self.pending.remove(&identity.package_id);
        self.pending_order
            .retain(|package_id| package_id != &identity.package_id);
        self.in_flight
            .insert(identity.package_id.clone(), identity.clone());
        DevelopmentStartOutcome::Started
    }

    /// Completes compiler work and performs the mandatory fresh `BuildInput` comparison before a
    /// successful result may be retained.
    #[must_use]
    pub fn complete(
        &mut self,
        identity: CandidateIdentity,
        original: &ResolvedBuildInput,
        current: &ResolvedBuildInput,
        completion: DevelopmentCompletionKind,
        retained_host_contract_changed: bool,
    ) -> DevelopmentCompletionOutcome {
        if let Some(kind) = self.terminal_kind(&identity) {
            self.note_duplicate(&identity.package_id);
            return DevelopmentCompletionOutcome::AlreadyTerminal { identity, kind };
        }
        if !self.is_live(&identity) {
            return DevelopmentCompletionOutcome::Stale(identity);
        }

        let desired_generation = self
            .packages
            .get(&identity.package_id)
            .map_or(0, |state| state.latest_generation);
        let freshness = identity.compare_freshness(desired_generation, original, current);
        let requested_terminal = match completion {
            DevelopmentCompletionKind::Compiled => DevelopmentTerminalKind::Compiled,
            DevelopmentCompletionKind::CompileFailed => DevelopmentTerminalKind::CompileFailed,
            DevelopmentCompletionKind::VerifyFailed => DevelopmentTerminalKind::VerifyFailed,
        };
        let terminal_kind = if retained_host_contract_changed {
            DevelopmentTerminalKind::RejectedHostContractChange
        } else {
            match freshness {
                FreshnessOutcome::Fresh => requested_terminal,
                FreshnessOutcome::HostRebuildRequired { .. } => {
                    DevelopmentTerminalKind::RejectedHostContractChange
                }
                FreshnessOutcome::Superseded(_) => DevelopmentTerminalKind::SupersededInFlight,
            }
        };
        let terminal = self
            .record_terminal(identity, terminal_kind)
            .expect("a live Generation has no prior terminal");
        if terminal_kind == requested_terminal {
            DevelopmentCompletionOutcome::Accepted(terminal)
        } else {
            DevelopmentCompletionOutcome::Rejected {
                terminal,
                freshness,
            }
        }
    }

    #[must_use]
    pub fn invalidate(
        &mut self,
        package_id: &PackageId,
        reason: DevelopmentInvalidation,
    ) -> Vec<DevelopmentTerminal> {
        let kind = match reason {
            DevelopmentInvalidation::Transient => DevelopmentTerminalKind::CancelledByInvalidation,
            DevelopmentInvalidation::SourceRemoval => {
                DevelopmentTerminalKind::CancelledBySourceRemoval
            }
        };
        let terminals = self.terminalize_live(package_id, kind, kind);
        if let Some(state) = self.packages.get_mut(package_id) {
            state.desired = None;
            state.observed = None;
            state.stable_scans = 0;
            state.stable_emitted_generation = None;
            state.terminal_fingerprint = None;
            if reason == DevelopmentInvalidation::SourceRemoval {
                state.active = None;
            }
        }
        terminals
    }

    #[must_use]
    pub fn shutdown(&mut self) -> Vec<DevelopmentTerminal> {
        self.stopped = true;
        let packages = self.packages.keys().cloned().collect::<Vec<_>>();
        let mut terminals = Vec::new();
        for package_id in packages {
            terminals.extend(self.terminalize_live(
                &package_id,
                DevelopmentTerminalKind::CancelledByShutdown,
                DevelopmentTerminalKind::CancelledByShutdown,
            ));
        }
        terminals
    }

    #[must_use]
    pub fn inspection(&self) -> DevelopmentCoordinatorInspection {
        let packages = self
            .packages
            .iter()
            .map(|(package_id, state)| {
                (
                    package_id.clone(),
                    DevelopmentPackageInspection {
                        latest_generation: state.latest_generation,
                        terminal_generations: state.terminal_generation_count,
                        duplicate_terminals: state.duplicate_terminals,
                        generations_without_terminal: state
                            .latest_generation
                            .saturating_sub(state.terminal_generation_count),
                        desired_build_fingerprint: state.desired,
                        active_build_fingerprint: state.active,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        DevelopmentCoordinatorInspection {
            queued_candidates: self.pending.len(),
            in_flight_candidates: self.in_flight.len(),
            backpressure_count: self.backpressure_count,
            created_generations: packages
                .values()
                .map(|package| package.latest_generation)
                .sum(),
            terminal_generations: packages
                .values()
                .map(|package| package.terminal_generations)
                .sum(),
            duplicate_terminals: packages
                .values()
                .map(|package| package.duplicate_terminals)
                .sum(),
            generations_without_terminal: packages
                .values()
                .map(|package| package.generations_without_terminal)
                .sum(),
            packages,
        }
    }

    #[must_use]
    pub fn terminal(&self, identity: &CandidateIdentity) -> Option<DevelopmentTerminal> {
        self.terminal_kind(identity)
            .map(|kind| DevelopmentTerminal {
                identity: identity.clone(),
                kind,
            })
    }

    fn is_live(&self, identity: &CandidateIdentity) -> bool {
        self.packages
            .get(&identity.package_id)
            .and_then(|state| state.live.as_ref())
            == Some(identity)
    }

    fn terminal_kind(&self, identity: &CandidateIdentity) -> Option<DevelopmentTerminalKind> {
        self.packages
            .get(&identity.package_id)?
            .terminal_generations
            .get(&identity.generation)
            .copied()
    }

    fn note_duplicate(&mut self, package_id: &PackageId) {
        if let Some(state) = self.packages.get_mut(package_id) {
            state.duplicate_terminals = state.duplicate_terminals.saturating_add(1);
        }
    }

    fn terminalize_live(
        &mut self,
        package_id: &PackageId,
        queued_kind: DevelopmentTerminalKind,
        in_flight_kind: DevelopmentTerminalKind,
    ) -> Vec<DevelopmentTerminal> {
        let Some(identity) = self
            .packages
            .get(package_id)
            .and_then(|state| state.live.clone())
        else {
            return Vec::new();
        };
        let kind = if self.in_flight.get(package_id) == Some(&identity) {
            in_flight_kind
        } else {
            queued_kind
        };
        self.record_terminal(identity, kind).into_iter().collect()
    }

    fn record_terminal(
        &mut self,
        identity: CandidateIdentity,
        kind: DevelopmentTerminalKind,
    ) -> Option<DevelopmentTerminal> {
        let retain_terminal_generations = self.config.retain_terminal_generations;
        let state = self.packages.get_mut(&identity.package_id)?;
        if identity.generation <= state.evicted_through_generation {
            return None;
        }
        if state
            .terminal_generations
            .contains_key(&identity.generation)
        {
            state.duplicate_terminals = state.duplicate_terminals.saturating_add(1);
            return None;
        }
        state.terminal_generations.insert(identity.generation, kind);
        state.terminal_generation_count = state.terminal_generation_count.saturating_add(1);
        while state.terminal_generations.len() > retain_terminal_generations {
            let Some(oldest) = state
                .terminal_generations
                .first_key_value()
                .map(|(generation, _)| *generation)
            else {
                break;
            };
            state.terminal_generations.remove(&oldest);
            state.evicted_through_generation = state.evicted_through_generation.max(oldest);
        }
        if matches!(
            kind,
            DevelopmentTerminalKind::Compiled
                | DevelopmentTerminalKind::CompileFailed
                | DevelopmentTerminalKind::VerifyFailed
                | DevelopmentTerminalKind::RejectedHostContractChange
        ) {
            state.terminal_fingerprint = Some(identity.build_fingerprint);
        }
        if state.live.as_ref() == Some(&identity) {
            state.live = None;
        }
        if self.pending.get(&identity.package_id) == Some(&identity) {
            self.pending.remove(&identity.package_id);
            self.pending_order
                .retain(|package_id| package_id != &identity.package_id);
        }
        if self.in_flight.get(&identity.package_id) == Some(&identity) {
            self.in_flight.remove(&identity.package_id);
        }
        Some(DevelopmentTerminal { identity, kind })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package() -> PackageId {
        PackageId::new("tests.development").expect("test PackageId")
    }

    fn fingerprint(byte: u8) -> BuildFingerprint {
        BuildFingerprint::from_bytes([byte; 32])
    }

    #[test]
    fn shutdown_terminalizes_existing_and_future_generations() {
        let mut coordinator = DevelopmentCoordinator::new(DevelopmentCoordinatorConfig {
            stable_scan_count: 1,
            queue_capacity: 1,
            retain_terminal_generations: 2,
        });
        let first = coordinator.observe(package(), fingerprint(1));
        let first_identity = first.identity.expect("first Generation");
        assert_eq!(
            coordinator.enqueue(first_identity.clone()),
            DevelopmentQueueOutcome::Accepted
        );
        let shutdown = coordinator.shutdown();
        assert_eq!(shutdown.len(), 1);
        assert_eq!(
            shutdown[0].kind,
            DevelopmentTerminalKind::CancelledByShutdown
        );

        let (second_identity, second_terminals) = coordinator.begin(package(), fingerprint(2));
        assert_eq!(second_terminals.len(), 1);
        assert_eq!(
            second_terminals[0].kind,
            DevelopmentTerminalKind::CancelledByShutdown
        );
        assert!(matches!(
            coordinator.enqueue(second_identity),
            DevelopmentQueueOutcome::Stale(_)
        ));

        let third = coordinator.observe(package(), fingerprint(3));
        assert_eq!(third.terminals.len(), 1);
        assert_eq!(
            third.terminals[0].kind,
            DevelopmentTerminalKind::CancelledByShutdown
        );
        let third_identity = third
            .identity
            .expect("post-shutdown observation Generation");
        assert!(matches!(
            coordinator.enqueue(third_identity),
            DevelopmentQueueOutcome::Stale(_)
        ));
        assert!(
            coordinator.terminal(&first_identity).is_none(),
            "the bounded ledger evicts its oldest retained terminal"
        );
        assert!(matches!(
            coordinator.enqueue(first_identity),
            DevelopmentQueueOutcome::Stale(_)
        ));

        let inspection = coordinator.inspection();
        assert_eq!(inspection.created_generations, 3);
        assert_eq!(inspection.terminal_generations, 3);
        assert_eq!(inspection.generations_without_terminal, 0);
        assert_eq!(inspection.duplicate_terminals, 0);
        assert_eq!(inspection.queued_candidates, 0);
        assert_eq!(inspection.in_flight_candidates, 0);
    }
}
