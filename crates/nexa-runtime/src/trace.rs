use nexa_core::{MachineKind, TraceRecord};

/// Monotonic in-memory trace sink used by the first runtime milestone and differential tests.
#[derive(Clone, Debug, Default)]
pub struct TraceRecorder {
    next_sequence: u64,
    records: Vec<TraceRecord>,
}

impl TraceRecorder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next_sequence: 0,
            records: Vec::new(),
        }
    }

    pub fn record(&mut self, mut record: TraceRecord) {
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
}
