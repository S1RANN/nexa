/// A point-in-time, authoritative count of all runtime-owned resource classes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeResourceLedger {
    pub tasks: u64,
    pub scopes: u64,
    pub continuations: u64,
    pub scheduler_tokens: u64,
    pub requests: u64,
    pub completion_reservations: u64,
    pub tokens: u64,
    pub snapshots: u64,
    pub release_reservations: u64,
    pub queued_releases: u64,
    pub heap_objects: u64,
    pub state_objects: u64,
    pub retired_epochs: u64,
}

impl RuntimeResourceLedger {
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.tasks == 0
            && self.scopes == 0
            && self.continuations == 0
            && self.scheduler_tokens == 0
            && self.requests == 0
            && self.completion_reservations == 0
            && self.tokens == 0
            && self.snapshots == 0
            && self.release_reservations == 0
            && self.queued_releases == 0
            && self.heap_objects == 0
            && self.state_objects == 0
            && self.retired_epochs == 0
    }
}

pub(crate) fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
