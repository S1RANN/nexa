use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RuntimeFailurePoint {
    TaskSlot,
    ScopeSlot,
    SchedulerSlot,
    FrameSlot,
    HeapSlot,
    RequestSlot,
    CompletionSlot,
    ReleaseSlot,
    SnapshotSlot,
    MigrationObjectSlot,
    MigrationFieldSlot,
    MigrationForwardingSlot,
    ActivationTrap,
    CleanupTrap,
    HostReturnObjectReservation,
    HostReturnCollectionReservation,
    HostReturnStringReservation,
    HostReturnStructWrite,
    HostReturnCollectionWrite,
    HostReturnCommit,
}

impl RuntimeFailurePoint {
    pub const REALM_PRODUCTION: [Self; 14] = [
        Self::TaskSlot,
        Self::ScopeSlot,
        Self::SchedulerSlot,
        Self::FrameSlot,
        Self::HeapSlot,
        Self::RequestSlot,
        Self::CompletionSlot,
        Self::ReleaseSlot,
        Self::SnapshotSlot,
        Self::MigrationObjectSlot,
        Self::MigrationFieldSlot,
        Self::MigrationForwardingSlot,
        Self::ActivationTrap,
        Self::CleanupTrap,
    ];
    pub const HOST_RETURN: [Self; 6] = [
        Self::HostReturnObjectReservation,
        Self::HostReturnCollectionReservation,
        Self::HostReturnStringReservation,
        Self::HostReturnStructWrite,
        Self::HostReturnCollectionWrite,
        Self::HostReturnCommit,
    ];
    pub const ALL: [Self; 20] = [
        Self::TaskSlot,
        Self::ScopeSlot,
        Self::SchedulerSlot,
        Self::FrameSlot,
        Self::HeapSlot,
        Self::RequestSlot,
        Self::CompletionSlot,
        Self::ReleaseSlot,
        Self::SnapshotSlot,
        Self::MigrationObjectSlot,
        Self::MigrationFieldSlot,
        Self::MigrationForwardingSlot,
        Self::ActivationTrap,
        Self::CleanupTrap,
        Self::HostReturnObjectReservation,
        Self::HostReturnCollectionReservation,
        Self::HostReturnStringReservation,
        Self::HostReturnStructWrite,
        Self::HostReturnCollectionWrite,
        Self::HostReturnCommit,
    ];

    const fn index(self) -> usize {
        self as usize
    }

    const fn operation(self) -> &'static str {
        match self {
            Self::TaskSlot => "task admission",
            Self::ScopeSlot => "scope admission",
            Self::SchedulerSlot => "scheduler reservation",
            Self::FrameSlot => "continuation reservation",
            Self::HeapSlot => "heap reservation",
            Self::RequestSlot => "request reservation",
            Self::CompletionSlot => "completion reservation",
            Self::ReleaseSlot => "release reservation",
            Self::SnapshotSlot => "snapshot reservation",
            Self::MigrationObjectSlot => "migration object reservation",
            Self::MigrationFieldSlot => "migration field reservation",
            Self::MigrationForwardingSlot => "migration forwarding reservation",
            Self::ActivationTrap => "reload activation",
            Self::CleanupTrap => "task cleanup",
            Self::HostReturnObjectReservation => "host return object reservation",
            Self::HostReturnCollectionReservation => "host return collection reservation",
            Self::HostReturnStringReservation => "host return string reservation",
            Self::HostReturnStructWrite => "host return struct write",
            Self::HostReturnCollectionWrite => "host return collection write",
            Self::HostReturnCommit => "host return commit",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RuntimeFailureMode {
    #[default]
    Off,
    Once,
    At(u64),
    Always,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FailurePointStats {
    pub attempted: u64,
    pub injected: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureProbeState {
    Armed,
    Consumed,
    ScenarioNotReached,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureObservation {
    pub point: RuntimeFailurePoint,
    pub state: FailureProbeState,
    pub operation: &'static str,
    pub task_handle: Option<crate::TaskHandle>,
    pub request_handle: Option<crate::HostRequestHandle>,
    pub result: &'static str,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RuntimeFailureRule {
    mode: RuntimeFailureMode,
    stats: FailurePointStats,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeFailureConfigError {
    ZeroOccurrence,
}

/// A cloneable failure-control plane shared by every production subsystem in one Realm.
#[derive(Clone, Debug)]
pub struct RuntimeFailureInjector {
    rules: Arc<Mutex<[RuntimeFailureRule; RuntimeFailurePoint::ALL.len()]>>,
    observations: Arc<Mutex<VecDeque<FailureObservation>>>,
}

impl Default for RuntimeFailureInjector {
    fn default() -> Self {
        Self {
            rules: Arc::new(Mutex::new(
                [RuntimeFailureRule::default(); RuntimeFailurePoint::ALL.len()],
            )),
            observations: Arc::new(Mutex::new(VecDeque::with_capacity(64))),
        }
    }
}

impl PartialEq for RuntimeFailureInjector {
    fn eq(&self, other: &Self) -> bool {
        self.rules_snapshot() == other.rules_snapshot()
    }
}

impl Eq for RuntimeFailureInjector {}

#[derive(Clone, Debug)]
pub struct FailureProbe {
    injector: RuntimeFailureInjector,
    point: RuntimeFailurePoint,
    injected_before: u64,
}

impl FailureProbe {
    #[must_use]
    pub fn was_consumed(&self) -> bool {
        self.injector.stats(self.point).injected > self.injected_before
    }

    #[must_use]
    pub fn state(&self) -> FailureProbeState {
        if self.was_consumed() {
            FailureProbeState::Consumed
        } else {
            FailureProbeState::ScenarioNotReached
        }
    }

    pub fn require_consumed(&self) -> Result<(), &'static str> {
        if self.was_consumed() {
            Ok(())
        } else {
            Err("SCENARIO_NOT_REACHED")
        }
    }
}

impl RuntimeFailureInjector {
    #[must_use = "retain the probe and require that the injected scenario was consumed"]
    pub fn arm_once(&self, point: RuntimeFailurePoint) -> FailureProbe {
        let injected_before = self.stats(point).injected;
        self.configure(point, RuntimeFailureMode::Once);
        self.record(FailureObservation {
            point,
            state: FailureProbeState::Armed,
            operation: point.operation(),
            task_handle: None,
            request_handle: None,
            result: "ARMED",
        });
        FailureProbe {
            injector: self.clone(),
            point,
            injected_before,
        }
    }

    pub fn arm_at(
        &self,
        point: RuntimeFailurePoint,
        occurrence: u64,
    ) -> Result<(), RuntimeFailureConfigError> {
        if occurrence == 0 {
            return Err(RuntimeFailureConfigError::ZeroOccurrence);
        }
        self.configure(point, RuntimeFailureMode::At(occurrence));
        Ok(())
    }

    pub fn arm_always(&self, point: RuntimeFailurePoint) {
        self.configure(point, RuntimeFailureMode::Always);
    }

    pub fn disarm(&self, point: RuntimeFailurePoint) {
        self.rules()[point.index()].mode = RuntimeFailureMode::Off;
    }

    pub fn clear(&self) {
        self.rules().fill(RuntimeFailureRule::default());
        self.observation_log().clear();
    }

    #[must_use]
    pub fn mode(&self, point: RuntimeFailurePoint) -> RuntimeFailureMode {
        self.rules()[point.index()].mode
    }

    #[must_use]
    pub fn stats(&self, point: RuntimeFailurePoint) -> FailurePointStats {
        self.rules()[point.index()].stats
    }

    #[must_use]
    pub fn all_stats(&self) -> [(RuntimeFailurePoint, FailurePointStats); 21] {
        std::array::from_fn(|index| {
            let point = RuntimeFailurePoint::ALL[index];
            (point, self.stats(point))
        })
    }

    #[must_use]
    pub fn observations(&self) -> Vec<FailureObservation> {
        self.observation_log().iter().cloned().collect()
    }

    #[must_use]
    pub fn trigger(&self, point: RuntimeFailurePoint) -> bool {
        self.trigger_with_context(point, None, None)
    }

    #[must_use]
    pub(crate) fn trigger_with_context(
        &self,
        point: RuntimeFailurePoint,
        task_handle: Option<crate::TaskHandle>,
        request_handle: Option<crate::HostRequestHandle>,
    ) -> bool {
        let mut rules = self.rules();
        let rule = &mut rules[point.index()];
        rule.stats.attempted = rule.stats.attempted.saturating_add(1);
        let injected = match rule.mode {
            RuntimeFailureMode::Off => false,
            RuntimeFailureMode::Once => {
                rule.mode = RuntimeFailureMode::Off;
                true
            }
            RuntimeFailureMode::At(occurrence) => {
                if rule.stats.attempted == occurrence {
                    rule.mode = RuntimeFailureMode::Off;
                    true
                } else {
                    false
                }
            }
            RuntimeFailureMode::Always => true,
        };
        if injected {
            rule.stats.injected = rule.stats.injected.saturating_add(1);
        }
        let armed = rule.mode != RuntimeFailureMode::Off || injected;
        drop(rules);
        if armed {
            self.record(FailureObservation {
                point,
                state: if injected {
                    FailureProbeState::Consumed
                } else {
                    FailureProbeState::Armed
                },
                operation: point.operation(),
                task_handle,
                request_handle,
                result: if injected {
                    "INJECTED"
                } else {
                    "NOT_REACHED_YET"
                },
            });
        }
        injected
    }

    fn configure(&self, point: RuntimeFailurePoint, mode: RuntimeFailureMode) {
        self.rules()[point.index()] = RuntimeFailureRule {
            mode,
            stats: FailurePointStats::default(),
        };
    }

    fn rules(
        &self,
    ) -> std::sync::MutexGuard<'_, [RuntimeFailureRule; RuntimeFailurePoint::ALL.len()]> {
        self.rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn rules_snapshot(&self) -> [RuntimeFailureRule; RuntimeFailurePoint::ALL.len()] {
        *self.rules()
    }

    fn observation_log(&self) -> std::sync::MutexGuard<'_, VecDeque<FailureObservation>> {
        self.observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn record(&self, observation: FailureObservation) {
        const CAPACITY: usize = 64;
        let mut observations = self.observation_log();
        if observations.len() == CAPACITY {
            observations.pop_front();
        }
        observations.push_back(observation);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FailurePointStats, RuntimeFailureConfigError, RuntimeFailureInjector, RuntimeFailureMode,
        RuntimeFailurePoint,
    };

    #[test]
    fn every_failure_point_supports_once_at_always_disarm_and_clear() {
        let classified = RuntimeFailurePoint::REALM_PRODUCTION
            .into_iter()
            .chain(RuntimeFailurePoint::HOST_RETURN)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(classified, RuntimeFailurePoint::ALL.into_iter().collect());
        let injector = RuntimeFailureInjector::default();
        for point in RuntimeFailurePoint::ALL {
            let probe = injector.arm_once(point);
            assert!(injector.trigger(point));
            assert!(!injector.trigger(point));
            probe.require_consumed().unwrap();
            assert_eq!(
                injector.stats(point),
                FailurePointStats {
                    attempted: 2,
                    injected: 1,
                }
            );
            assert_eq!(injector.mode(point), RuntimeFailureMode::Off);

            injector.arm_at(point, 3).unwrap();
            assert!(!injector.trigger(point));
            assert!(!injector.trigger(point));
            assert!(injector.trigger(point));
            assert!(!injector.trigger(point));
            assert_eq!(
                injector.stats(point),
                FailurePointStats {
                    attempted: 4,
                    injected: 1,
                }
            );

            injector.arm_always(point);
            assert!(injector.trigger(point));
            assert!(injector.trigger(point));
            injector.disarm(point);
            assert!(!injector.trigger(point));
            assert_eq!(
                injector.stats(point),
                FailurePointStats {
                    attempted: 3,
                    injected: 2,
                }
            );
        }
        assert_eq!(
            injector.arm_at(RuntimeFailurePoint::TaskSlot, 0),
            Err(RuntimeFailureConfigError::ZeroOccurrence)
        );
        injector.arm_always(RuntimeFailurePoint::TaskSlot);
        injector.arm_always(RuntimeFailurePoint::HeapSlot);
        injector.clear();
        assert!(RuntimeFailurePoint::ALL.into_iter().all(|point| {
            injector.mode(point) == RuntimeFailureMode::Off
                && injector.stats(point) == FailurePointStats::default()
        }));
    }

    #[test]
    fn clones_share_modes_and_statistics() {
        let injector = RuntimeFailureInjector::default();
        let subsystem = injector.clone();
        let probe = injector.arm_once(RuntimeFailurePoint::HeapSlot);
        assert!(subsystem.trigger(RuntimeFailurePoint::HeapSlot));
        probe.require_consumed().unwrap();
        assert_eq!(
            injector.stats(RuntimeFailurePoint::HeapSlot),
            FailurePointStats {
                attempted: 1,
                injected: 1,
            }
        );
    }
}
