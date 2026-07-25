use std::collections::VecDeque;

use nexa_core::{MachineKind, TraceRecord};

/// Realm-wide monotonic trace sink. All VM-thread machines append to this single total order.
#[derive(Clone, Debug)]
pub struct RuntimeTrace {
    next_sequence: u64,
    records: VecDeque<TraceRecord>,
    max_records: usize,
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
            records: VecDeque::new(),
            max_records: usize::MAX,
            enabled: true,
        }
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            next_sequence: 0,
            records: VecDeque::with_capacity(capacity),
            max_records: capacity,
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
        if self.max_records == 0 {
            return;
        }
        if self.records.len() == self.max_records {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    pub fn record_with(&mut self, create: impl FnOnce() -> TraceRecord) {
        if self.enabled {
            self.record(create());
        }
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn reserved_capacity(&self) -> usize {
        self.records.capacity()
    }

    #[must_use]
    pub fn records(&self) -> TraceRecords<'_> {
        let (first, second) = self.records.as_slices();
        TraceRecords { first, second }
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

#[derive(Clone, Copy, Debug)]
pub struct TraceRecords<'a> {
    first: &'a [TraceRecord],
    second: &'a [TraceRecord],
}

impl<'a> TraceRecords<'a> {
    #[must_use]
    pub const fn len(self) -> usize {
        self.first.len() + self.second.len()
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.first.is_empty() && self.second.is_empty()
    }

    #[must_use]
    pub fn first(self) -> Option<&'a TraceRecord> {
        self.first.first().or_else(|| self.second.first())
    }

    #[must_use]
    pub fn last(self) -> Option<&'a TraceRecord> {
        self.second.last().or_else(|| self.first.last())
    }

    #[must_use]
    pub fn iter(self) -> impl DoubleEndedIterator<Item = &'a TraceRecord> + 'a {
        self.first.iter().chain(self.second.iter())
    }
}
