#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChangeScanConfig {
    pub interval_ticks: u64,
}

impl Default for ChangeScanConfig {
    fn default() -> Self {
        Self { interval_ticks: 60 }
    }
}
