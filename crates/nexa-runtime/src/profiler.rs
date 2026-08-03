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

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::rc::Rc;
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
    static PROFILE: RefCell<Option<Rc<ProfileStorage>>> = const { RefCell::new(None) };
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

#[derive(Debug, Default)]
struct FunctionCell {
    module: Cell<u16>,
    function: Cell<u32>,
    instructions: Cell<u64>,
    occupied: Cell<bool>,
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
    modules: RefCell<Box<[ModuleCell]>>,
    opcodes: [Cell<u64>; OPCODE_TABLE_SIZE],
    functions: Box<[FunctionCell]>,
    sites: RefCell<Box<[SiteCell]>>,
    host_calls: RefCell<Box<[HostCell]>>,
    gc: Cell<GcProfile>,
    tasks: Cell<TaskProfile>,
    dropped: Cell<DroppedProfile>,
}

impl Default for ProfileStorage {
    fn default() -> Self {
        Self {
            modules: RefCell::new(
                vec![ModuleCell::default(); PROFILER_MODULE_CAPACITY].into_boxed_slice(),
            ),
            opcodes: std::array::from_fn(|_| Cell::new(0)),
            functions: (0..PROFILER_FUNCTION_CAPACITY)
                .map(|_| FunctionCell::default())
                .collect(),
            sites: RefCell::new(
                vec![SiteCell::default(); PROFILER_SITE_CAPACITY].into_boxed_slice(),
            ),
            host_calls: RefCell::new(
                vec![HostCell::default(); PROFILER_HOST_CALL_CAPACITY].into_boxed_slice(),
            ),
            gc: Cell::new(GcProfile::default()),
            tasks: Cell::new(TaskProfile::default()),
            dropped: Cell::new(DroppedProfile::default()),
        }
    }
}

pub(crate) struct ProfilePoll {
    storage: Rc<ProfileStorage>,
    module: u16,
    opcodes: [u64; OPCODE_TABLE_SIZE],
    current_function: Option<u16>,
    current_function_instructions: u64,
}

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
pub(crate) fn begin_module(module: &VerifiedModule) -> Option<ProfilePoll> {
    register_module(
        module.profile_fingerprint(),
        module.profile_metadata().cloned(),
    )
}

fn register_module(
    key: [u8; 32],
    metadata: Option<Arc<ModuleProfileMetadata>>,
) -> Option<ProfilePoll> {
    PROFILE.with(|cell| {
        let storage = {
            let mut current = cell.borrow_mut();
            Rc::clone(current.get_or_insert_with(|| Rc::new(ProfileStorage::default())))
        };
        let mut modules = storage.modules.borrow_mut();
        let capacity = modules.len();
        let hash = u64::from_le_bytes(key[..8].try_into().expect("fixed fingerprint prefix"));
        let start = usize::try_from(hash % capacity as u64).unwrap_or(0);
        for probe in 0..capacity {
            let index = (start + probe) % capacity;
            let module = &mut modules[index];
            if module.occupied && module.key == key {
                drop(modules);
                return Some(ProfilePoll::new(
                    storage,
                    u16::try_from(index).expect("profile module capacity fits u16"),
                ));
            }
            if !module.occupied {
                *module = ModuleCell {
                    key,
                    metadata,
                    occupied: true,
                };
                drop(modules);
                return Some(ProfilePoll::new(
                    storage,
                    u16::try_from(index).expect("profile module capacity fits u16"),
                ));
            }
        }
        let mut dropped = storage.dropped.get();
        dropped.modules = dropped.modules.saturating_add(1);
        storage.dropped.set(dropped);
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
            let modules = storage
                .modules
                .borrow()
                .len()
                .saturating_mul(size_of::<ModuleCell>()) as u64;
            let functions = storage
                .functions
                .len()
                .saturating_mul(size_of::<FunctionCell>()) as u64;
            let sites = storage
                .sites
                .borrow()
                .len()
                .saturating_mul(size_of::<SiteCell>()) as u64;
            let host_calls = storage
                .host_calls
                .borrow()
                .len()
                .saturating_mul(size_of::<HostCell>()) as u64;
            inline
                .saturating_add(modules)
                .saturating_add(functions)
                .saturating_add(sites)
                .saturating_add(host_calls)
        })
    })
}

/// Drains this thread's storage into a stable reader-facing report.
#[must_use]
pub fn take_thread_report() -> Option<PerformanceProfile> {
    PROFILE.with(|cell| {
        let storage = cell.borrow_mut().take()?;
        let storage = match Rc::try_unwrap(storage) {
            Ok(storage) => storage,
            Err(storage) => {
                cell.borrow_mut().replace(storage);
                return None;
            }
        };
        let ProfileStorage {
            modules,
            opcodes,
            functions,
            sites,
            host_calls,
            gc,
            tasks,
            dropped,
        } = storage;
        let modules = modules.into_inner();
        let sites = sites.into_inner();
        let host_calls = host_calls.into_inner();
        let mut report = PerformanceProfile {
            schema: PROFILER_SCHEMA_VERSION,
            gc: gc.get(),
            tasks: tasks.get(),
            dropped: dropped.get(),
            ..PerformanceProfile::default()
        };
        for (index, executions) in opcodes.iter().enumerate() {
            let executions = executions.get();
            if executions > 0 {
                report.opcodes.push(OpcodeProfile {
                    opcode: crate::interpreter::OPCODE_NAMES[index],
                    executions,
                });
            }
        }
        for cell in functions.iter().filter(|cell| cell.occupied.get()) {
            let module = &modules[usize::from(cell.module.get())];
            report.functions.push(FunctionProfile {
                identity: function_identity(module, cell.function.get()),
                instructions: cell.instructions.get(),
            });
        }
        for cell in sites.iter().filter(|cell| cell.occupied) {
            let module = &modules[usize::from(cell.module)];
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
        for cell in host_calls.iter().filter(|cell| cell.occupied) {
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
        sort_report(&mut report);
        Some(report)
    })
}

fn sort_report(report: &mut PerformanceProfile) {
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

impl ProfilePoll {
    fn new(storage: Rc<ProfileStorage>, module: u16) -> Self {
        Self {
            storage,
            module,
            opcodes: [0; OPCODE_TABLE_SIZE],
            current_function: None,
            current_function_instructions: 0,
        }
    }

    /// Settles the preceding frame segment and selects the dense destination
    /// for the next one. This runs only when the interpreter changes
    /// functions; individual instructions update one poll-local scalar.
    pub(crate) fn resolve_function(&mut self, function: u32) {
        self.flush_function();
        let capacity = self.storage.functions.len();
        let key = (u64::from(self.module) << 32) | u64::from(function);
        let start = usize::try_from(key % capacity as u64).unwrap_or(0);
        for probe in 0..capacity {
            let index = (start + probe) % capacity;
            let cell = &self.storage.functions[index];
            if cell.occupied.get()
                && cell.module.get() == self.module
                && cell.function.get() == function
            {
                self.current_function =
                    Some(u16::try_from(index).expect("profile function capacity fits u16"));
                return;
            }
            if !cell.occupied.get() {
                cell.module.set(self.module);
                cell.function.set(function);
                cell.instructions.set(0);
                cell.occupied.set(true);
                self.current_function =
                    Some(u16::try_from(index).expect("profile function capacity fits u16"));
                return;
            }
        }
        let mut dropped = self.storage.dropped.get();
        dropped.functions = dropped.functions.saturating_add(1);
        self.storage.dropped.set(dropped);
    }

    #[inline]
    fn flush_function(&mut self) {
        if let Some(slot) = self.current_function.take() {
            let counter = &self.storage.functions[usize::from(slot)].instructions;
            counter.set(
                counter
                    .get()
                    .saturating_add(self.current_function_instructions),
            );
        }
        self.current_function_instructions = 0;
    }
}

impl Drop for ProfilePoll {
    fn drop(&mut self) {
        self.flush_function();
        for (destination, pending) in self.storage.opcodes.iter().zip(self.opcodes) {
            if pending != 0 {
                destination.set(destination.get().saturating_add(pending));
            }
        }
    }
}

#[inline]
pub(crate) fn record_instruction(poll: &mut ProfilePoll, opcode: usize) {
    debug_assert!(opcode < OPCODE_TABLE_SIZE);
    poll.opcodes[opcode] = poll.opcodes[opcode].saturating_add(1);
    poll.current_function_instructions = poll.current_function_instructions.saturating_add(1);
}

#[cold]
pub(crate) fn record_instruction_event(
    poll: &ProfilePoll,
    function: u32,
    allocation: Option<AllocationEvent>,
    host_call: Option<(StableId, HostCallMode)>,
) {
    if let Some(allocation) = allocation {
        record_site(poll, function, allocation);
    }
    if let Some((stable_id, mode)) = host_call {
        record_host_call(poll, stable_id, mode);
    }
}

fn record_site(poll: &ProfilePoll, function: u32, event: AllocationEvent) {
    let mut sites = poll.storage.sites.borrow_mut();
    let capacity = sites.len();
    let key = (u64::from(poll.module) << 48)
        ^ (u64::from(function) << 16)
        ^ u64::from(event.pc)
        ^ event.type_id.0.rotate_left(17)
        ^ u64::from(event.kind as u8);
    let start = usize::try_from(key % capacity as u64).unwrap_or(0);
    for probe in 0..capacity {
        let cell = &mut sites[(start + probe) % capacity];
        if cell.occupied
            && cell.module == poll.module
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
                module: poll.module,
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
    let mut dropped = poll.storage.dropped.get();
    dropped.allocations = dropped.allocations.saturating_add(1);
    poll.storage.dropped.set(dropped);
}

fn record_host_call(poll: &ProfilePoll, stable_id: StableId, mode: HostCallMode) {
    let mut host_calls = poll.storage.host_calls.borrow_mut();
    let capacity = host_calls.len();
    let mode_byte = u8::from(mode == HostCallMode::Async);
    let start =
        usize::try_from((stable_id.0 ^ u64::from(mode_byte)) % capacity as u64).unwrap_or(0);
    for probe in 0..capacity {
        let cell = &mut host_calls[(start + probe) % capacity];
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
    let mut dropped = poll.storage.dropped.get();
    dropped.host_calls = dropped.host_calls.saturating_add(1);
    poll.storage.dropped.set(dropped);
}

pub(crate) fn record_full_gc(marked: usize, reclaimed: usize, bytes_reclaimed: u64) {
    if !enabled() {
        return;
    }
    PROFILE.with(|cell| {
        let storage = {
            let mut current = cell.borrow_mut();
            Rc::clone(current.get_or_insert_with(|| Rc::new(ProfileStorage::default())))
        };
        let mut gc = storage.gc.get();
        gc.full_collections = gc.full_collections.saturating_add(1);
        gc.completed_cycles = gc.completed_cycles.saturating_add(1);
        gc.objects_marked = gc
            .objects_marked
            .saturating_add(u64::try_from(marked).unwrap_or(u64::MAX));
        gc.objects_reclaimed = gc
            .objects_reclaimed
            .saturating_add(u64::try_from(reclaimed).unwrap_or(u64::MAX));
        gc.bytes_reclaimed = gc.bytes_reclaimed.saturating_add(bytes_reclaimed);
        storage.gc.set(gc);
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
        let storage = {
            let mut current = cell.borrow_mut();
            Rc::clone(current.get_or_insert_with(|| Rc::new(ProfileStorage::default())))
        };
        let mut gc = storage.gc.get();
        gc.incremental_steps = gc.incremental_steps.saturating_add(1);
        gc.roots_seeded = gc
            .roots_seeded
            .saturating_add(u64::try_from(roots_seeded).unwrap_or(u64::MAX));
        gc.objects_marked = gc
            .objects_marked
            .saturating_add(u64::try_from(objects_marked).unwrap_or(u64::MAX));
        gc.slots_swept = gc
            .slots_swept
            .saturating_add(u64::try_from(slots_swept).unwrap_or(u64::MAX));
        gc.barrier_shades = gc.barrier_shades.saturating_add(barrier_shades);
        gc.bytes_reclaimed = gc.bytes_reclaimed.saturating_add(bytes_reclaimed);
        if let Some(reclaimed) = completed_reclaimed {
            gc.completed_cycles = gc.completed_cycles.saturating_add(1);
            gc.objects_reclaimed = gc
                .objects_reclaimed
                .saturating_add(u64::try_from(reclaimed).unwrap_or(u64::MAX));
        }
        storage.gc.set(gc);
    });
}

pub(crate) fn record_task_poll(outcome: TaskPollProfileOutcome) {
    if !enabled() {
        return;
    }
    PROFILE.with(|cell| {
        let storage = {
            let mut current = cell.borrow_mut();
            Rc::clone(current.get_or_insert_with(|| Rc::new(ProfileStorage::default())))
        };
        let mut tasks = storage.tasks.get();
        tasks.polls = tasks.polls.saturating_add(1);
        let counter = match outcome {
            TaskPollProfileOutcome::Completed => &mut tasks.completed,
            TaskPollProfileOutcome::FuelYielded => &mut tasks.yielded_fuel,
            TaskPollProfileOutcome::ExplicitYielded => &mut tasks.yielded_explicit,
            TaskPollProfileOutcome::WaitingHost => &mut tasks.waiting_host,
            TaskPollProfileOutcome::Cancelled => &mut tasks.cancelled,
            TaskPollProfileOutcome::Trapped => &mut tasks.trapped,
        };
        *counter = counter.saturating_add(1);
        storage.tasks.set(tasks);
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
        let mut module = register_module([3; 32], Some(metadata)).expect("module slot");
        module.resolve_function(7);
        record_instruction(&mut module, 3);
        record_instruction(&mut module, 3);
        module.resolve_function(9);
        record_instruction(&mut module, 18);
        record_instruction_event(
            &module,
            9,
            None,
            Some((StableId(0xabc), HostCallMode::Immediate)),
        );
        module.resolve_function(7);
        record_instruction(&mut module, 60);
        record_instruction_event(
            &module,
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
        drop(module);
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

        assert_site_overflow_is_bounded();
    }

    fn assert_site_overflow_is_bounded() {
        enable();
        let mut module = register_module([5; 32], None).expect("module slot");
        module.resolve_function(1);
        let site_capacity = u32::try_from(PROFILER_SITE_CAPACITY).expect("capacity fits u32");
        for index in 0..(site_capacity + 8) {
            record_instruction(&mut module, 60);
            record_instruction_event(
                &module,
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
        drop(module);
        disable();
        let report = take_thread_report().expect("profile storage exists");
        assert_eq!(report.allocations.len(), PROFILER_SITE_CAPACITY);
        assert_eq!(report.dropped.allocations, 8);
    }
}
