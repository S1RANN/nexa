#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecutionMetrics {
    pub interpreter_ops: u64,
    pub task_runtime_ops: u64,
    pub host_ops: u64,
    pub gc_cycles: u64,
    pub gc_reclaimed: u64,
    pub user_ops: u64,
}

impl ExecutionMetrics {
    pub fn record_gc(&mut self, reclaimed: usize) {
        self.gc_cycles = self.gc_cycles.saturating_add(1);
        self.gc_reclaimed = self
            .gc_reclaimed
            .saturating_add(u64::try_from(reclaimed).unwrap_or(u64::MAX));
    }
}
