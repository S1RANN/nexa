//! Bounded runtime profiler (M5 WP14-WP16).
//!
//! Disabled-state contract: one relaxed atomic load per interpreter poll and
//! one cached branch per instruction; no allocation, global lock, or hash
//! table. Enabled-state recording uses fixed-capacity thread-local open
//! addressing. Overflow increments explicit dropped counters instead of
//! growing storage.
//!
//! Package builds attach cold semantic metadata to [`nexa_verifier::VerifiedModule`].
//! The interpreter records only dense module/function slots. Report creation
//! resolves those slots to Package ID, module, stable function ID, source span,
//! allocation kind, and type ID after execution.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nexa_bytecode::HostCallMode;
use nexa_core::{SourceSpan, StableId};
use nexa_verifier::{ModuleProfileMetadata, VerifiedModule};

pub const PROFILER_SCHEMA_VERSION: u32 = 1;
pub(crate) const OPCODE_TABLE_SIZE: usize = 109;
pub const PROFILER_MODULE_CAPACITY: usize = 32;
pub const PROFILER_FUNCTION_CAPACITY: usize = 256;
pub const PROFILER_SITE_CAPACITY: usize = 512;
pub const PROFILER_HOST_CALL_CAPACITY: usize = 128;

static PROFILER_ENABLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static PROFILE: RefCell<Option<Box<ProfileStorage>>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum AllocationKind {
    #[default]
    Object,
    String,
    Class,
    ArrayStorage,
    BufferStorage,
    MapSlots,
    StructMaterialization,
    EnumMaterialization,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionIdentity {
    pub package_id: String,
    pub module: String,
    pub stable_id: StableId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionProfile {
    pub identity: FunctionIdentity,
    pub instructions: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpcodeProfile {
    pub opcode: &'static str,
    pub executions: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationSiteId {
    pub package_id: String,
    pub module: String,
    pub function_stable_id: StableId,
    pub source_span: Option<SourceSpan>,
    pub kind: AllocationKind,
    pub type_id: StableId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationProfile {
    pub site: AllocationSiteId,
    pub count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostCallProfile {
    pub stable_id: StableId,
    pub mode: HostCallMode,
    pub calls: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcProfile {
    pub full_collections: u64,
    pub incremental_steps: u64,
    pub completed_cycles: u64,
    pub roots_seeded: u64,
    pub objects_marked: u64,
    pub slots_swept: u64,
    pub objects_reclaimed: u64,
    pub bytes_reclaimed: u64,
    pub barrier_shades: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TaskProfile {
    pub polls: u64,
    pub completed: u64,
    pub yielded_fuel: u64,
    pub yielded_explicit: u64,
    pub waiting_host: u64,
    pub cancelled: u64,
    pub trapped: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DroppedProfile {
    pub modules: u64,
    pub functions: u64,
    pub allocations: u64,
    pub host_calls: u64,
}

/// Complete bounded profiler snapshot. Constructing this reader-facing report
/// may allocate; enabled hot-path recording never grows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PerformanceProfile {
    pub schema: u32,
    pub opcodes: Vec<OpcodeProfile>,
    pub functions: Vec<FunctionProfile>,
    pub allocations: Vec<AllocationProfile>,
    pub gc: GcProfile,
    pub host_calls: Vec<HostCallProfile>,
    pub tasks: TaskProfile,
    pub dropped: DroppedProfile,
}

#[derive(Clone, Default)]
struct ModuleCell {
    key: [u8; 32],
    metadata: Option<Arc<ModuleProfileMetadata>>,
    occupied: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FunctionCell {
    module: u16,
    function: u32,
    instructions: u64,
    occupied: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SiteCell {
    module: u16,
    function: u32,
    pc: u32,
    source_span: Option<SourceSpan>,
    kind: AllocationKind,
    type_id: StableId,
    count: u64,
    occupied: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HostCell {
    stable_id: StableId,
    mode: u8,
    calls: u64,
    occupied: bool,
}

struct ProfileStorage {
    modules: [ModuleCell; PROFILER_MODULE_CAPACITY],
    opcodes: [u64; OPCODE_TABLE_SIZE],
    functions: [FunctionCell; PROFILER_FUNCTION_CAPACITY],
    sites: Box<[SiteCell]>,
    host_calls: [HostCell; PROFILER_HOST_CALL_CAPACITY],
    gc: GcProfile,
    tasks: TaskProfile,
    dropped: DroppedProfile,
}

impl Default for ProfileStorage {
    fn default() -> Self {
        Self {
            modules: std::array::from_fn(|_| ModuleCell::default()),
            opcodes: [0; OPCODE_TABLE_SIZE],
            functions: [FunctionCell::default(); PROFILER_FUNCTION_CAPACITY],
            sites: vec![SiteCell::default(); PROFILER_SITE_CAPACITY].into_boxed_slice(),
            host_calls: [HostCell::default(); PROFILER_HOST_CALL_CAPACITY],
            gc: GcProfile::default(),
            tasks: TaskProfile::default(),
            dropped: DroppedProfile::default(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ProfileModuleSlot(u16);

#[derive(Clone, Copy)]
pub(crate) struct AllocationEvent {
    pub pc: u32,
    pub source_span: Option<SourceSpan>,
    pub kind: AllocationKind,
    pub type_id: StableId,
}

#[derive(Clone, Copy)]
pub(crate) enum TaskPollProfileOutcome {
    Completed,
    FuelYielded,
    ExplicitYielded,
    WaitingHost,
    Cancelled,
    Trapped,
}

/// Enables profiling for subsequent polls in this process.
pub fn enable() {
    PROFILER_ENABLED.store(true, Ordering::Relaxed);
}

/// Disables profiling. Existing bounded thread-local storage remains until
/// [`take_thread_report`] drains it.
pub fn disable() {
    PROFILER_ENABLED.store(false, Ordering::Relaxed);
}

#[must_use]
pub(crate) fn enabled() -> bool {
    PROFILER_ENABLED.load(Ordering::Relaxed)
}

/// Registers one immutable module once per profiled interpreter poll.
///
/// Arc cloning and table probing happen outside the instruction loop. A
/// module-table overflow drops the whole poll rather than paying repeated
/// failed lookups for every instruction.
pub(crate) fn begin_module(module: &VerifiedModule) -> Option<ProfileModuleSlot> {
    register_module(
        module.profile_fingerprint(),
        module.profile_metadata().cloned(),
    )
}

fn register_module(
    key: [u8; 32],
    metadata: Option<Arc<ModuleProfileMetadata>>,
) -> Option<ProfileModuleSlot> {
    PROFILE.with(|cell| {
        let mut storage = cell.borrow_mut();
        let storage = storage.get_or_insert_with(Box::default);
        let capacity = storage.modules.len();
        let hash = u64::from_le_bytes(key[..8].try_into().expect("fixed fingerprint prefix"));
        let start = usize::try_from(hash % capacity as u64).unwrap_or(0);
        for probe in 0..capacity {
            let index = (start + probe) % capacity;
            let module = &mut storage.modules[index];
            if module.occupied && module.key == key {
                return Some(ProfileModuleSlot(
                    u16::try_from(index).expect("profile module capacity fits u16"),
                ));
            }
            if !module.occupied {
                *module = ModuleCell {
                    key,
                    metadata,
                    occupied: true,
                };
                return Some(ProfileModuleSlot(
                    u16::try_from(index).expect("profile module capacity fits u16"),
                ));
            }
        }
        storage.dropped.modules = storage.dropped.modules.saturating_add(1);
        None
    })
}

/// Bytes held by this thread's fixed profiler table, excluding shared module
/// metadata already owned by verified Package artifacts.
#[must_use]
pub(crate) fn thread_storage_bytes() -> u64 {
    PROFILE.with(|cell| {
        cell.borrow().as_ref().map_or(0, |storage| {
            let inline = size_of::<ProfileStorage>() as u64;
            let sites = storage.sites.len().saturating_mul(size_of::<SiteCell>()) as u64;
            inline.saturating_add(sites)
        })
    })
}

/// Drains this thread's storage into a stable reader-facing report.
#[must_use]
pub fn take_thread_report() -> Option<PerformanceProfile> {
    PROFILE.with(|cell| {
        let storage = cell.borrow_mut().take()?;
        let mut report = PerformanceProfile {
            schema: PROFILER_SCHEMA_VERSION,
            gc: storage.gc,
            tasks: storage.tasks,
            dropped: storage.dropped,
            ..PerformanceProfile::default()
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
            let module = &storage.modules[usize::from(cell.module)];
            report.functions.push(FunctionProfile {
                identity: function_identity(module, cell.function),
                instructions: cell.instructions,
            });
        }
        for cell in storage.sites.iter().filter(|cell| cell.occupied) {
            let module = &storage.modules[usize::from(cell.module)];
            let identity = function_identity(module, cell.function);
            report.allocations.push(AllocationProfile {
                site: AllocationSiteId {
                    package_id: identity.package_id,
                    module: identity.module,
                    function_stable_id: identity.stable_id,
                    source_span: cell.source_span,
                    kind: cell.kind,
                    type_id: cell.type_id,
                },
                count: cell.count,
            });
        }
        for cell in storage.host_calls.iter().filter(|cell| cell.occupied) {
            report.host_calls.push(HostCallProfile {
                stable_id: cell.stable_id,
                mode: if cell.mode == 0 {
                    HostCallMode::Immediate
                } else {
                    HostCallMode::Async
                },
                calls: cell.calls,
            });
        }
        report.functions.sort_by(|left, right| {
            (
                &left.identity.package_id,
                &left.identity.module,
                left.identity.stable_id,
            )
                .cmp(&(
                    &right.identity.package_id,
                    &right.identity.module,
                    right.identity.stable_id,
                ))
        });
        report.allocations.sort_by(|left, right| {
            (
                &left.site.package_id,
                &left.site.module,
                left.site.function_stable_id,
                source_span_key(left.site.source_span),
                left.site.kind as u8,
                left.site.type_id,
            )
                .cmp(&(
                    &right.site.package_id,
                    &right.site.module,
                    right.site.function_stable_id,
                    source_span_key(right.site.source_span),
                    right.site.kind as u8,
                    right.site.type_id,
                ))
        });
        report
            .host_calls
            .sort_by_key(|entry| (entry.stable_id, entry.mode as u8));
        Some(report)
    })
}

const fn source_span_key(span: Option<SourceSpan>) -> (u32, u32, u32, u8) {
    match span {
        Some(span) => (span.file.0, span.start, span.end, 1),
        None => (0, 0, 0, 0),
    }
}

fn function_identity(module: &ModuleCell, function: u32) -> FunctionIdentity {
    if let Some(metadata) = module
        .metadata
        .as_deref()
        .and_then(|metadata| metadata.function(function))
    {
        return FunctionIdentity {
            package_id: metadata.package_id.clone(),
            module: metadata.module.clone(),
            stable_id: metadata.stable_id,
        };
    }
    let mut fingerprint = String::with_capacity(64);
    for byte in module.key {
        write!(fingerprint, "{byte:02x}").expect("writing to String cannot fail");
    }
    let function_text = function.to_string();
    FunctionIdentity {
        package_id: "bytecode".to_owned(),
        module: format!("module:{fingerprint}"),
        stable_id: StableId::from_parts(&["bytecode", &fingerprint, &function_text]),
    }
}

pub(crate) fn record_instruction(
    module: ProfileModuleSlot,
    opcode: usize,
    function: u32,
    allocation: Option<AllocationEvent>,
    host_call: Option<(StableId, HostCallMode)>,
) {
    PROFILE.with(|cell| {
        let mut storage = cell.borrow_mut();
        let Some(storage) = storage.as_deref_mut() else {
            return;
        };
        let opcode = opcode.min(OPCODE_TABLE_SIZE - 1);
        storage.opcodes[opcode] = storage.opcodes[opcode].saturating_add(1);
        record_function(storage, module, function);
        if let Some(allocation) = allocation {
            record_site(storage, module, function, allocation);
        }
        if let Some((stable_id, mode)) = host_call {
            record_host_call(storage, stable_id, mode);
        }
    });
}

fn record_function(storage: &mut ProfileStorage, module: ProfileModuleSlot, function: u32) {
    let capacity = storage.functions.len();
    let key = (u64::from(module.0) << 32) | u64::from(function);
    let start = usize::try_from(key % capacity as u64).unwrap_or(0);
    for probe in 0..capacity {
        let cell = &mut storage.functions[(start + probe) % capacity];
        if cell.occupied && cell.module == module.0 && cell.function == function {
            cell.instructions = cell.instructions.saturating_add(1);
            return;
        }
        if !cell.occupied {
            *cell = FunctionCell {
                module: module.0,
                function,
                instructions: 1,
                occupied: true,
            };
            return;
        }
    }
    storage.dropped.functions = storage.dropped.functions.saturating_add(1);
}

fn record_site(
    storage: &mut ProfileStorage,
    module: ProfileModuleSlot,
    function: u32,
    event: AllocationEvent,
) {
    let capacity = storage.sites.len();
    let key = (u64::from(module.0) << 48)
        ^ (u64::from(function) << 16)
        ^ u64::from(event.pc)
        ^ event.type_id.0.rotate_left(17)
        ^ u64::from(event.kind as u8);
    let start = usize::try_from(key % capacity as u64).unwrap_or(0);
    for probe in 0..capacity {
        let cell = &mut storage.sites[(start + probe) % capacity];
        if cell.occupied
            && cell.module == module.0
            && cell.function == function
            && cell.pc == event.pc
            && cell.kind == event.kind
            && cell.type_id == event.type_id
        {
            cell.count = cell.count.saturating_add(1);
            return;
        }
        if !cell.occupied {
            *cell = SiteCell {
                module: module.0,
                function,
                pc: event.pc,
                source_span: event.source_span,
                kind: event.kind,
                type_id: event.type_id,
                count: 1,
                occupied: true,
            };
            return;
        }
    }
    storage.dropped.allocations = storage.dropped.allocations.saturating_add(1);
}

fn record_host_call(storage: &mut ProfileStorage, stable_id: StableId, mode: HostCallMode) {
    let capacity = storage.host_calls.len();
    let mode_byte = u8::from(mode == HostCallMode::Async);
    let start =
        usize::try_from((stable_id.0 ^ u64::from(mode_byte)) % capacity as u64).unwrap_or(0);
    for probe in 0..capacity {
        let cell = &mut storage.host_calls[(start + probe) % capacity];
        if cell.occupied && cell.stable_id == stable_id && cell.mode == mode_byte {
            cell.calls = cell.calls.saturating_add(1);
            return;
        }
        if !cell.occupied {
            *cell = HostCell {
                stable_id,
                mode: mode_byte,
                calls: 1,
                occupied: true,
            };
            return;
        }
    }
    storage.dropped.host_calls = storage.dropped.host_calls.saturating_add(1);
}

pub(crate) fn record_full_gc(marked: usize, reclaimed: usize, bytes_reclaimed: u64) {
    if !enabled() {
        return;
    }
    PROFILE.with(|cell| {
        let mut storage = cell.borrow_mut();
        let storage = storage.get_or_insert_with(Box::default);
        storage.gc.full_collections = storage.gc.full_collections.saturating_add(1);
        storage.gc.completed_cycles = storage.gc.completed_cycles.saturating_add(1);
        storage.gc.objects_marked = storage
            .gc
            .objects_marked
            .saturating_add(u64::try_from(marked).unwrap_or(u64::MAX));
        storage.gc.objects_reclaimed = storage
            .gc
            .objects_reclaimed
            .saturating_add(u64::try_from(reclaimed).unwrap_or(u64::MAX));
        storage.gc.bytes_reclaimed = storage.gc.bytes_reclaimed.saturating_add(bytes_reclaimed);
    });
}

pub(crate) fn record_incremental_gc(
    roots_seeded: usize,
    objects_marked: usize,
    slots_swept: usize,
    barrier_shades: u64,
    bytes_reclaimed: u64,
    completed_reclaimed: Option<usize>,
) {
    if !enabled() {
        return;
    }
    PROFILE.with(|cell| {
        let mut storage = cell.borrow_mut();
        let storage = storage.get_or_insert_with(Box::default);
        storage.gc.incremental_steps = storage.gc.incremental_steps.saturating_add(1);
        storage.gc.roots_seeded = storage
            .gc
            .roots_seeded
            .saturating_add(u64::try_from(roots_seeded).unwrap_or(u64::MAX));
        storage.gc.objects_marked = storage
            .gc
            .objects_marked
            .saturating_add(u64::try_from(objects_marked).unwrap_or(u64::MAX));
        storage.gc.slots_swept = storage
            .gc
            .slots_swept
            .saturating_add(u64::try_from(slots_swept).unwrap_or(u64::MAX));
        storage.gc.barrier_shades = storage.gc.barrier_shades.saturating_add(barrier_shades);
        storage.gc.bytes_reclaimed = storage.gc.bytes_reclaimed.saturating_add(bytes_reclaimed);
        if let Some(reclaimed) = completed_reclaimed {
            storage.gc.completed_cycles = storage.gc.completed_cycles.saturating_add(1);
            storage.gc.objects_reclaimed = storage
                .gc
                .objects_reclaimed
                .saturating_add(u64::try_from(reclaimed).unwrap_or(u64::MAX));
        }
    });
}

pub(crate) fn record_task_poll(outcome: TaskPollProfileOutcome) {
    if !enabled() {
        return;
    }
    PROFILE.with(|cell| {
        let mut storage = cell.borrow_mut();
        let storage = storage.get_or_insert_with(Box::default);
        storage.tasks.polls = storage.tasks.polls.saturating_add(1);
        let counter = match outcome {
            TaskPollProfileOutcome::Completed => &mut storage.tasks.completed,
            TaskPollProfileOutcome::FuelYielded => &mut storage.tasks.yielded_fuel,
            TaskPollProfileOutcome::ExplicitYielded => &mut storage.tasks.yielded_explicit,
            TaskPollProfileOutcome::WaitingHost => &mut storage.tasks.waiting_host,
            TaskPollProfileOutcome::Cancelled => &mut storage.tasks.cancelled,
            TaskPollProfileOutcome::Trapped => &mut storage.tasks.trapped,
        };
        *counter = counter.saturating_add(1);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexa_core::FileId;
    use nexa_verifier::FunctionProfileMetadata;

    // The enabled flag is process-global, so all scenarios run in one test
    // to avoid libtest interleaving.
    #[test]
    fn profiler_contract_is_stable_complete_bounded_and_disabled_noop() {
        disable();
        let _ = take_thread_report();
        assert!(take_thread_report().is_none());

        let metadata = Arc::new(ModuleProfileMetadata::new(vec![
            FunctionProfileMetadata {
                function: 7,
                package_id: "game.snake".into(),
                module: "game.tick".into(),
                stable_id: StableId(0x777),
                definition_span: SourceSpan::new(FileId(4), 10, 20),
            },
            FunctionProfileMetadata {
                function: 9,
                package_id: "game.snake".into(),
                module: "game.tick".into(),
                stable_id: StableId(0x999),
                definition_span: SourceSpan::new(FileId(4), 30, 40),
            },
        ]));
        enable();
        let module = register_module([3; 32], Some(metadata)).expect("module slot");
        record_instruction(module, 3, 7, None, None);
        record_instruction(module, 3, 7, None, None);
        record_instruction(
            module,
            18,
            9,
            None,
            Some((StableId(0xabc), HostCallMode::Immediate)),
        );
        record_instruction(
            module,
            60,
            7,
            Some(AllocationEvent {
                pc: 4,
                source_span: Some(SourceSpan::new(FileId(4), 13, 18)),
                kind: AllocationKind::StructMaterialization,
                type_id: StableId(0xfeed),
            }),
            None,
        );
        record_full_gc(3, 2, 64);
        record_incremental_gc(1, 2, 3, 4, 32, Some(1));
        record_task_poll(TaskPollProfileOutcome::WaitingHost);
        record_task_poll(TaskPollProfileOutcome::Completed);
        disable();

        let report = take_thread_report().expect("profile storage exists");
        assert_eq!(report.schema, PROFILER_SCHEMA_VERSION);
        assert_eq!(report.host_calls[0].stable_id, StableId(0xabc));
        assert_eq!(report.host_calls[0].calls, 1);
        let add = report
            .opcodes
            .iter()
            .find(|entry| entry.opcode == "Add")
            .expect("Add profile");
        assert_eq!(add.executions, 2);
        assert_eq!(report.functions.len(), 2);
        assert_eq!(report.allocations.len(), 1);
        let site = &report.allocations[0].site;
        assert_eq!(site.package_id, "game.snake");
        assert_eq!(site.module, "game.tick");
        assert_eq!(site.function_stable_id, StableId(0x777));
        assert_eq!(site.source_span, Some(SourceSpan::new(FileId(4), 13, 18)));
        assert_eq!(site.kind, AllocationKind::StructMaterialization);
        assert_eq!(site.type_id, StableId(0xfeed));
        assert_eq!(report.gc.full_collections, 1);
        assert_eq!(report.gc.incremental_steps, 1);
        assert_eq!(report.gc.completed_cycles, 2);
        assert_eq!(report.gc.objects_reclaimed, 3);
        assert_eq!(report.tasks.polls, 2);
        assert_eq!(report.tasks.waiting_host, 1);
        assert_eq!(report.tasks.completed, 1);
        assert_eq!(report.dropped, DroppedProfile::default());
        assert!(take_thread_report().is_none());

        enable();
        let module = register_module([5; 32], None).expect("module slot");
        let site_capacity = u32::try_from(PROFILER_SITE_CAPACITY).expect("capacity fits u32");
        for index in 0..(site_capacity + 8) {
            record_instruction(
                module,
                60,
                1,
                Some(AllocationEvent {
                    pc: index,
                    source_span: None,
                    kind: AllocationKind::Object,
                    type_id: StableId(1),
                }),
                None,
            );
        }
        disable();
        let report = take_thread_report().expect("profile storage exists");
        assert_eq!(report.allocations.len(), PROFILER_SITE_CAPACITY);
        assert_eq!(report.dropped.allocations, 8);
    }
}
