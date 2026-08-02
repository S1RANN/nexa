//! Bounded runtime profiler (M5 WP14/WP15).
//!
//! Disabled-state contract: one relaxed atomic load per interpreter poll and
//! one cached-branch check per instruction; no allocation, no global mutex,
//! no hash map. Enabled-state contract: fixed-capacity thread-local storage
//! (open-addressed arrays) whose overflow increments dropped counters
//! instead of growing.
//!
//! Allocation sites are keyed by function index and program counter within
//! the executing module; package, symbol, and source-span attribution is
//! resolved at report time through the module's cold metadata, never on the
//! hot path.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) const OPCODE_TABLE_SIZE: usize = 107;
pub const PROFILER_FUNCTION_CAPACITY: usize = 256;
pub const PROFILER_SITE_CAPACITY: usize = 512;

static PROFILER_ENABLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static PROFILE: RefCell<Option<Box<ProfileStorage>>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FunctionCell {
    function: u32,
    instructions: u64,
    occupied: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SiteCell {
    function: u32,
    pc: u32,
    opcode: u16,
    type_id: u64,
    count: u64,
    occupied: bool,
}

struct ProfileStorage {
    opcodes: [u64; OPCODE_TABLE_SIZE],
    functions: [FunctionCell; PROFILER_FUNCTION_CAPACITY],
    sites: [SiteCell; PROFILER_SITE_CAPACITY],
    host_calls: u64,
    dropped_functions: u64,
    dropped_sites: u64,
}

impl Default for ProfileStorage {
    fn default() -> Self {
        Self {
            opcodes: [0; OPCODE_TABLE_SIZE],
            functions: [FunctionCell::default(); PROFILER_FUNCTION_CAPACITY],
            sites: [SiteCell::default(); PROFILER_SITE_CAPACITY],
            host_calls: 0,
            dropped_functions: 0,
            dropped_sites: 0,
        }
    }
}

/// One executed-opcode row of a profiler report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpcodeProfile {
    pub opcode: &'static str,
    pub executions: u64,
}

/// Per-function instruction counts keyed by module function index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionProfile {
    pub function: u32,
    pub instructions: u64,
}

/// WP14 allocation site: function + pc + allocating opcode + type identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocationSiteProfile {
    pub function: u32,
    pub pc: u32,
    pub opcode: u16,
    pub type_id: u64,
    pub count: u64,
}

/// Bounded profiler snapshot; report construction may allocate, hot-path
/// recording never does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfilerReport {
    pub opcodes: Vec<OpcodeProfile>,
    pub functions: Vec<FunctionProfile>,
    pub allocation_sites: Vec<AllocationSiteProfile>,
    pub host_calls: u64,
    pub dropped_functions: u64,
    pub dropped_sites: u64,
}

/// Enables instruction profiling on this process.
pub fn enable() {
    PROFILER_ENABLED.store(true, Ordering::Relaxed);
}

/// Disables instruction profiling; existing storage is kept until taken.
pub fn disable() {
    PROFILER_ENABLED.store(false, Ordering::Relaxed);
}

#[must_use]
pub(crate) fn enabled() -> bool {
    PROFILER_ENABLED.load(Ordering::Relaxed)
}

/// G4 byte accounting: bytes held by this thread's profile storage, zero
/// until the lazily boxed table exists. The table is fixed-size, so this
/// is a constant once profiling has recorded on the thread.
#[must_use]
pub(crate) fn thread_storage_bytes() -> u64 {
    PROFILE.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0, |_| size_of::<ProfileStorage>() as u64)
    })
}

/// Drains this thread's profile into a report, resetting the storage.
#[must_use]
pub fn take_thread_report() -> Option<ProfilerReport> {
    PROFILE.with(|cell| {
        let storage = cell.borrow_mut().take()?;
        let mut report = ProfilerReport {
            host_calls: storage.host_calls,
            dropped_functions: storage.dropped_functions,
            dropped_sites: storage.dropped_sites,
            ..ProfilerReport::default()
        };
        for (index, executions) in storage.opcodes.iter().enumerate() {
            if *executions > 0 {
                report.opcodes.push(OpcodeProfile {
                    opcode: crate::interpreter::OPCODE_NAMES[index],
                    executions: *executions,
                });
            }
        }
        for cell in storage.functions.iter().filter(|cell| cell.occupied) {
            report.functions.push(FunctionProfile {
                function: cell.function,
                instructions: cell.instructions,
            });
        }
        for cell in storage.sites.iter().filter(|cell| cell.occupied) {
            report.allocation_sites.push(AllocationSiteProfile {
                function: cell.function,
                pc: cell.pc,
                opcode: cell.opcode,
                type_id: cell.type_id,
                count: cell.count,
            });
        }
        report.functions.sort_by_key(|entry| entry.function);
        report
            .allocation_sites
            .sort_by_key(|entry| (entry.function, entry.pc));
        Some(report)
    })
}

pub(crate) fn record_instruction(
    opcode: usize,
    function: u32,
    allocation: Option<(u32, u64)>,
    is_host_call: bool,
) {
    PROFILE.with(|cell| {
        let mut storage = cell.borrow_mut();
        let storage = storage.get_or_insert_with(Box::default);
        storage.opcodes[opcode.min(OPCODE_TABLE_SIZE - 1)] =
            storage.opcodes[opcode.min(OPCODE_TABLE_SIZE - 1)].saturating_add(1);
        if is_host_call {
            storage.host_calls = storage.host_calls.saturating_add(1);
        }
        record_function(storage, function);
        if let Some((pc, type_id)) = allocation {
            record_site(storage, function, pc, opcode, type_id);
        }
    });
}

fn record_function(storage: &mut ProfileStorage, function: u32) {
    let capacity = storage.functions.len();
    let start = function as usize % capacity;
    for probe in 0..capacity {
        let cell = &mut storage.functions[(start + probe) % capacity];
        if cell.occupied && cell.function == function {
            cell.instructions = cell.instructions.saturating_add(1);
            return;
        }
        if !cell.occupied {
            *cell = FunctionCell {
                function,
                instructions: 1,
                occupied: true,
            };
            return;
        }
    }
    storage.dropped_functions = storage.dropped_functions.saturating_add(1);
}

fn record_site(storage: &mut ProfileStorage, function: u32, pc: u32, opcode: usize, type_id: u64) {
    let capacity = storage.sites.len();
    let key = (u64::from(function) << 32) | u64::from(pc);
    let start = usize::try_from(key % capacity as u64).unwrap_or(0);
    let opcode = u16::try_from(opcode).unwrap_or(u16::MAX);
    for probe in 0..capacity {
        let cell = &mut storage.sites[(start + probe) % capacity];
        if cell.occupied && cell.function == function && cell.pc == pc {
            cell.count = cell.count.saturating_add(1);
            return;
        }
        if !cell.occupied {
            *cell = SiteCell {
                function,
                pc,
                opcode,
                type_id,
                count: 1,
                occupied: true,
            };
            return;
        }
    }
    storage.dropped_sites = storage.dropped_sites.saturating_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    // The enabled flag is process-global, so all scenarios run inside one
    // test to avoid libtest thread interleaving.
    #[test]
    fn profiler_contract_disabled_noop_bounded_storage_and_drop_counters() {
        disable();
        let _ = take_thread_report();
        record_probe_when_disabled();
        assert!(take_thread_report().is_none());

        enable();
        record_instruction(3, 7, None, false);
        record_instruction(3, 7, None, false);
        record_instruction(18, 9, None, true);
        record_instruction(60, 7, Some((4, 0xfeed)), false);
        disable();

        let report = take_thread_report().expect("profile storage exists");
        assert_eq!(report.host_calls, 1);
        let add = report
            .opcodes
            .iter()
            .find(|entry| entry.opcode == "Add")
            .expect("Add profile");
        assert_eq!(add.executions, 2);
        assert_eq!(report.functions.len(), 2);
        assert_eq!(report.allocation_sites.len(), 1);
        assert_eq!(report.allocation_sites[0].function, 7);
        assert_eq!(report.allocation_sites[0].pc, 4);
        assert_eq!(report.allocation_sites[0].type_id, 0xfeed);
        assert!(take_thread_report().is_none());

        enable();
        let site_capacity = u32::try_from(PROFILER_SITE_CAPACITY).expect("capacity fits u32");
        for index in 0..(site_capacity + 8) {
            record_instruction(60, 1, Some((index, 1)), false);
        }
        disable();
        let report = take_thread_report().expect("profile storage exists");
        assert_eq!(report.allocation_sites.len(), PROFILER_SITE_CAPACITY);
        assert_eq!(report.dropped_sites, 8);
    }

    fn record_probe_when_disabled() {
        // The interpreter only calls record_instruction behind the cached
        // enabled flag; this mirrors that contract.
        if enabled() {
            record_instruction(0, 0, None, false);
        }
    }
}
