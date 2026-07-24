use nexa_core::{MachineKind, TraceRecord};

/// Realm-wide monotonic trace sink. All VM-thread machines append to this single total order.
#[derive(Clone, Debug)]
pub struct RuntimeTrace {
    next_sequence: u64,
    records: Vec<TraceRecord>,
    enabled: bool,
}

impl Default for RuntimeTrace {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeTrace {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next_sequence: 0,
            records: Vec::new(),
            enabled: true,
        }
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            next_sequence: 0,
            records: Vec::with_capacity(capacity),
            enabled: true,
        }
    }

    pub fn record(&mut self, mut record: TraceRecord) {
        if !self.enabled {
            return;
        }
        record.sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("trace sequence exhausted u64");
        self.records.push(record);
    }

    #[must_use]
    pub fn records(&self) -> &[TraceRecord] {
        &self.records
    }

    pub fn drain(&mut self) -> impl Iterator<Item = TraceRecord> + '_ {
        self.records.drain(..)
    }

    #[must_use]
    pub fn count_for(&self, kind: MachineKind) -> usize {
        self.records
            .iter()
            .filter(|record| record.machine_kind == kind)
            .count()
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}
