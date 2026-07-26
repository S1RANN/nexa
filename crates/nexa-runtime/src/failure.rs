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
    ReloadCompletionSlot,
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
    pub const ALL: [Self; 21] = [
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
        Self::ReloadCompletionSlot,
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
}

impl Default for RuntimeFailureInjector {
    fn default() -> Self {
        Self {
            rules: Arc::new(Mutex::new(
                [RuntimeFailureRule::default(); RuntimeFailurePoint::ALL.len()],
            )),
        }
    }
}

impl PartialEq for RuntimeFailureInjector {
    fn eq(&self, other: &Self) -> bool {
        self.rules_snapshot() == other.rules_snapshot()
    }
}

impl Eq for RuntimeFailureInjector {}

impl RuntimeFailureInjector {
    pub fn arm_once(&self, point: RuntimeFailurePoint) {
        self.configure(point, RuntimeFailureMode::Once);
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
    pub fn trigger(&self, point: RuntimeFailurePoint) -> bool {
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
}

#[cfg(test)]
mod tests {
    use super::{
        FailurePointStats, RuntimeFailureConfigError, RuntimeFailureInjector, RuntimeFailureMode,
        RuntimeFailurePoint,
    };

    #[test]
    fn every_failure_point_supports_once_at_always_disarm_and_clear() {
        let injector = RuntimeFailureInjector::default();
        for point in RuntimeFailurePoint::ALL {
            injector.arm_once(point);
            assert!(injector.trigger(point));
            assert!(!injector.trigger(point));
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
        injector.arm_once(RuntimeFailurePoint::HeapSlot);
        assert!(subsystem.trigger(RuntimeFailurePoint::HeapSlot));
        assert_eq!(
            injector.stats(RuntimeFailurePoint::HeapSlot),
            FailurePointStats {
                attempted: 1,
                injected: 1,
            }
        );
    }
}
