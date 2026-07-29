use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageStatus {
    Discovered,
    Locked,
    Disabled,
    Enabling,
    Enabled,
    Reloading,
    Disabling,
    Faulted,
    Incompatible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackageLifecycle {
    status: PackageStatus,
}

impl PackageLifecycle {
    #[must_use]
    pub const fn discovered() -> Self {
        Self {
            status: PackageStatus::Discovered,
        }
    }

    #[must_use]
    pub const fn status(self) -> PackageStatus {
        self.status
    }

    pub fn transition(&mut self, next: PackageStatus) -> Result<(), LifecycleError> {
        let allowed = matches!(
            (self.status, next),
            (
                PackageStatus::Discovered,
                PackageStatus::Locked
                    | PackageStatus::Disabled
                    | PackageStatus::Incompatible
                    | PackageStatus::Enabling
            ) | (
                PackageStatus::Locked,
                PackageStatus::Disabled | PackageStatus::Enabling
            ) | (
                PackageStatus::Disabled,
                PackageStatus::Locked | PackageStatus::Enabling | PackageStatus::Incompatible
            ) | (
                PackageStatus::Enabling,
                PackageStatus::Enabled
                    | PackageStatus::Faulted
                    | PackageStatus::Incompatible
                    | PackageStatus::Locked
            ) | (
                PackageStatus::Enabled,
                PackageStatus::Reloading
                    | PackageStatus::Disabling
                    | PackageStatus::Faulted
                    | PackageStatus::Locked
            ) | (
                PackageStatus::Reloading,
                PackageStatus::Enabled | PackageStatus::Faulted
            ) | (
                PackageStatus::Disabling,
                PackageStatus::Disabled | PackageStatus::Faulted
            ) | (
                PackageStatus::Faulted,
                PackageStatus::Enabling | PackageStatus::Disabled | PackageStatus::Locked
            ) | (
                PackageStatus::Incompatible,
                PackageStatus::Disabled | PackageStatus::Locked
            )
        );
        if !allowed {
            return Err(LifecycleError {
                from: self.status,
                to: next,
            });
        }
        self.status = next;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleError {
    pub from: PackageStatus,
    pub to: PackageStatus,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid package transition {:?} -> {:?}",
            self.from, self.to
        )
    }
}

impl std::error::Error for LifecycleError {}
