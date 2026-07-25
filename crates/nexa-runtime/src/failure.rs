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
}

impl RuntimeFailurePoint {
    pub const ALL: [Self; 15] = [
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
    Nth(u64),
    Always,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RuntimeFailureRule {
    mode: RuntimeFailureMode,
    hits: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeFailureConfigError {
    ZeroOccurrence,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeFailureInjector {
    rules: [RuntimeFailureRule; RuntimeFailurePoint::ALL.len()],
}

impl RuntimeFailureInjector {
    pub fn arm_once(&mut self, point: RuntimeFailurePoint) {
        self.configure(point, RuntimeFailureMode::Once);
    }

    pub fn arm_nth(
        &mut self,
        point: RuntimeFailurePoint,
        occurrence: u64,
    ) -> Result<(), RuntimeFailureConfigError> {
        if occurrence == 0 {
            return Err(RuntimeFailureConfigError::ZeroOccurrence);
        }
        self.configure(point, RuntimeFailureMode::Nth(occurrence));
        Ok(())
    }

    pub fn arm_always(&mut self, point: RuntimeFailurePoint) {
        self.configure(point, RuntimeFailureMode::Always);
    }

    pub fn disable(&mut self, point: RuntimeFailurePoint) {
        self.configure(point, RuntimeFailureMode::Off);
    }

    pub fn disable_all(&mut self) {
        self.rules.fill(RuntimeFailureRule::default());
    }

    #[must_use]
    pub fn mode(&self, point: RuntimeFailurePoint) -> RuntimeFailureMode {
        self.rules[point.index()].mode
    }

    #[must_use]
    pub fn hits(&self, point: RuntimeFailurePoint) -> u64 {
        self.rules[point.index()].hits
    }

    pub fn trigger(&mut self, point: RuntimeFailurePoint) -> bool {
        let rule = &mut self.rules[point.index()];
        match rule.mode {
            RuntimeFailureMode::Off => false,
            RuntimeFailureMode::Once => {
                rule.hits = rule.hits.saturating_add(1);
                rule.mode = RuntimeFailureMode::Off;
                true
            }
            RuntimeFailureMode::Nth(occurrence) => {
                rule.hits = rule.hits.saturating_add(1);
                if rule.hits == occurrence {
                    rule.mode = RuntimeFailureMode::Off;
                    true
                } else {
                    false
                }
            }
            RuntimeFailureMode::Always => {
                rule.hits = rule.hits.saturating_add(1);
                true
            }
        }
    }

    fn configure(&mut self, point: RuntimeFailurePoint, mode: RuntimeFailureMode) {
        self.rules[point.index()] = RuntimeFailureRule { mode, hits: 0 };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RuntimeFailureConfigError, RuntimeFailureInjector, RuntimeFailureMode, RuntimeFailurePoint,
    };

    #[test]
    fn every_failure_point_supports_once_nth_always_and_disable() {
        let mut injector = RuntimeFailureInjector::default();
        for point in RuntimeFailurePoint::ALL {
            injector.arm_once(point);
            assert!(injector.trigger(point));
            assert!(!injector.trigger(point));
            assert_eq!(injector.hits(point), 1);
            assert_eq!(injector.mode(point), RuntimeFailureMode::Off);

            injector.arm_nth(point, 3).unwrap();
            assert!(!injector.trigger(point));
            assert!(!injector.trigger(point));
            assert!(injector.trigger(point));
            assert!(!injector.trigger(point));
            assert_eq!(injector.hits(point), 3);

            injector.arm_always(point);
            assert!(injector.trigger(point));
            assert!(injector.trigger(point));
            assert_eq!(injector.hits(point), 2);
            injector.disable(point);
            assert!(!injector.trigger(point));
            assert_eq!(injector.hits(point), 0);
        }
        assert_eq!(
            injector.arm_nth(RuntimeFailurePoint::TaskSlot, 0),
            Err(RuntimeFailureConfigError::ZeroOccurrence)
        );
        injector.arm_always(RuntimeFailurePoint::TaskSlot);
        injector.arm_always(RuntimeFailurePoint::HeapSlot);
        injector.disable_all();
        assert!(
            RuntimeFailurePoint::ALL
                .into_iter()
                .all(|point| injector.mode(point) == RuntimeFailureMode::Off)
        );
    }
}
