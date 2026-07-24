use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocationSnapshot {
    pub admission: u64,
    pub first_slice: u64,
    pub promotion: u64,
    pub resume: u64,
    pub terminal_cleanup: u64,
}

static ADMISSION: AtomicU64 = AtomicU64::new(0);
static FIRST_SLICE: AtomicU64 = AtomicU64::new(0);
static PROMOTION: AtomicU64 = AtomicU64::new(0);
static RESUME: AtomicU64 = AtomicU64::new(0);
static TERMINAL_CLEANUP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
pub(crate) enum AllocationPhase {
    Admission,
    FirstSlice,
    Promotion,
    Resume,
    TerminalCleanup,
}

pub(crate) fn record(phase: AllocationPhase, count: u64) {
    if !cfg!(feature = "allocation-counting") {
        return;
    }
    let counter = match phase {
        AllocationPhase::Admission => &ADMISSION,
        AllocationPhase::FirstSlice => &FIRST_SLICE,
        AllocationPhase::Promotion => &PROMOTION,
        AllocationPhase::Resume => &RESUME,
        AllocationPhase::TerminalCleanup => &TERMINAL_CLEANUP,
    };
    counter.fetch_add(count, Ordering::Relaxed);
}

#[must_use]
pub fn allocation_snapshot() -> AllocationSnapshot {
    AllocationSnapshot {
        admission: ADMISSION.load(Ordering::Relaxed),
        first_slice: FIRST_SLICE.load(Ordering::Relaxed),
        promotion: PROMOTION.load(Ordering::Relaxed),
        resume: RESUME.load(Ordering::Relaxed),
        terminal_cleanup: TERMINAL_CLEANUP.load(Ordering::Relaxed),
    }
}

impl std::ops::Sub for AllocationSnapshot {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            admission: self.admission.saturating_sub(rhs.admission),
            first_slice: self.first_slice.saturating_sub(rhs.first_slice),
            promotion: self.promotion.saturating_sub(rhs.promotion),
            resume: self.resume.saturating_sub(rhs.resume),
            terminal_cleanup: self.terminal_cleanup.saturating_sub(rhs.terminal_cleanup),
        }
    }
}
