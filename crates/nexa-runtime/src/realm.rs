use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexa_bytecode::{
    AbandonPolicy, AsyncResultType, CancelPolicy, FunctionEffect, HostImport,
    MigrationLimitRequirements, Signature, ValueType,
};
use nexa_core::{FingerprintBuilder, RawHandle, StableId, StateSchemaFingerprint};
use nexa_verifier::VerifiedModule;

use crate::heap::HeapReservation;
use crate::reload::{ReloadCoordinator, ReloadTransaction};
use crate::scheduler::Scheduler;
use crate::stateful::{MigrationLimitError, MigrationLimits, StatefulDomainId, StatefulRegistry};
use crate::task::TaskExecution;
use crate::{
    CheckedInterpreter, CollectionStats, ContinuationReservation, DiagnosticCode, ExecutionCharge,
    FuelState, GcBudget, GcPhase, GcRef, GcRoots, Heap, HeapError, HostCallOutcome,
    HostCompletionDelivery, HostCompletionResult, HostErrorPayload, HostFunctionSlot, HostPayload,
    HostRegistry, HostRequestError, HostRequestHandle, HostTrap, IncrementalGcReport,
    InterpreterError, InterpreterHost, InterpreterHostOutcome, InterpreterOutcome,
    InterpreterState, Object, OpcodeCostTable, PendingHostRequest, ReloadError,
    ResourceTokenHandle, RuntimeError, RuntimeHost, RuntimeHostArgs, RuntimeHostDomain,
    RuntimeHostState, RuntimeLimits, RuntimeMessage, RuntimeResources, RuntimeTrace, RuntimeValue,
    ScopeHandle, SlotAllocError, SlotPool, SnapshotHandle, StepConfig, SuspendReason, TaskHandle,
    TaskRuntime, TaskState, Trap, TrapKind,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleHandle(RawHandle);

impl ModuleHandle {
    #[must_use]
    pub const fn raw(self) -> RawHandle {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleLifecycle {
    Active,
    Staging,
    Activating,
    ActivationFaulted,
    Retired,
}

#[derive(Clone, Debug)]
pub struct ModuleEpochRoot {
    pub module_id: u32,
    pub stateful_domain: StatefulDomainId,
    pub epoch: u64,
    pub verified: Arc<VerifiedModule>,
    /// F2: predecoded execution rows built once at load; the step path
    /// consumes them instead of recomputing static fuel and safepoints.
    pub executable: Arc<crate::executable::ExecutableModule>,
    pub host_contract_id: StableId,
    host_function_slots: Box<[HostFunctionSlot]>,
    pub lifecycle: ModuleLifecycle,
    globals: Vec<GcRef>,
    state: Arc<StatefulRegistry>,
    staging_roots: Vec<GcRef>,
}

#[derive(Clone)]
struct ExecutionImage {
    verified: Arc<VerifiedModule>,
    executable: Arc<crate::executable::ExecutableModule>,
}

struct ExecutionImageEntry {
    key: [u8; 32],
    image: ExecutionImage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionImageCacheInspection {
    pub entries: usize,
    pub capacity: usize,
    pub hits: u64,
    pub misses: u64,
}

struct ExecutionImageCache {
    entries: VecDeque<ExecutionImageEntry>,
    capacity: usize,
    hits: u64,
    misses: u64,
}

impl ExecutionImageCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
            hits: 0,
            misses: 0,
        }
    }

    fn resolve(
        &mut self,
        verified: VerifiedModule,
        costs: &OpcodeCostTable,
    ) -> Result<ExecutionImage, crate::ExecutableBuildError> {
        let key = execution_image_key(&verified, costs);
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            let entry = self
                .entries
                .remove(index)
                .expect("the located execution image entry exists");
            let image = entry.image.clone();
            self.entries.push_back(entry);
            self.hits = self.hits.saturating_add(1);
            return Ok(image);
        }

        self.misses = self.misses.saturating_add(1);
        let executable = crate::executable::ExecutableModule::build(&verified, costs)?;
        let image = ExecutionImage {
            verified: Arc::new(verified),
            executable: Arc::new(executable),
        };
        if self.capacity != 0 {
            if self.entries.len() == self.capacity {
                self.entries.pop_front();
            }
            self.entries.push_back(ExecutionImageEntry {
                key,
                image: image.clone(),
            });
        }
        Ok(image)
    }

    fn inspection(&self) -> ExecutionImageCacheInspection {
        ExecutionImageCacheInspection {
            entries: self.entries.len(),
            capacity: self.capacity,
            hits: self.hits,
            misses: self.misses,
        }
    }
}

fn execution_image_key(verified: &VerifiedModule, costs: &OpcodeCostTable) -> [u8; 32] {
    let mut fingerprint = FingerprintBuilder::new("nexa.runtime.execution-image", 1);
    fingerprint.field_bytes("portable_module", &verified.portable_fingerprint());
    fingerprint.field_u32("opcode_cost_table_version", costs.version);
    fingerprint.finish_bytes()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootPublicationRecord {
    pub publication_id: u64,
    pub old_root: ModuleHandle,
    pub candidate_root: ModuleHandle,
    pub candidate_epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetiredModuleRecord {
    pub module_id: u32,
    pub epoch: u64,
    pub committed_at_publication: u64,
    pub released: bool,
}

#[derive(Debug)]
struct RetiredModuleLog {
    entries: VecDeque<RetiredModuleRecord>,
    capacity: usize,
}

impl RetiredModuleLog {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, entry: RetiredModuleRecord) {
        if self.capacity == 0 {
            return;
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }
}

struct RealmHostBridge<'a> {
    registry: &'a mut dyn HostRegistry,
    resources: &'a mut RuntimeResources,
    task: TaskHandle,
    module_id: u32,
    epoch: u64,
    function_slots: &'a [HostFunctionSlot],
}

impl InterpreterHost for RealmHostBridge<'_> {
    fn call(
        &mut self,
        import: u32,
        arguments: &[RuntimeValue],
        heap: Option<&mut Heap>,
    ) -> Result<InterpreterHostOutcome, HostTrap> {
        let slot = self.function_slots.get(import as usize).ok_or_else(|| {
            HostTrap::Host("host import index is outside the verified module".into())
        })?;
        let values = RuntimeHostArgs::new(arguments, heap)?;
        let mut context = self
            .resources
            .context(self.task, self.module_id, self.epoch);
        match crate::invoke_host_boundary(|| {
            self.registry.call_runtime(*slot, &mut context, values)
        })? {
            HostCallOutcome::RuntimeImmediate(value) => {
                Ok(InterpreterHostOutcome::Immediate(value))
            }
            HostCallOutcome::Pending(request) => {
                if !self.resources.owns_request(self.task, request) {
                    return Err(HostTrap::Host(
                        "pending host request is not owned by the calling task".into(),
                    ));
                }
                Ok(InterpreterHostOutcome::Pending(request))
            }
        }
    }
}

struct RealmStateBridge<'a> {
    registry: &'a mut StatefulRegistry,
}

impl InterpreterState for RealmStateBridge<'_> {
    fn current_object(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
    ) -> Result<RuntimeValue, RuntimeMessage> {
        self.registry.current_object_proxy(stable_id, type_id)
    }

    fn object_field(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
        field_id: StableId,
        expected: ValueType,
    ) -> Result<RuntimeValue, RuntimeMessage> {
        self.registry
            .current_object_field(stable_id, type_id, field_id, expected)
    }

    fn set_object_field(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
        field_id: StableId,
        expected: ValueType,
        value: RuntimeValue,
    ) -> Result<(), RuntimeMessage> {
        self.registry
            .set_current_object_field(stable_id, type_id, field_id, expected, value)
    }

    fn resolve(
        &mut self,
        handle: crate::StateHandle,
        target: ValueType,
    ) -> Result<RuntimeValue, crate::StateHandleError> {
        self.registry.resolve_runtime_handle(handle, target)
    }

    fn is_alive(&mut self, handle: crate::StateHandle) -> bool {
        self.registry.is_handle_alive(handle)
    }
}

fn completion_to_runtime(
    payload: HostPayload,
    expected: Option<ValueType>,
) -> Result<RuntimeValue, InterpreterError> {
    if expected.is_none() {
        return Ok(RuntimeValue::Unit);
    }
    match (payload, expected) {
        (HostPayload::I32(value), Some(ValueType::I32)) => Ok(RuntimeValue::I32(value)),
        (HostPayload::I64(value), Some(ValueType::I64)) => Ok(RuntimeValue::I64(value)),
        (HostPayload::F32(bits), Some(ValueType::F32)) => Ok(RuntimeValue::F32(bits)),
        (HostPayload::F64(bits), Some(ValueType::F64)) => Ok(RuntimeValue::F64(bits)),
        (HostPayload::Bool(value), Some(ValueType::Bool)) => Ok(RuntimeValue::Bool(value)),
        (HostPayload::Rune(value), Some(ValueType::Rune)) if char::from_u32(value).is_some() => {
            Ok(RuntimeValue::Rune(value))
        }
        (HostPayload::Opaque(value), Some(ValueType::Named(type_id))) => {
            Ok(RuntimeValue::Opaque { value, type_id })
        }
        (HostPayload::Token(value), Some(ValueType::Named(_))) => {
            Ok(RuntimeValue::ResourceToken(value))
        }
        (HostPayload::Snapshot(value), Some(ValueType::Named(type_id)))
            if value.type_id() == type_id =>
        {
            Ok(RuntimeValue::Snapshot(value))
        }
        _ => Err(InterpreterError::TypeMismatch),
    }
}

#[derive(Clone, Debug)]
pub struct RealmConfig {
    pub realm_id: u32,
    pub runtime_limits: RuntimeLimits,
    pub max_modules: u32,
    /// Maximum immutable verified/executable image pairs retained for
    /// content-identical load and reload reuse.
    pub execution_image_cache_capacity: usize,
    pub max_heap_objects: u32,
    /// G6: ceiling over live out-of-slot payload bytes (`GC_V1` heap
    /// accounting); `u64::MAX` disables the byte limit.
    pub max_heap_bytes: u64,
    pub max_string_bytes: usize,
    pub max_collection_elements: usize,
    pub max_collection_ranges: usize,
    pub max_host_resources: u32,
    pub release_capacity: usize,
    pub tombstone_capacity: usize,
    pub cost_table: OpcodeCostTable,
    pub migration_limits: MigrationLimits,
}

impl Default for RealmConfig {
    fn default() -> Self {
        Self {
            realm_id: 1,
            runtime_limits: RuntimeLimits::default(),
            max_modules: 16,
            execution_image_cache_capacity: 8,
            max_heap_objects: 4_096,
            max_heap_bytes: u64::MAX,
            max_string_bytes: 1024 * 1024,
            max_collection_elements: 65_536,
            max_collection_ranges: 4_097,
            max_host_resources: 1_024,
            release_capacity: 2_048,
            tombstone_capacity: 1_024,
            cost_table: OpcodeCostTable::default(),
            migration_limits: MigrationLimits::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingReason {
    Fuel,
    ExplicitYield,
    HostRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YieldReason {
    Fuel,
    Explicit,
}

pub type NexaValue = RuntimeValue;
pub type HostResult = HostCompletionResult;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskPoll {
    Completed(NexaValue),
    Yielded(YieldReason),
    Waiting(HostRequestHandle),
    Cancelled(CancelReason),
    Trapped(RuntimeError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionDisposition {
    Delivered,
    Discarded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartReloadPolicy {
    pub migration_arguments: Vec<RuntimeValue>,
    pub activation_arguments: Vec<RuntimeValue>,
    pub activation_fuel: u64,
}

impl Default for RestartReloadPolicy {
    fn default() -> Self {
        Self {
            migration_arguments: Vec::new(),
            activation_arguments: Vec::new(),
            activation_fuel: 4_096,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestartReloadOutcome {
    Committed(ModuleHandle),
    RolledBackBeforeCommit {
        candidate: ModuleHandle,
        reason: ReloadError,
    },
    ActivationFaulted {
        candidate: ModuleHandle,
        error: ReloadError,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestartReloadMetrics {
    pub quiesce_duration: Duration,
    pub migration_duration: Duration,
    pub commit_duration: Duration,
    pub activation_duration: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartReloadResult {
    pub outcome: RestartReloadOutcome,
    pub metrics: RestartReloadMetrics,
}

/// Stable, typed identity of the one cell that a staged REPL candidate may run.
///
/// Transactional cells are always task-effect exports. Synchronous cells are
/// lowered to task wrappers by the compiler, which keeps this runtime boundary
/// independent of raw function indices and prevents callers from accidentally
/// invoking migration, cleanup, or activation functions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionalCellEntrypoint {
    stable_id: StableId,
    signature: Signature,
    state_extension: Option<TransactionalStateExtension>,
}

impl TransactionalCellEntrypoint {
    #[must_use]
    pub const fn new(stable_id: StableId, signature: Signature) -> Self {
        Self {
            stable_id,
            signature,
            state_extension: None,
        }
    }

    /// Marks this entrypoint as the sole writer for a staged REPL environment
    /// schema extension. The runtime validates the exact old/candidate schema
    /// delta; this value is authority to use the narrow transactional path,
    /// not authority to change arbitrary state.
    #[must_use]
    pub const fn with_state_extension(mut self, environment: StableId) -> Self {
        self.state_extension = Some(TransactionalStateExtension { environment });
        self
    }

    #[must_use]
    pub const fn stable_id(&self) -> StableId {
        self.stable_id
    }

    #[must_use]
    pub const fn signature(&self) -> &Signature {
        &self.signature
    }

    #[must_use]
    pub const fn effect(&self) -> FunctionEffect {
        FunctionEffect::Task
    }

    #[must_use]
    pub const fn state_extension(&self) -> Option<TransactionalStateExtension> {
        self.state_extension
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionalStateExtension {
    environment: StableId,
}

impl TransactionalStateExtension {
    #[must_use]
    pub const fn environment(self) -> StableId {
        self.environment
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionalCellPoll {
    Yielded(YieldReason),
    Waiting(HostRequestHandle),
    ReadyToCommit {
        value: RuntimeValue,
        charge: ExecutionCharge,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionalCellCommit {
    pub module: ModuleHandle,
    pub value: RuntimeValue,
    pub charge: ExecutionCharge,
}

#[derive(Debug)]
pub enum TransactionalCellFailureCause {
    Cancelled(CancelReason),
    Trapped(Box<RuntimeError>),
    Runtime(Box<RuntimeError>),
    Activation(Box<RealmError>),
    NotReady,
    AlreadyFinished,
}

#[derive(Debug)]
pub struct TransactionalCellFailure {
    pub cause: TransactionalCellFailureCause,
    /// Cleanup is best-effort but never short-circuited. This records the first
    /// cleanup failure while preserving the primary cell failure above.
    pub rollback_error: Option<Box<RealmError>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionalCellRollback {
    pub candidate: ModuleHandle,
    pub reason: CancelReason,
}

/// Exclusive guard for a prepared REPL cell candidate.
///
/// While this value exists the Realm cannot be used through another mutable
/// reference. A successful cell still does not publish until [`Self::commit`];
/// every other terminal path and `Drop` release the candidate.
pub struct StagedCellTransaction<'a> {
    realm: &'a mut RealmRuntime,
    candidate: ModuleHandle,
    task: TaskHandle,
    fuel_slice: u64,
    activation_arguments: Vec<RuntimeValue>,
    activation_fuel: u64,
    ready: Option<(RuntimeValue, ExecutionCharge)>,
    heap_checkpoint: Option<crate::heap::HeapCheckpoint>,
    session_checkpoint: Option<TransactionalSessionCheckpoint>,
    finished: bool,
}

#[derive(Clone, Copy, Debug)]
struct TransactionalSessionCheckpoint {
    next_epoch: u64,
    next_stateful_domain: u64,
    last_migration_usage_report: Option<crate::MigrationUsageReport>,
    last_migration_hash: Option<StableId>,
}

struct CommitReloadMeasurement {
    result: Result<ModuleHandle, RealmError>,
    commit_duration: Duration,
    activation_duration: Duration,
}

/// Handle-free reload accounting for high-level embedding inspection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReloadAccounting {
    pub cancelled_tasks: usize,
    pub detached_requests: usize,
    pub total_cancelled_tasks: u64,
    pub total_detached_requests: u64,
}

#[derive(Debug)]
enum PlannedResultPayload {
    Value(RuntimeValue),
    String(String),
    Struct {
        type_id: StableId,
        fields: Vec<PlannedResultPayload>,
    },
    Enum {
        type_id: StableId,
        variant: StableId,
        tag: u32,
        payload: Option<Box<PlannedResultPayload>>,
    },
    Array {
        type_id: StableId,
        element_type: ValueType,
        values: Vec<PlannedResultPayload>,
    },
    Buffer {
        type_id: StableId,
        element_type: ValueType,
        values: Vec<PlannedResultPayload>,
    },
}

#[allow(clippy::too_many_lines)]
fn plan_host_payload(
    payload: &HostPayload,
    expected: ValueType,
    enum_types: &[nexa_bytecode::EnumType],
    struct_types: &[nexa_bytecode::StructType],
    array_types: &[nexa_bytecode::ArrayType],
    buffer_types: &[nexa_bytecode::BufferType],
) -> Result<PlannedResultPayload, InterpreterError> {
    if let HostPayload::String(value) = payload
        && expected == ValueType::String
    {
        return Ok(PlannedResultPayload::String(value.clone()));
    }
    if let (
        HostPayload::Enum {
            type_id,
            variant,
            tag,
            payload,
        },
        ValueType::Named(expected_type),
    ) = (payload, expected)
        && *type_id == expected_type
    {
        let metadata = enum_types
            .iter()
            .find(|enum_type| enum_type.type_id == expected_type)
            .and_then(|enum_type| {
                enum_type
                    .variants
                    .iter()
                    .find(|candidate| candidate.stable_id == *variant && candidate.tag == *tag)
            })
            .ok_or(InterpreterError::TypeMismatch)?;
        let payload = match (payload.as_deref(), metadata.payload_type) {
            (Some(payload), Some(payload_type)) => Some(Box::new(plan_host_payload(
                payload,
                payload_type,
                enum_types,
                struct_types,
                array_types,
                buffer_types,
            )?)),
            (None, None) => None,
            _ => return Err(InterpreterError::TypeMismatch),
        };
        return Ok(PlannedResultPayload::Enum {
            type_id: *type_id,
            variant: *variant,
            tag: *tag,
            payload,
        });
    }
    if let (HostPayload::Struct(fields), ValueType::Named(type_id)) = (payload, expected) {
        let metadata = struct_types
            .iter()
            .find(|struct_type| struct_type.type_id == type_id)
            .ok_or(InterpreterError::TypeMismatch)?;
        if fields.len() != metadata.fields.len() {
            return Err(InterpreterError::TypeMismatch);
        }
        let fields = fields
            .iter()
            .zip(&metadata.fields)
            .map(|(value, field)| {
                plan_host_payload(
                    value,
                    field.ty,
                    enum_types,
                    struct_types,
                    array_types,
                    buffer_types,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(PlannedResultPayload::Struct { type_id, fields });
    }
    if let (HostPayload::Array(array), ValueType::Named(type_id)) = (payload, expected) {
        let metadata = array_types
            .iter()
            .find(|array| array.type_id == type_id)
            .ok_or(InterpreterError::TypeMismatch)?;
        let values = array
            .as_slice()
            .iter()
            .map(|value| {
                plan_host_payload(
                    value,
                    metadata.element,
                    enum_types,
                    struct_types,
                    array_types,
                    buffer_types,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(PlannedResultPayload::Array {
            type_id,
            element_type: metadata.element,
            values,
        });
    }
    if let (HostPayload::Buffer(buffer), ValueType::Named(type_id)) = (payload, expected) {
        let metadata = buffer_types
            .iter()
            .find(|buffer| buffer.type_id == type_id)
            .ok_or(InterpreterError::TypeMismatch)?;
        let values = buffer
            .as_slice()
            .iter()
            .map(|value| {
                plan_host_payload(
                    value,
                    metadata.element,
                    enum_types,
                    struct_types,
                    array_types,
                    buffer_types,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(PlannedResultPayload::Buffer {
            type_id,
            element_type: metadata.element,
            values,
        });
    }
    Ok(PlannedResultPayload::Value(completion_to_runtime(
        payload.clone(),
        Some(expected),
    )?))
}

fn planned_payload_slots(payload: &PlannedResultPayload) -> Result<usize, RealmError> {
    match payload {
        PlannedResultPayload::Value(_) => Ok(0),
        PlannedResultPayload::String(_) => Ok(1),
        PlannedResultPayload::Struct { fields, .. } => {
            fields.iter().try_fold(1_usize, |slots, field| {
                slots
                    .checked_add(planned_payload_slots(field)?)
                    .ok_or(RealmError::Heap(HeapError::CapacityExhausted))
            })
        }
        PlannedResultPayload::Enum { payload, .. } => payload.as_deref().map_or(Ok(1), |payload| {
            planned_payload_slots(payload)?
                .checked_add(1)
                .ok_or(RealmError::Heap(HeapError::CapacityExhausted))
        }),
        PlannedResultPayload::Array { values, .. }
        | PlannedResultPayload::Buffer { values, .. } => {
            values.iter().try_fold(1_usize, |slots, value| {
                slots
                    .checked_add(planned_payload_slots(value)?)
                    .ok_or(RealmError::Heap(HeapError::CapacityExhausted))
            })
        }
    }
}

fn validate_planned_payload(heap: &Heap, payload: &PlannedResultPayload) -> Result<(), RealmError> {
    match payload {
        PlannedResultPayload::String(value) => {
            heap.validate_string_length(value.len())?;
        }
        PlannedResultPayload::Enum {
            payload: Some(payload),
            ..
        } => validate_planned_payload(heap, payload)?,
        PlannedResultPayload::Struct { fields, .. } => {
            for field in fields {
                validate_planned_payload(heap, field)?;
            }
        }
        PlannedResultPayload::Array { values, .. }
        | PlannedResultPayload::Buffer { values, .. } => {
            heap.validate_collection_length(values.len())?;
            for value in values {
                validate_planned_payload(heap, value)?;
            }
        }
        PlannedResultPayload::Value(_) | PlannedResultPayload::Enum { payload: None, .. } => {}
    }
    Ok(())
}

fn commit_planned_payload(
    heap: &mut Heap,
    reservation: &mut HeapReservation,
    payload: PlannedResultPayload,
) -> Result<RuntimeValue, RealmError> {
    match payload {
        PlannedResultPayload::Value(value) => Ok(value),
        PlannedResultPayload::String(value) => {
            let reference = heap.commit(reservation, Object::String(value));
            let hash = heap.string_hash(reference)?;
            Ok(RuntimeValue::String { reference, hash })
        }
        PlannedResultPayload::Struct { type_id, fields } => {
            let field_count = fields.len();
            let mut values = [RuntimeValue::Unit; nexa_bytecode::MAX_STRUCT_FIELDS];
            for (index, field) in fields.into_iter().enumerate() {
                values[index] = commit_planned_payload(heap, reservation, field)?;
            }
            heap.commit_struct(reservation, type_id, &values[..field_count])
                .map_err(RealmError::Heap)
        }
        PlannedResultPayload::Enum {
            type_id,
            variant,
            tag,
            payload,
        } => {
            let payload = payload
                .map(|payload| commit_planned_payload(heap, reservation, *payload))
                .transpose()?;
            let reference = heap.commit(
                reservation,
                Object::Enum {
                    type_id,
                    variant,
                    tag,
                    payload,
                },
            );
            Ok(RuntimeValue::NamedRef { reference, type_id })
        }
        PlannedResultPayload::Array {
            type_id,
            element_type,
            values,
        } => {
            let values = values
                .into_iter()
                .map(|value| commit_planned_payload(heap, reservation, value))
                .collect::<Result<Vec<_>, _>>()?;
            heap.commit_array_values_reserved(reservation, type_id, element_type, &values)
                .map_err(RealmError::Heap)
        }
        PlannedResultPayload::Buffer {
            type_id,
            element_type,
            values,
        } => {
            let values = values
                .into_iter()
                .map(|value| commit_planned_payload(heap, reservation, value))
                .collect::<Result<Vec<_>, _>>()?;
            heap.commit_buffer_values_reserved(reservation, type_id, element_type, &values)
                .map_err(RealmError::Heap)
        }
    }
}

#[derive(Debug)]
enum ResultWritebackAction {
    ResumeDirect(RuntimeValue),
    ResumeDirectPlanned {
        payload: PlannedResultPayload,
        heap: HeapReservation,
    },
    ResumeAsync {
        result: AsyncResultType,
        success: bool,
        payload: PlannedResultPayload,
        heap: HeapReservation,
    },
    Cancel,
    TrapFailure(u32),
    TrapCode {
        code: DiagnosticCode,
        argument: u64,
    },
    TrapMessage(&'static str),
}

#[derive(Debug)]
struct ResultWritebackPreflight {
    action: ResultWritebackAction,
}

#[derive(Debug)]
struct DeliveryWritebackPreflight {
    task: TaskHandle,
    snapshot: crate::TaskSnapshot,
    result: ResultWritebackPreflight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    OwnerDestroyed,
    ScopeCancelled,
    BudgetExceeded,
    RuntimeShutdown,
    ReloadCommit,
    HostCancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum PollResult<T> {
    Completed { value: T, charge: ExecutionCharge },
    Pending(PendingReason),
    Cancelled(CancelReason),
    Trapped(Trap),
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum TaskTerminalReason {
    Completed(Option<RuntimeValue>),
    Cancelled(CancelReason),
    Trapped(Trap),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskTerminalRecord {
    pub state: TaskState,
    pub reason: TaskTerminalReason,
    /// Exact callee-to-caller script stack captured at the terminal boundary.
    ///
    /// Budget cancellation records this before cleanup consumes the active
    /// continuation, allowing package tests and diagnostics to report the real
    /// fuel-exhaustion location without manufacturing a root-only frame.
    pub script_call_stack: Option<crate::ScriptCallStack>,
    pub module_epoch: u64,
    pub continuation_resume_count: u32,
    pub final_charge: ExecutionCharge,
    pub trace_range: std::ops::Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickBudget {
    pub max_tasks: usize,
    pub frame_fuel_budget: u64,
    pub collect_garbage: bool,
}

impl Default for TickBudget {
    fn default() -> Self {
        Self {
            max_tasks: 64,
            frame_fuel_budget: 1_024,
            collect_garbage: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub polled: usize,
    pub completed: usize,
    pub cancelled: usize,
    pub trapped: usize,
    pub releases: usize,
    pub collection: Option<CollectionStats>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeCapacityReport {
    pub module_slots: usize,
    pub task_slots: usize,
    pub scope_slots: usize,
    pub trace_records: usize,
    pub scheduler_ready: usize,
    pub scheduler_waiting: usize,
    pub host_requests: usize,
    pub release_records: usize,
}

#[cfg(any(test, feature = "model-adapter"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleInspection {
    pub handle: ModuleHandle,
    pub generation: u32,
    pub module_id: u32,
    pub epoch: u64,
    pub lifecycle: ModuleLifecycle,
    pub stateful_domain: StatefulDomainId,
    pub state_objects: usize,
    pub module_gc_roots: usize,
    pub state_gc_roots: usize,
    pub staging_gc_roots: usize,
}

#[cfg(any(test, feature = "model-adapter"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskExecutionInspection {
    Ready,
    Running,
    FuelYielded,
    ExplicitYielded,
    Waiting {
        request: HostRequestHandle,
        destination: u16,
    },
    Cancelling,
    Cleanup,
}

#[cfg(any(test, feature = "model-adapter"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulerInspection {
    Ready,
    Waiting(HostRequestHandle),
    Detached,
}

#[cfg(any(test, feature = "model-adapter"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskInspection {
    pub handle: TaskHandle,
    pub state: TaskState,
    pub execution: TaskExecutionInspection,
    pub scheduler: SchedulerInspection,
    pub module_id: u32,
    pub module_generation: u32,
    pub epoch: u64,
    pub ownership: crate::TaskResourceSet,
}

#[cfg(any(test, feature = "model-adapter"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalTaskInspection {
    pub task: TaskHandle,
    pub state: TaskState,
    pub continuation_id: Option<u64>,
    pub continuation_resume_count: u32,
    pub scheduler_token: Option<u64>,
    pub request: Option<HostRequestHandle>,
    pub terminal_record_count: u32,
}

#[cfg(any(test, feature = "model-adapter"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReloadInspectionState {
    #[default]
    Idle,
    Preparing,
    Quiescing,
    Staging,
    Committing,
    Published,
    Activating,
    Completed,
    RolledBack,
    ActivationFaulted,
}

#[cfg(any(test, feature = "model-adapter"))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReloadInspection {
    pub state: ReloadInspectionState,
    pub old_module: Option<ModuleHandle>,
    pub candidate_module: Option<ModuleHandle>,
    pub cancelled_tasks: usize,
    pub detached_requests: usize,
    pub total_cancelled_tasks: u64,
    pub total_detached_requests: u64,
    pub late_completions_discarded: u64,
    pub root_publications: Vec<RootPublicationRecord>,
}

#[cfg(any(test, feature = "model-adapter"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RootInspection {
    pub module_globals: usize,
    pub stateful_registry: usize,
    pub staging_heap: usize,
    pub suspended_tasks: usize,
    pub published_roots: usize,
}

#[cfg(any(test, feature = "model-adapter"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapInspection {
    pub live_objects: usize,
    pub capacity: u32,
}

#[cfg(any(test, feature = "model-adapter"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RealmInspectionSnapshot {
    pub active_root: Option<ModuleInspection>,
    pub candidate_root: Option<ModuleInspection>,
    pub modules: Vec<ModuleInspection>,
    pub retired_modules: Vec<RetiredModuleRecord>,
    pub tasks: Vec<TaskInspection>,
    pub terminal_tasks: Vec<TerminalTaskInspection>,
    pub resources: crate::RuntimeResourceLedger,
    pub completion_accounting: crate::CompletionAccounting,
    pub reload: ReloadInspection,
    pub roots: RootInspection,
    pub heap: HeapInspection,
    pub runtime_host: Option<RuntimeHostState>,
    pub runtime_host_releases: Vec<crate::ReleaseRecord>,
    pub terminal_records: Vec<(TaskHandle, TaskTerminalRecord)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RealmError {
    Runtime(RuntimeError),
    Interpreter(InterpreterError),
    Host(HostRequestError),
    Heap(HeapError),
    ModuleAllocation(SlotAllocError),
    ModuleHandle(crate::HandleError),
    ExecutableBuild(crate::executable::ExecutableBuildError),
    MissingModule(u32),
    HostCapabilitiesUnavailable,
    MissingHostContractRuntimeId,
    RuntimeHostClosing,
    RuntimeHostClosed,
    HostContractIdMismatch,
    MissingHostFunctionAuthority(StableId),
    HostFunctionAuthorityMismatch {
        stable_id: StableId,
        field: HostFunctionAuthorityField,
    },
    SchemaHashMismatch,
    EpochExhausted,
    ModuleNotCallable,
    TerminalTask,
    StaleTaskHandle,
    CrossRealmTaskHandle,
    TaskWaiting,
    Reload(ReloadError),
    State(crate::StatefulError),
    InjectedFailure(crate::RuntimeFailurePoint),
    MissingTransactionalCellExport(StableId),
    MissingScriptExport(StableId),
    ScriptExportMetadataMismatch(StableId),
    ScriptExportNotCallable(StableId),
    TransactionalCellSignatureMismatch(StableId),
    TransactionalCellEffectMismatch {
        stable_id: StableId,
        actual: FunctionEffect,
    },
    InvalidTransactionalStateExtension,
    InvalidTransactionalStateSeed,
    TransactionalCellTerminalRecordMissing,
    TransactionalCellSetupRollbackFailed {
        setup: Box<RealmError>,
        rollback: Box<RealmError>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostFunctionAuthorityField {
    StableId,
    DeclarationFingerprint,
    Capabilities,
    Parameters,
    Result,
    Mode,
    FuelCost,
    AsyncResultPresence,
    AsyncResultType,
    AsyncSuccessType,
    AsyncErrorType,
    CancelPolicy,
    AbandonPolicy,
    CancelErrorVariant,
    AbandonErrorVariant,
}

impl fmt::Display for RealmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for RealmError {}

impl From<RuntimeError> for RealmError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl From<RealmError> for RuntimeError {
    fn from(error: RealmError) -> Self {
        match error {
            RealmError::Runtime(error) => error,
            RealmError::TerminalTask => Self::TerminalTask,
            RealmError::StaleTaskHandle => Self::StaleTaskHandle,
            RealmError::CrossRealmTaskHandle => Self::CrossRealmTaskHandle,
            error => Self::Realm(Box::new(error)),
        }
    }
}

impl From<InterpreterError> for RealmError {
    fn from(error: InterpreterError) -> Self {
        Self::Interpreter(error)
    }
}

impl From<HostRequestError> for RealmError {
    fn from(error: HostRequestError) -> Self {
        Self::Host(error)
    }
}

impl From<HeapError> for RealmError {
    fn from(error: HeapError) -> Self {
        Self::Heap(error)
    }
}

impl From<ReloadError> for RealmError {
    fn from(error: ReloadError) -> Self {
        Self::Reload(error)
    }
}

impl From<crate::StatefulError> for RealmError {
    fn from(error: crate::StatefulError) -> Self {
        Self::State(error)
    }
}

pub struct RealmRuntime {
    realm_id: u32,
    modules: SlotPool<ModuleEpochRoot>,
    active_root: Option<ModuleHandle>,
    root_publications: VecDeque<RootPublicationRecord>,
    next_publication_id: u64,
    tasks: TaskRuntime,
    resources: RuntimeResources,
    heap: Heap,
    scheduler: Scheduler,
    cost_table: OpcodeCostTable,
    tombstones: VecDeque<(TaskHandle, TaskTerminalRecord)>,
    tombstone_capacity: usize,
    next_epoch: u64,
    next_stateful_domain: u64,
    retired_modules: RetiredModuleLog,
    reload: ReloadCoordinator,
    migration_limits: MigrationLimits,
    last_migration_usage_report: Option<crate::MigrationUsageReport>,
    last_migration_hash: Option<StableId>,
    last_reload_cancelled_tasks: usize,
    last_reload_detached_requests: usize,
    total_reload_cancelled_tasks: u64,
    total_reload_detached_requests: u64,
    host_registry: Option<Box<dyn HostRegistry>>,
    host_registry_contract_id: Option<StableId>,
    runtime_host: Option<RuntimeHost>,
    failure_injector: crate::RuntimeFailureInjector,
    /// WP95/WP96: process-local predecoded images keyed by portable artifact
    /// identity. The cache is realm-bounded and never serializes dense slots
    /// or runtime pointers.
    execution_images: ExecutionImageCache,
    /// G2 trigger baseline: cumulative object allocations at the moment
    /// the last incremental cycle completed.
    gc_cycle_baseline: u64,
    /// H1: bounded pool of retired continuation arenas. Admission pops one
    /// and reuses its storage when the capacities satisfy the module's
    /// reservation; terminal polls push the storage back. Bounded so idle
    /// realms never retain more than a few task stacks worth of memory.
    continuation_pool: Vec<crate::FrameArena>,
}

impl RealmRuntime {
    fn base(config: RealmConfig) -> Self {
        let execution_image_cache_capacity = config
            .execution_image_cache_capacity
            .min(config.max_modules as usize);
        let failure_injector = crate::RuntimeFailureInjector::default();
        let mut tasks = TaskRuntime::new(config.realm_id, config.runtime_limits);
        tasks.set_failure_injector(failure_injector.clone());
        let mut resources = RuntimeResources::new(
            config.realm_id,
            config.max_host_resources,
            config.release_capacity,
        );
        resources.set_failure_injector(failure_injector.clone());
        let mut heap = Heap::new_with_arena_limits(
            config.max_heap_objects,
            config.max_string_bytes,
            Heap::DEFAULT_MAX_COLLECTION_LENGTH,
            config.max_collection_elements,
            config.max_collection_ranges,
        );
        heap.set_failure_injector(failure_injector.clone());
        heap.set_max_heap_bytes(config.max_heap_bytes);
        Self {
            realm_id: config.realm_id,
            modules: SlotPool::with_capacity_limit(config.realm_id, config.max_modules),
            active_root: None,
            root_publications: VecDeque::with_capacity(config.max_modules as usize),
            next_publication_id: 1,
            tasks,
            resources,
            heap,
            scheduler: Scheduler::with_capacity(
                config.runtime_limits.max_scheduler_tokens as usize,
            ),
            cost_table: config.cost_table,
            tombstones: VecDeque::with_capacity(config.tombstone_capacity),
            tombstone_capacity: config.tombstone_capacity,
            next_epoch: 1,
            next_stateful_domain: 1,
            retired_modules: RetiredModuleLog::new(config.max_modules as usize),
            reload: ReloadCoordinator::default(),
            migration_limits: config.migration_limits,
            last_migration_usage_report: None,
            last_migration_hash: None,
            last_reload_cancelled_tasks: 0,
            last_reload_detached_requests: 0,
            total_reload_cancelled_tasks: 0,
            total_reload_detached_requests: 0,
            host_registry: None,
            host_registry_contract_id: None,
            runtime_host: None,
            failure_injector,
            execution_images: ExecutionImageCache::new(execution_image_cache_capacity),
            gc_cycle_baseline: 0,
            // H1: preallocated so terminal-path pushes never allocate; the
            // allocation-observer gates pin task terminals at zero.
            continuation_pool: Vec::with_capacity(Self::CONTINUATION_POOL_LIMIT),
        }
    }

    #[must_use]
    pub fn isolated(config: RealmConfig) -> Self {
        Self::base(config)
    }

    pub fn hosted(
        config: RealmConfig,
        runtime_host: RuntimeHost,
        registry: Box<dyn HostRegistry>,
    ) -> Result<Self, RealmError> {
        let host_registry_contract_id = registry
            .contract_runtime_id()
            .ok_or(RealmError::MissingHostContractRuntimeId)?;
        runtime_host.register_realm().map_err(|state| match state {
            RuntimeHostState::Closing => RealmError::RuntimeHostClosing,
            RuntimeHostState::Closed => RealmError::RuntimeHostClosed,
            RuntimeHostState::Open => unreachable!("open hosts admit realms"),
        })?;
        let resource_config = config.clone();
        let mut realm = Self::base(config);
        realm.host_registry = Some(registry);
        realm.host_registry_contract_id = Some(host_registry_contract_id);
        realm.resources = RuntimeResources::with_runtime_host(
            resource_config.realm_id,
            resource_config.max_host_resources,
            resource_config.release_capacity,
            &runtime_host,
        );
        realm
            .resources
            .set_failure_injector(realm.failure_injector.clone());
        realm.runtime_host = Some(runtime_host);
        Ok(realm)
    }

    pub fn create_scope(&mut self, parent: Option<ScopeHandle>) -> Result<ScopeHandle, RealmError> {
        Ok(self.tasks.create_scope(parent)?)
    }

    #[cfg(any(test, feature = "model-adapter"))]
    pub fn destroy_empty_scope(&mut self, scope: ScopeHandle) -> Result<(), RealmError> {
        Ok(self.tasks.destroy_scope(scope)?)
    }

    #[must_use]
    pub fn failure_injector(&self) -> &crate::RuntimeFailureInjector {
        &self.failure_injector
    }

    #[cfg(any(test, feature = "model-adapter"))]
    pub fn preflight_host_request_admission(&self) -> Result<(), RealmError> {
        for point in [
            crate::RuntimeFailurePoint::RequestSlot,
            crate::RuntimeFailurePoint::CompletionSlot,
            crate::RuntimeFailurePoint::ReleaseSlot,
        ] {
            self.fail_if_injected(point)?;
        }
        Ok(())
    }

    pub fn scope_snapshot(&self, scope: ScopeHandle) -> Result<crate::ScopeSnapshot, RealmError> {
        Ok(self.tasks.scope_snapshot(scope)?)
    }

    #[must_use]
    pub const fn realm_id(&self) -> u32 {
        self.realm_id
    }

    #[must_use]
    pub fn reserved_capacities(&self) -> RuntimeCapacityReport {
        let (task_slots, scope_slots, trace_records) = self.tasks.reserved_capacities();
        let (scheduler_ready, scheduler_waiting) = self.scheduler.reserved_capacities();
        let (host_requests, release_records) = self.resources.reserved_capacities();
        RuntimeCapacityReport {
            module_slots: self.modules.reserved_capacity(),
            task_slots,
            scope_slots,
            trace_records,
            scheduler_ready,
            scheduler_waiting,
            host_requests,
            release_records,
        }
    }

    pub fn active_module_epoch(&self, module: ModuleHandle) -> Result<u64, RealmError> {
        Ok(self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .epoch)
    }

    #[must_use]
    pub const fn reload_accounting(&self) -> ReloadAccounting {
        ReloadAccounting {
            cancelled_tasks: self.last_reload_cancelled_tasks,
            detached_requests: self.last_reload_detached_requests,
            total_cancelled_tasks: self.total_reload_cancelled_tasks,
            total_detached_requests: self.total_reload_detached_requests,
        }
    }

    pub fn module_stateful_domain(
        &self,
        module: ModuleHandle,
    ) -> Result<StatefulDomainId, RealmError> {
        Ok(self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .stateful_domain)
    }

    #[must_use]
    pub const fn active_root(&self) -> Option<ModuleHandle> {
        self.active_root
    }

    pub fn module_lifecycle(&self, module: ModuleHandle) -> Result<ModuleLifecycle, RealmError> {
        Ok(self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .lifecycle)
    }

    pub fn insert_state(
        &mut self,
        module: ModuleHandle,
        stable_id: StableId,
        value: crate::StateValue,
    ) -> Result<crate::StateHandle, RealmError> {
        let root = self
            .modules
            .resolve_mut(module.raw())
            .map_err(RealmError::ModuleHandle)?;
        Ok(Arc::make_mut(&mut root.state).insert(stable_id, value)?)
    }

    /// Creates revision zero's unique, empty transactional environment.
    ///
    /// The seed module must expose exactly one zero-field state Class and no
    /// lifecycle entry. This operation is intentionally distinct from generic
    /// `insert_state`, so REPL setup cannot silently seed an arbitrary schema.
    pub fn initialize_transactional_state_seed(
        &mut self,
        module: ModuleHandle,
        environment: StableId,
    ) -> Result<crate::StateHandle, RealmError> {
        let root = self
            .modules
            .resolve_mut(module.raw())
            .map_err(RealmError::ModuleHandle)?;
        let schema = &root.verified.module().state_schema;
        let [state_type] = schema.types.as_slice() else {
            return Err(RealmError::InvalidTransactionalStateSeed);
        };
        let reload = root.verified.module().reload_metadata;
        if root.lifecycle != ModuleLifecycle::Active
            || root.state.object_count() != 0
            || state_type.stable_id != environment
            || state_type.version == 0
            || !state_type.fields.is_empty()
            || reload.migration_entry.is_some()
            || reload.activation_entry.is_some()
        {
            return Err(RealmError::InvalidTransactionalStateSeed);
        }
        let handle = Arc::make_mut(&mut root.state).insert(
            environment,
            crate::StateValue::Object(crate::StateObject {
                type_id: environment,
                version: state_type.version,
                fields: BTreeMap::new(),
            }),
        )?;
        root.state
            .validate_transactional_state(schema)
            .map_err(RealmError::State)?;
        Ok(handle)
    }

    pub fn state_handles(
        &self,
        module: ModuleHandle,
    ) -> Result<Vec<crate::StateHandle>, RealmError> {
        Ok(self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .state
            .handles())
    }

    pub fn resolve_state(
        &self,
        module: ModuleHandle,
        handle: crate::StateHandle,
    ) -> Result<crate::StateValue, RealmError> {
        Ok(self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .state
            .resolve(handle)?)
    }

    pub fn state_handle_value(
        &self,
        module: ModuleHandle,
        handle: crate::StateHandle,
    ) -> Result<RuntimeValue, RealmError> {
        Ok(self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .state
            .runtime_handle(handle)?)
    }

    pub fn state_handle_is_alive(
        &self,
        module: ModuleHandle,
        handle: crate::StateHandle,
    ) -> Result<bool, RealmError> {
        Ok(self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .state
            .is_handle_alive(handle))
    }

    #[must_use]
    pub fn migration_capacity_report(&self) -> crate::MigrationCapacityReport {
        self.migration_limits.capacity_report()
    }

    #[must_use]
    pub const fn last_migration_usage_report(&self) -> Option<crate::MigrationUsageReport> {
        self.last_migration_usage_report
    }

    #[must_use]
    pub const fn last_migration_hash(&self) -> Option<StableId> {
        self.last_migration_hash
    }

    fn resolve_host_functions(
        &self,
        imports: &[HostImport],
    ) -> Result<Box<[HostFunctionSlot]>, RealmError> {
        if imports.is_empty() {
            return Ok(Box::new([]));
        }
        let registry = self
            .host_registry
            .as_deref()
            .ok_or(RealmError::HostCapabilitiesUnavailable)?;
        let mut slots = Vec::with_capacity(imports.len());
        for import in imports {
            let resolved = registry
                .resolve_function(import.stable_id)
                .ok_or(RealmError::MissingHostFunctionAuthority(import.stable_id))?;
            let contract = resolved.authority();
            let mismatch = |field| RealmError::HostFunctionAuthorityMismatch {
                stable_id: import.stable_id,
                field,
            };
            if contract.stable_id() != import.stable_id {
                return Err(mismatch(HostFunctionAuthorityField::StableId));
            }
            if contract.declaration_fingerprint() != import.declaration_fingerprint {
                return Err(mismatch(HostFunctionAuthorityField::DeclarationFingerprint));
            }
            let capabilities = contract.capabilities();
            if capabilities.len() != import.capabilities.len()
                || capabilities
                    .iter()
                    .zip(&import.capabilities)
                    .any(|(expected, actual)| expected != actual)
            {
                return Err(mismatch(HostFunctionAuthorityField::Capabilities));
            }
            if contract.parameters() != import.parameters.as_slice() {
                return Err(mismatch(HostFunctionAuthorityField::Parameters));
            }
            if contract.result() != import.result {
                return Err(mismatch(HostFunctionAuthorityField::Result));
            }
            if contract.mode() != import.mode {
                return Err(mismatch(HostFunctionAuthorityField::Mode));
            }
            if contract.fuel_cost() != import.fuel_cost {
                return Err(mismatch(HostFunctionAuthorityField::FuelCost));
            }
            match (contract.async_result(), import.async_result) {
                (None, None) => {}
                (Some(_), None) | (None, Some(_)) => {
                    return Err(mismatch(HostFunctionAuthorityField::AsyncResultPresence));
                }
                (Some(expected), Some(actual)) => {
                    if expected.result_type != actual.result_type {
                        return Err(mismatch(HostFunctionAuthorityField::AsyncResultType));
                    }
                    if expected.success != actual.success {
                        return Err(mismatch(HostFunctionAuthorityField::AsyncSuccessType));
                    }
                    if expected.error != actual.error {
                        return Err(mismatch(HostFunctionAuthorityField::AsyncErrorType));
                    }
                    if expected.cancel_policy != actual.cancel_policy {
                        return Err(mismatch(HostFunctionAuthorityField::CancelPolicy));
                    }
                    if expected.abandon_policy != actual.abandon_policy {
                        return Err(mismatch(HostFunctionAuthorityField::AbandonPolicy));
                    }
                    if expected.cancel_error != actual.cancel_error {
                        return Err(mismatch(HostFunctionAuthorityField::CancelErrorVariant));
                    }
                    if expected.abandon_error != actual.abandon_error {
                        return Err(mismatch(HostFunctionAuthorityField::AbandonErrorVariant));
                    }
                }
            }
            slots.push(resolved.slot());
        }
        Ok(slots.into_boxed_slice())
    }

    pub fn load_module(
        &mut self,
        verified: VerifiedModule,
        host_contract_id: StableId,
        state_schema_fingerprint: StateSchemaFingerprint,
    ) -> Result<ModuleHandle, RealmError> {
        if self.runtime_host.is_none() && module_requires_host_capabilities(verified.module()) {
            return Err(RealmError::HostCapabilitiesUnavailable);
        }
        if self
            .host_registry_contract_id
            .is_some_and(|registry_id| registry_id != host_contract_id)
        {
            return Err(RealmError::HostContractIdMismatch);
        }
        if verified.module().host_contract_id != Some(host_contract_id) {
            return Err(RealmError::HostContractIdMismatch);
        }
        let host_function_slots = self.resolve_host_functions(&verified.module().host_imports)?;
        if verified.module().state_schema_fingerprint != state_schema_fingerprint {
            return Err(RealmError::SchemaHashMismatch);
        }
        let epoch = self.next_epoch;
        let next_epoch = self
            .next_epoch
            .checked_add(1)
            .ok_or(RealmError::EpochExhausted)?;
        let stateful_domain = StatefulDomainId::new(self.next_stateful_domain);
        let next_stateful_domain = self
            .next_stateful_domain
            .checked_add(1)
            .ok_or(RealmError::EpochExhausted)?;
        // WP95: the predecoded rows remain part of admission, but an
        // identical portable artifact reuses its immutable verified module
        // and executable rows. Reusing the pair together preserves the
        // code-backing identity held by static-leaf certificates.
        let image = self
            .execution_images
            .resolve(verified, &self.cost_table)
            .map_err(RealmError::ExecutableBuild)?;
        let raw = self
            .modules
            .try_allocate(ModuleEpochRoot {
                module_id: 0,
                stateful_domain,
                epoch,
                verified: image.verified,
                executable: image.executable,
                host_contract_id,
                host_function_slots,
                lifecycle: ModuleLifecycle::Active,
                globals: Vec::new(),
                state: Arc::new(StatefulRegistry::new(stateful_domain)),
                staging_roots: Vec::new(),
            })
            .map_err(RealmError::ModuleAllocation)?;
        self.next_epoch = next_epoch;
        self.next_stateful_domain = next_stateful_domain;
        let loaded = self
            .modules
            .resolve_mut(raw)
            .expect("new module handle resolves");
        loaded.module_id = raw.index;
        let handle = ModuleHandle(raw);
        if self.active_root.is_none() {
            self.set_active_root(handle);
        }
        Ok(handle)
    }

    #[must_use]
    pub fn execution_image_cache_inspection(&self) -> ExecutionImageCacheInspection {
        self.execution_images.inspection()
    }

    pub(crate) fn prepare_reload(
        &mut self,
        old_module: ModuleHandle,
        candidate: VerifiedModule,
        host_contract_id: StableId,
    ) -> Result<ModuleHandle, RealmError> {
        if self.reload.active() {
            return Err(ReloadError::InvalidState.into());
        }
        let old = self
            .modules
            .resolve(old_module.raw())
            .map_err(RealmError::ModuleHandle)?;
        if self.active_root != Some(old_module) || old.lifecycle != ModuleLifecycle::Active {
            return Err(ReloadError::InvalidState.into());
        }
        if old.verified.module().host_contract_id != Some(host_contract_id) {
            return Err(RealmError::HostContractIdMismatch);
        }
        self.resolve_host_functions(&candidate.module().host_imports)?;
        let old_module_id = old.module_id;
        let old_epoch = old.epoch;
        let stateful_domain = old.stateful_domain;
        let candidate_schema = candidate.module().state_schema_fingerprint;
        if nexa_verifier::verify_reload_transition(&old.verified, &candidate).is_err() {
            return Err(
                ReloadError::Migration("schema changes require a migration entry".into()).into(),
            );
        }
        if let Some(error) = migration_requirement_error(
            self.migration_limits,
            candidate.module().reload_metadata.minimum_migration_limits,
        ) {
            return Err(ReloadError::MigrationLimit(error).into());
        }
        let candidate = self.load_module(candidate, host_contract_id, candidate_schema)?;
        let candidate_root = self
            .modules
            .resolve_mut(candidate.raw())
            .map_err(RealmError::ModuleHandle)?;
        candidate_root.lifecycle = ModuleLifecycle::Staging;
        candidate_root.stateful_domain = stateful_domain;
        candidate_root.state = Arc::new(StatefulRegistry::new(stateful_domain));
        let transaction = ReloadTransaction {
            old_module,
            candidate,
            old_module_id,
            old_epoch,
            cancelled_task_count: 0,
            detached_request_count: 0,
        };
        if let Err(error) = self.reload.begin(transaction) {
            self.modules
                .release(candidate.raw())
                .map_err(RealmError::ModuleHandle)?;
            return Err(error.into());
        }
        Ok(candidate)
    }

    fn prepare_transactional_state_reload(
        &mut self,
        old_module: ModuleHandle,
        candidate: VerifiedModule,
        host_contract_id: StableId,
        extension: TransactionalStateExtension,
    ) -> Result<ModuleHandle, RealmError> {
        if self.reload.active() {
            return Err(ReloadError::InvalidState.into());
        }
        let old = self
            .modules
            .resolve(old_module.raw())
            .map_err(RealmError::ModuleHandle)?;
        if self.active_root != Some(old_module) || old.lifecycle != ModuleLifecycle::Active {
            return Err(ReloadError::InvalidState.into());
        }
        if old.verified.module().host_contract_id != Some(host_contract_id) {
            return Err(RealmError::HostContractIdMismatch);
        }
        validate_transactional_state_transition(
            old.verified.module(),
            candidate.module(),
            extension.environment(),
        )?;
        self.resolve_host_functions(&candidate.module().host_imports)?;
        let old_module_id = old.module_id;
        let old_epoch = old.epoch;
        let stateful_domain = old.stateful_domain;
        let candidate_schema = candidate.module().state_schema_fingerprint;
        let candidate = self.load_module(candidate, host_contract_id, candidate_schema)?;
        let candidate_root = self
            .modules
            .resolve_mut(candidate.raw())
            .map_err(RealmError::ModuleHandle)?;
        candidate_root.lifecycle = ModuleLifecycle::Staging;
        candidate_root.stateful_domain = stateful_domain;
        candidate_root.state = Arc::new(StatefulRegistry::new(stateful_domain));
        let transaction = ReloadTransaction {
            old_module,
            candidate,
            old_module_id,
            old_epoch,
            cancelled_task_count: 0,
            detached_request_count: 0,
        };
        if let Err(error) = self.reload.begin(transaction) {
            self.modules
                .release(candidate.raw())
                .map_err(RealmError::ModuleHandle)?;
            return Err(error.into());
        }
        Ok(candidate)
    }

    pub fn restart_reload(
        &mut self,
        module: ModuleHandle,
        candidate: VerifiedModule,
        policy: RestartReloadPolicy,
    ) -> Result<RestartReloadOutcome, ReloadError> {
        self.restart_reload_measured(module, candidate, policy)
            .map(|result| result.outcome)
    }

    pub fn restart_reload_measured(
        &mut self,
        module: ModuleHandle,
        candidate: VerifiedModule,
        policy: RestartReloadPolicy,
    ) -> Result<RestartReloadResult, ReloadError> {
        let RestartReloadPolicy {
            migration_arguments,
            activation_arguments,
            activation_fuel,
        } = policy;
        let mut metrics = RestartReloadMetrics::default();
        let host_contract_id = self
            .modules
            .resolve(module.raw())
            .map_err(|error| ReloadError::Migration(RuntimeMessage::inline(&error.to_string())))?
            .host_contract_id;
        let candidate = self
            .prepare_reload(module, candidate, host_contract_id)
            .map_err(restart_reload_error)?;
        let quiesce_started = Instant::now();
        self.quiesce_reload().map_err(restart_reload_error)?;
        metrics.quiesce_duration = quiesce_started.elapsed();

        let migration_started = Instant::now();
        if let Err(error) = self.stage_reload(&migration_arguments) {
            metrics.migration_duration = migration_started.elapsed();
            let reason = restart_reload_error(error);
            self.rollback_reload().map_err(restart_reload_error)?;
            self.flush_releases();
            return Ok(RestartReloadResult {
                outcome: RestartReloadOutcome::RolledBackBeforeCommit { candidate, reason },
                metrics,
            });
        }
        metrics.migration_duration = migration_started.elapsed();

        let activation_heap_checkpoint = self.heap.checkpoint();
        let commit = self.commit_reload_measured(&activation_arguments, activation_fuel);
        metrics.commit_duration = commit.commit_duration;
        metrics.activation_duration = commit.activation_duration;
        let outcome = match commit.result {
            Ok(committed) => {
                self.flush_releases();
                RestartReloadOutcome::Committed(committed)
            }
            Err(error)
                if self.active_root == Some(candidate)
                    && self
                        .module_lifecycle(candidate)
                        .is_ok_and(|state| state == ModuleLifecycle::ActivationFaulted) =>
            {
                let error = restart_reload_error(error);
                self.rollback_activation_fault(candidate, activation_heap_checkpoint)
                    .map_err(restart_reload_error)?;
                self.flush_releases();
                RestartReloadOutcome::ActivationFaulted { candidate, error }
            }
            Err(error) => return Err(restart_reload_error(error)),
        };
        Ok(RestartReloadResult { outcome, metrics })
    }

    fn transactional_session_checkpoint(&self) -> TransactionalSessionCheckpoint {
        TransactionalSessionCheckpoint {
            next_epoch: self.next_epoch,
            next_stateful_domain: self.next_stateful_domain,
            last_migration_usage_report: self.last_migration_usage_report,
            last_migration_hash: self.last_migration_hash,
        }
    }

    fn restore_transactional_session_checkpoint(
        &mut self,
        checkpoint: TransactionalSessionCheckpoint,
    ) {
        self.next_epoch = checkpoint.next_epoch;
        self.next_stateful_domain = checkpoint.next_stateful_domain;
        self.last_migration_usage_report = checkpoint.last_migration_usage_report;
        self.last_migration_hash = checkpoint.last_migration_hash;
    }

    fn rollback_failed_cell_setup(
        &mut self,
        setup: RealmError,
        heap_checkpoint: crate::heap::HeapCheckpoint,
        session_checkpoint: TransactionalSessionCheckpoint,
    ) -> RealmError {
        let cleanup = if self.reload.active() {
            self.cleanup_staged_cell(None, CancelReason::HostCancelled)
        } else {
            Ok(())
        };
        self.heap.restore_checkpoint(heap_checkpoint);
        let rollback = cleanup.err().or_else(|| {
            self.reload
                .active()
                .then_some(RealmError::Reload(ReloadError::InvalidState))
        });
        if let Some(rollback) = rollback {
            RealmError::TransactionalCellSetupRollbackFailed {
                setup: Box::new(setup),
                rollback: Box::new(rollback),
            }
        } else {
            self.restore_transactional_session_checkpoint(session_checkpoint);
            setup
        }
    }

    /// Prepare and migrate a candidate module without publishing it, then
    /// start exactly one stable, typed task export from that candidate.
    ///
    /// The returned guard owns the mutable Realm borrow. It must observe a
    /// successful terminal value and explicitly commit before the candidate
    /// can become active; dropping it rolls the candidate back.
    pub fn stage_cell_transaction(
        &mut self,
        old_module: ModuleHandle,
        candidate: VerifiedModule,
        entrypoint: &TransactionalCellEntrypoint,
        cell_arguments: &[RuntimeValue],
        policy: RestartReloadPolicy,
        step: StepConfig,
    ) -> Result<StagedCellTransaction<'_>, RealmError> {
        let host_contract_id = self
            .modules
            .resolve(old_module.raw())
            .map_err(RealmError::ModuleHandle)?
            .host_contract_id;
        let session_checkpoint = self.transactional_session_checkpoint();
        let heap_checkpoint = self.heap.checkpoint();
        let state_extension = entrypoint.state_extension();
        let prepared = match state_extension {
            Some(extension) => self.prepare_transactional_state_reload(
                old_module,
                candidate,
                host_contract_id,
                extension,
            ),
            None => self.prepare_reload(old_module, candidate, host_contract_id),
        };
        if let Err(error) = prepared {
            return Err(self.rollback_failed_cell_setup(
                error,
                heap_checkpoint,
                session_checkpoint,
            ));
        }
        let Ok(candidate) = prepared else {
            unreachable!("the failed prepare branch returned above");
        };
        let setup = (|| {
            self.quiesce_reload()?;
            match state_extension {
                Some(extension) => {
                    if !policy.migration_arguments.is_empty() {
                        return Err(RealmError::InvalidTransactionalStateExtension);
                    }
                    self.stage_transactional_state_reload(extension)?;
                }
                None => {
                    self.stage_reload(&policy.migration_arguments)?;
                }
            }
            let function = self.resolve_staged_cell_function(candidate, entrypoint)?;
            self.spawn_staged_cell_task(candidate, function, cell_arguments, step)
        })();
        let task = match setup {
            Ok(task) => task,
            Err(error) => {
                return Err(self.rollback_failed_cell_setup(
                    error,
                    heap_checkpoint,
                    session_checkpoint,
                ));
            }
        };
        Ok(StagedCellTransaction {
            realm: self,
            candidate,
            task,
            fuel_slice: step.fuel_slice,
            activation_arguments: policy.activation_arguments,
            activation_fuel: policy.activation_fuel,
            ready: None,
            heap_checkpoint: Some(heap_checkpoint),
            session_checkpoint: Some(session_checkpoint),
            finished: false,
        })
    }

    pub(crate) fn quiesce_reload(&mut self) -> Result<usize, RealmError> {
        let transaction = self.reload.transaction()?;
        let old_module = transaction.old_module;
        let old_id = transaction.old_module_id;
        let old_generation = old_module.raw().generation;
        let old_epoch = transaction.old_epoch;
        let detached_request_count = self.resources.epoch_counts(old_id, old_epoch).requests;
        let tasks = self
            .tasks
            .task_handles()
            .into_iter()
            .filter(|task| {
                self.tasks.task_snapshot(*task).is_ok_and(|snapshot| {
                    snapshot.module_id == old_id
                        && snapshot.module_generation == old_generation
                        && snapshot.module_epoch == old_epoch
                })
            })
            .collect::<Vec<_>>();
        for task in tasks.iter().copied() {
            self.cancel_task(task, CancelReason::ReloadCommit)
                .map_err(RealmError::from)?;
        }
        let transaction = self.reload.transaction_mut()?;
        transaction.cancelled_task_count = tasks.len();
        transaction.detached_request_count = detached_request_count;
        self.last_reload_cancelled_tasks = tasks.len();
        self.last_reload_detached_requests = detached_request_count;
        self.total_reload_cancelled_tasks = self
            .total_reload_cancelled_tasks
            .saturating_add(u64::try_from(tasks.len()).unwrap_or(u64::MAX));
        self.total_reload_detached_requests = self
            .total_reload_detached_requests
            .saturating_add(u64::try_from(detached_request_count).unwrap_or(u64::MAX));
        self.reload.quiesced()?;
        Ok(tasks.len())
    }

    pub(crate) fn stage_reload(
        &mut self,
        arguments: &[RuntimeValue],
    ) -> Result<Option<RuntimeValue>, RealmError> {
        self.last_migration_hash = None;
        let candidate_handle = self.reload.transaction()?.candidate;
        let candidate = self
            .modules
            .resolve(candidate_handle.raw())
            .map_err(RealmError::ModuleHandle)?;
        let candidate_domain = candidate.stateful_domain;
        let candidate_schema = candidate.verified.module().state_schema.clone();
        let migration_entry = candidate.verified.module().reload_metadata.migration_entry;
        let old_module = self.reload.transaction()?.old_module;
        let old_root = self
            .modules
            .resolve(old_module.raw())
            .map_err(RealmError::ModuleHandle)?;
        let schema_unchanged = old_root.verified.module().state_schema == candidate_schema;
        let old_state = old_root.state.clone();
        for point in [
            crate::RuntimeFailurePoint::MigrationObjectSlot,
            crate::RuntimeFailurePoint::MigrationFieldSlot,
            crate::RuntimeFailurePoint::MigrationForwardingSlot,
        ] {
            self.fail_if_injected(point)?;
        }
        let mut migration = crate::stateful::MigrationContext::new(
            old_state,
            candidate_domain,
            candidate_schema,
            schema_unchanged,
            self.migration_limits,
        )?;
        let Some(migration_entry) = migration_entry else {
            return self.stage_reload_without_entry(candidate_handle, migration);
        };
        self.last_migration_usage_report = Some(crate::MigrationUsageReport::default());
        let function = self
            .modules
            .resolve(candidate_handle.raw())
            .map_err(RealmError::ModuleHandle)?
            .verified
            .module()
            .functions
            .get(migration_entry as usize)
            .ok_or(RealmError::MissingModule(migration_entry))?;
        if function.effect != nexa_bytecode::FunctionEffect::Migration {
            return Err(ReloadError::Migration(
                "migration entry does not have Migration effect".into(),
            )
            .into());
        }
        let execution = CheckedInterpreter::run_migration(
            &candidate.verified,
            migration_entry,
            arguments,
            self.migration_limits.max_fuel,
            crate::FrameLimits {
                max_call_depth: u32::from(self.migration_limits.max_call_depth),
                ..crate::FrameLimits::default()
            },
            &mut migration,
        );
        self.last_migration_usage_report = Some(migration.usage_report());
        if let Some(error) = migration.limit_error() {
            return Err(ReloadError::MigrationLimit(error).into());
        }
        let execution = match execution {
            Err(InterpreterError::ContinuationLimit(crate::FrameError::CallDepthLimit)) => {
                return Err(ReloadError::MigrationLimit(MigrationLimitError::CallDepth).into());
            }
            Err(error) => return Err(error.into()),
            Ok(execution) => execution,
        };
        match execution {
            InterpreterOutcome::Returned { value, .. } => {
                let (migrated, hash, usage) = migration.finish()?.into_shared();
                if migrated_graph_has_invalid_gc_root(&self.heap, &migrated) {
                    return Err(ReloadError::GraphCheck.into());
                }
                self.last_migration_usage_report = Some(usage);
                self.last_migration_hash = Some(hash);
                self.modules
                    .resolve_mut(candidate_handle.raw())
                    .map_err(RealmError::ModuleHandle)?
                    .state = migrated;
                self.reload.staged()?;
                Ok(value)
            }
            InterpreterOutcome::Suspended { reason, .. } => {
                if reason == SuspendReason::Fuel {
                    Err(ReloadError::MigrationLimit(MigrationLimitError::Fuel).into())
                } else {
                    Err(ReloadError::Migration("migration attempted to suspend".into()).into())
                }
            }
            InterpreterOutcome::HostPending { .. } => {
                Err(ReloadError::Migration("migration attempted a host call".into()).into())
            }
            InterpreterOutcome::Trapped { trap, .. } => {
                Err(ReloadError::Migration(trap.message).into())
            }
        }
    }

    fn stage_transactional_state_reload(
        &mut self,
        extension: TransactionalStateExtension,
    ) -> Result<Option<RuntimeValue>, RealmError> {
        self.last_migration_hash = None;
        self.last_migration_usage_report = Some(crate::MigrationUsageReport::default());
        let transaction = self.reload.transaction()?;
        let old_root = self
            .modules
            .resolve(transaction.old_module.raw())
            .map_err(RealmError::ModuleHandle)?;
        let old_schema = old_root.verified.module().state_schema.clone();
        let mut staged_state = (*old_root.state).clone();
        let candidate_schema = self
            .modules
            .resolve(transaction.candidate.raw())
            .map_err(RealmError::ModuleHandle)?
            .verified
            .module()
            .state_schema
            .clone();
        staged_state.stage_transactional_schema_extension(
            extension.environment(),
            &old_schema,
            &candidate_schema,
        )?;
        if migrated_graph_has_invalid_gc_root(&self.heap, &staged_state) {
            return Err(ReloadError::GraphCheck.into());
        }
        self.modules
            .resolve_mut(transaction.candidate.raw())
            .map_err(RealmError::ModuleHandle)?
            .state = Arc::new(staged_state);
        self.reload.staged()?;
        Ok(None)
    }

    fn stage_reload_without_entry(
        &mut self,
        candidate: ModuleHandle,
        migration: crate::stateful::MigrationContext,
    ) -> Result<Option<RuntimeValue>, RealmError> {
        let (migrated, hash, usage) = migration.finish()?.into_shared();
        if migrated_graph_has_invalid_gc_root(&self.heap, &migrated) {
            return Err(ReloadError::GraphCheck.into());
        }
        self.last_migration_usage_report = Some(usage);
        self.last_migration_hash = Some(hash);
        self.modules
            .resolve_mut(candidate.raw())
            .map_err(RealmError::ModuleHandle)?
            .state = migrated;
        self.reload.staged()?;
        Ok(None)
    }

    fn preflight_staged_activation(
        &mut self,
        activation_arguments: &[RuntimeValue],
        activation_fuel: u64,
    ) -> Result<(), RealmError> {
        let candidate = self.reload.transaction()?.candidate;
        let verified = self
            .modules
            .resolve(candidate.raw())
            .map_err(RealmError::ModuleHandle)?
            .verified
            .clone();
        if self
            .failure_injector
            .trigger(crate::RuntimeFailurePoint::ActivationTrap)
        {
            return Err(RealmError::InjectedFailure(
                crate::RuntimeFailurePoint::ActivationTrap,
            ));
        }
        let Some(activation_entry) = verified.module().reload_metadata.activation_entry else {
            return Ok(());
        };
        let function = verified
            .module()
            .functions
            .get(activation_entry as usize)
            .ok_or(ReloadError::Activation(RuntimeMessage::Static(
                "activation function is missing",
            )))?;
        if function.effect != FunctionEffect::Immediate {
            return Err(ReloadError::Activation(RuntimeMessage::Static(
                "activation entry must have Immediate effect",
            ))
            .into());
        }
        let activation = CheckedInterpreter::run_with_heap(
            &verified,
            activation_entry,
            activation_arguments,
            activation_fuel,
            &mut self.heap,
        )
        .map_err(|_| {
            ReloadError::Activation(RuntimeMessage::Static("activation interpreter failed"))
        })?;
        match activation {
            InterpreterOutcome::Returned { .. } => Ok(()),
            InterpreterOutcome::Trapped { trap, .. } => {
                Err(ReloadError::Activation(trap.message).into())
            }
            InterpreterOutcome::Suspended { .. } | InterpreterOutcome::HostPending { .. } => {
                Err(ReloadError::Activation(RuntimeMessage::Static(
                    "activation entry attempted to suspend",
                ))
                .into())
            }
        }
    }

    fn commit_staged_cell(
        &mut self,
        activation_arguments: &[RuntimeValue],
        activation_fuel: u64,
    ) -> Result<ModuleHandle, RealmError> {
        self.validate_staged_cell_state()?;
        self.preflight_staged_activation(activation_arguments, activation_fuel)?;
        let candidate = self.reload.transaction()?.candidate;
        self.publish_reload_root()?;
        self.reload.activation_succeeded()?;
        self.modules
            .resolve_mut(candidate.raw())
            .map_err(RealmError::ModuleHandle)?
            .lifecycle = ModuleLifecycle::Active;
        let transaction = self.reload.finish()?;
        debug_assert_eq!(transaction.candidate, candidate);
        self.retire_published_old_root(&transaction)?;
        Ok(candidate)
    }

    fn validate_staged_cell_state(&self) -> Result<(), RealmError> {
        let candidate = self.reload.transaction()?.candidate;
        let root = self
            .modules
            .resolve(candidate.raw())
            .map_err(RealmError::ModuleHandle)?;
        root.state
            .validate_transactional_state(&root.verified.module().state_schema)?;
        if migrated_graph_has_invalid_gc_root(&self.heap, &root.state) {
            return Err(ReloadError::GraphCheck.into());
        }
        Ok(())
    }

    fn commit_reload_measured(
        &mut self,
        activation_arguments: &[RuntimeValue],
        activation_fuel: u64,
    ) -> CommitReloadMeasurement {
        let commit_started = Instant::now();
        let prepared = (|| {
            let candidate = self.reload.transaction()?.candidate;
            let verified = self
                .modules
                .resolve(candidate.raw())
                .map_err(RealmError::ModuleHandle)?
                .verified
                .clone();
            let activation_entry = verified.module().reload_metadata.activation_entry;
            self.publish_reload_root()?;
            Ok::<_, RealmError>((candidate, verified, activation_entry))
        })();
        let (candidate, verified, activation_entry) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                return CommitReloadMeasurement {
                    result: Err(error),
                    commit_duration: commit_started.elapsed(),
                    activation_duration: Duration::ZERO,
                };
            }
        };
        let commit_duration = commit_started.elapsed();
        let activation_started = Instant::now();
        let result = (|| {
            if self
                .failure_injector
                .trigger(crate::RuntimeFailurePoint::ActivationTrap)
            {
                self.reload.activation_failed()?;
                self.modules
                    .resolve_mut(candidate.raw())
                    .map_err(RealmError::ModuleHandle)?
                    .lifecycle = ModuleLifecycle::ActivationFaulted;
                return Err(RealmError::InjectedFailure(
                    crate::RuntimeFailurePoint::ActivationTrap,
                ));
            }
            let activation_result: Result<(), RuntimeMessage> = (|| {
                let Some(activation_entry) = activation_entry else {
                    return Ok(());
                };
                let function = verified
                    .module()
                    .functions
                    .get(activation_entry as usize)
                    .ok_or(RuntimeMessage::Static("activation function is missing"))?;
                if function.effect != nexa_bytecode::FunctionEffect::Immediate {
                    return Err(RuntimeMessage::Static(
                        "activation entry must have Immediate effect",
                    ));
                }
                match CheckedInterpreter::run_with_heap(
                    &verified,
                    activation_entry,
                    activation_arguments,
                    activation_fuel,
                    &mut self.heap,
                )
                .map_err(|_| RuntimeMessage::Static("activation interpreter failed"))?
                {
                    InterpreterOutcome::Returned { .. } => Ok(()),
                    InterpreterOutcome::Trapped { trap, .. } => Err(trap.message),
                    InterpreterOutcome::Suspended { .. }
                    | InterpreterOutcome::HostPending { .. } => Err(RuntimeMessage::Static(
                        "activation entry attempted to suspend",
                    )),
                }
            })();
            match crate::invoke_reload_activation(|| activation_result) {
                Ok(()) => {
                    self.reload.activation_succeeded()?;
                    self.modules
                        .resolve_mut(candidate.raw())
                        .map_err(RealmError::ModuleHandle)?
                        .lifecycle = ModuleLifecycle::Active;
                    let transaction = self.reload.finish()?;
                    self.retire_published_old_root(&transaction)?;
                    Ok(transaction.candidate)
                }
                Err(ReloadError::Activation(error)) => {
                    self.reload.activation_failed()?;
                    self.modules
                        .resolve_mut(candidate.raw())
                        .map_err(RealmError::ModuleHandle)?
                        .lifecycle = ModuleLifecycle::ActivationFaulted;
                    Err(ReloadError::Activation(error).into())
                }
                Err(error) => Err(error.into()),
            }
        })();
        CommitReloadMeasurement {
            result,
            commit_duration,
            activation_duration: activation_started.elapsed(),
        }
    }

    pub(crate) fn rollback_reload(&mut self) -> Result<(), RealmError> {
        self.reload.rollback()?;
        let transaction = self.reload.finish()?;
        self.modules
            .release(transaction.candidate.raw())
            .map_err(RealmError::ModuleHandle)?;
        Ok(())
    }

    fn cleanup_staged_cell(
        &mut self,
        task: Option<TaskHandle>,
        cancel_reason: CancelReason,
    ) -> Result<(), RealmError> {
        let mut first_error = None;
        if let Some(task) = task {
            if self.terminal_record(task).is_none()
                && self.tasks.task_snapshot(task).is_ok()
                && let Err(error) = self.cancel_task(task, cancel_reason)
            {
                first_error = Some(RealmError::Runtime(error));
            }
            if let Err(error) = self.drain_host_completions()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
            let _ = self.take_terminal_record(task);
        } else if let Err(error) = self.drain_host_completions() {
            first_error = Some(error);
        }
        self.flush_releases();
        if self.reload.active()
            && let Err(error) = self.rollback_reload()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        self.flush_releases();
        if let Err(error) = self.collect_garbage()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if self.reload.active() && first_error.is_none() {
            first_error = Some(RealmError::Reload(ReloadError::InvalidState));
        }
        first_error.map_or(Ok(()), Err)
    }

    fn publish_reload_root(&mut self) -> Result<(), RealmError> {
        let transaction = self.reload.transaction()?;
        let old = transaction.old_module;
        let candidate = transaction.candidate;
        if self.active_root != Some(old) {
            return Err(ReloadError::InvalidState.into());
        }
        let publication_id = self.next_publication_id;
        let next_publication_id = publication_id
            .checked_add(1)
            .ok_or(RealmError::EpochExhausted)?;
        let candidate_epoch = self.active_module_epoch(candidate)?;
        self.reload.publish()?;
        self.set_active_root(candidate);
        self.next_publication_id = next_publication_id;
        if self.root_publications.len() == self.root_publications.capacity() {
            self.root_publications.pop_front();
        }
        self.root_publications.push_back(RootPublicationRecord {
            publication_id,
            old_root: old,
            candidate_root: candidate,
            candidate_epoch,
        });
        self.reload.begin_activation()?;
        self.modules
            .resolve_mut(candidate.raw())
            .map_err(RealmError::ModuleHandle)?
            .lifecycle = ModuleLifecycle::Activating;
        self.modules
            .resolve_mut(old.raw())
            .map_err(RealmError::ModuleHandle)?
            .lifecycle = ModuleLifecycle::Retired;
        Ok(())
    }

    fn retire_published_old_root(
        &mut self,
        transaction: &ReloadTransaction,
    ) -> Result<(), RealmError> {
        let publication_id = self
            .root_publications
            .back()
            .filter(|publication| {
                publication.old_root == transaction.old_module
                    && publication.candidate_root == transaction.candidate
            })
            .map(|publication| publication.publication_id)
            .ok_or(ReloadError::InvalidState)?;
        let old_root = self
            .modules
            .release(transaction.old_module.raw())
            .map_err(RealmError::ModuleHandle)?;
        self.retired_modules.push(RetiredModuleRecord {
            module_id: old_root.module_id,
            epoch: old_root.epoch,
            committed_at_publication: publication_id,
            released: true,
        });
        Ok(())
    }

    fn rollback_activation_fault(
        &mut self,
        candidate: ModuleHandle,
        heap_checkpoint: crate::heap::HeapCheckpoint,
    ) -> Result<(), RealmError> {
        let transaction = self.reload.finish()?;
        if transaction.candidate != candidate || self.active_root != Some(candidate) {
            return Err(ReloadError::InvalidState.into());
        }
        let publication_id = self
            .root_publications
            .back()
            .filter(|publication| {
                publication.old_root == transaction.old_module
                    && publication.candidate_root == transaction.candidate
            })
            .map(|publication| publication.publication_id)
            .ok_or(ReloadError::InvalidState)?;
        self.set_active_root(transaction.old_module);
        self.modules
            .resolve_mut(transaction.old_module.raw())
            .map_err(RealmError::ModuleHandle)?
            .lifecycle = ModuleLifecycle::Active;
        let candidate_root = self
            .modules
            .release(candidate.raw())
            .map_err(RealmError::ModuleHandle)?;
        self.retired_modules.push(RetiredModuleRecord {
            module_id: candidate_root.module_id,
            epoch: candidate_root.epoch,
            committed_at_publication: publication_id,
            released: true,
        });
        self.heap.restore_checkpoint(heap_checkpoint);
        self.collect_garbage()?;
        Ok(())
    }

    fn set_active_root(&mut self, root: ModuleHandle) {
        self.active_root = Some(root);
    }

    /// H1: bounded retention so the pool never outlives its usefulness.
    const CONTINUATION_POOL_LIMIT: usize = 16;

    fn recycle_continuation_storage(&mut self, storage: crate::FrameArena) {
        if self.continuation_pool.len() < Self::CONTINUATION_POOL_LIMIT {
            self.continuation_pool.push(storage);
        }
    }

    /// H1 observability: retired continuation arenas currently pooled.
    #[must_use]
    pub fn continuation_pool_depth(&self) -> usize {
        self.continuation_pool.len()
    }

    fn spawn_task_inner(
        &mut self,
        module: ModuleHandle,
        function: u32,
        arguments: &[RuntimeValue],
        config: StepConfig,
    ) -> Result<TaskHandle, RealmError> {
        if self
            .reload
            .transaction()
            .is_ok_and(|transaction| transaction.old_module == module)
        {
            return Err(ReloadError::InvalidState.into());
        }
        let loaded = self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?;
        if loaded.lifecycle != ModuleLifecycle::Active {
            return Err(RealmError::ModuleNotCallable);
        }
        self.admit_task(module, function, arguments, config)
    }

    fn spawn_staged_cell_task(
        &mut self,
        module: ModuleHandle,
        function: u32,
        arguments: &[RuntimeValue],
        config: StepConfig,
    ) -> Result<TaskHandle, RealmError> {
        let transaction = self.reload.transaction()?;
        if transaction.candidate != module || self.active_root != Some(transaction.old_module) {
            return Err(ReloadError::InvalidState.into());
        }
        let loaded = self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?;
        if loaded.lifecycle != ModuleLifecycle::Staging {
            return Err(RealmError::ModuleNotCallable);
        }
        self.admit_task(module, function, arguments, config)
    }

    fn resolve_staged_cell_function(
        &self,
        module: ModuleHandle,
        entrypoint: &TransactionalCellEntrypoint,
    ) -> Result<u32, RealmError> {
        let transaction = self.reload.transaction()?;
        if transaction.candidate != module || self.active_root != Some(transaction.old_module) {
            return Err(ReloadError::InvalidState.into());
        }
        let loaded = self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?;
        if loaded.lifecycle != ModuleLifecycle::Staging {
            return Err(RealmError::ModuleNotCallable);
        }
        let export = loaded
            .verified
            .module()
            .exports
            .iter()
            .find(|export| export.stable_id == entrypoint.stable_id)
            .ok_or(RealmError::MissingTransactionalCellExport(
                entrypoint.stable_id,
            ))?;
        if export.signature != entrypoint.signature {
            return Err(RealmError::TransactionalCellSignatureMismatch(
                entrypoint.stable_id,
            ));
        }
        if export.effect != entrypoint.effect() {
            return Err(RealmError::TransactionalCellEffectMismatch {
                stable_id: entrypoint.stable_id,
                actual: export.effect,
            });
        }
        let function = loaded
            .verified
            .module()
            .functions
            .get(export.function as usize)
            .ok_or(RealmError::MissingTransactionalCellExport(
                entrypoint.stable_id,
            ))?;
        if function.signature != entrypoint.signature {
            return Err(RealmError::TransactionalCellSignatureMismatch(
                entrypoint.stable_id,
            ));
        }
        if function.effect != entrypoint.effect() {
            return Err(RealmError::TransactionalCellEffectMismatch {
                stable_id: entrypoint.stable_id,
                actual: function.effect,
            });
        }
        Ok(export.function)
    }

    fn admit_task(
        &mut self,
        module: ModuleHandle,
        function: u32,
        arguments: &[RuntimeValue],
        config: StepConfig,
    ) -> Result<TaskHandle, RealmError> {
        let loaded = self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?;
        let verified = Arc::clone(&loaded.verified);
        let epoch = loaded.epoch;
        let reservation = reservation_for_module(&verified, config.limits.frames);
        // H1: reuse pooled continuation storage when its retained
        // capacities satisfy this module's reservation; the constructor
        // falls back to a fresh reservation otherwise.
        let continuation = crate::InterpreterContinuation::new_with_storage(
            &verified,
            function,
            arguments,
            config.limits.frames,
            reservation,
            self.continuation_pool.pop(),
        )?;
        let task = self.tasks.admit_task(config.owner, epoch, true)?;
        if let Err(error) = self.tasks.attach_continuation(
            task,
            config.priority,
            FuelState::new(config.fuel_slice, 0, config.cumulative_budget),
            continuation,
            module.raw(),
            config.limits,
        ) {
            self.tasks.finish_task(task)?;
            return Err(error.into());
        }
        self.scheduler.schedule(task, config.priority);
        Ok(task)
    }

    pub fn spawn_task(
        &mut self,
        module: ModuleHandle,
        export: StableId,
        arguments: &[RuntimeValue],
        config: StepConfig,
    ) -> Result<TaskHandle, RuntimeError> {
        let function = self
            .resolve_dynamic_export_index(module, export)
            .map_err(RuntimeError::from)?;
        self.spawn_task_inner(module, function, arguments, config)
            .map_err(RuntimeError::from)
    }

    fn resolve_dynamic_export_index(
        &self,
        module: ModuleHandle,
        stable_id: StableId,
    ) -> Result<u32, RealmError> {
        let loaded = self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?;
        let export = loaded
            .verified
            .module()
            .exports
            .iter()
            .find(|candidate| candidate.stable_id == stable_id)
            .ok_or(RealmError::MissingScriptExport(stable_id))?;
        let function = loaded
            .verified
            .module()
            .functions
            .get(export.function as usize)
            .ok_or(RealmError::ScriptExportMetadataMismatch(stable_id))?;
        if function.signature != export.signature || function.effect != export.effect {
            return Err(RealmError::ScriptExportMetadataMismatch(stable_id));
        }
        if matches!(
            function.effect,
            FunctionEffect::Migration | FunctionEffect::Cleanup
        ) {
            return Err(RealmError::ScriptExportNotCallable(stable_id));
        }
        Ok(export.function)
    }

    fn resolve_export_index<E: crate::ScriptExport>(
        &self,
        module: ModuleHandle,
    ) -> Result<u32, crate::ScriptCallError> {
        self.resolve_export::<E>(module)
            .map(|(function, _)| function)
    }

    /// Resolves one export and reports the module function's verified
    /// effect. The declared effect check uses the WP89 satisfaction rule:
    /// a module may strengthen an Ordinary declaration to `@immediate` -
    /// the ABI is identical and the rights are strictly narrower (the
    /// verifier rejects suspension points inside Immediate functions) -
    /// so callers compiled against the Ordinary declaration stay correct
    /// while the effect-aware paths may settle the call task-free.
    fn resolve_export<E: crate::ScriptExport>(
        &self,
        module: ModuleHandle,
    ) -> Result<(u32, FunctionEffect), crate::ScriptCallError> {
        let loaded = self
            .modules
            .resolve(module.raw())
            .map_err(|error| crate::ScriptCallError::Runtime(format!("{error:?}")))?;
        let export = loaded
            .verified
            .module()
            .exports
            .iter()
            .find(|candidate| candidate.stable_id == E::STABLE_ID)
            .ok_or(crate::ScriptCallError::MissingExport {
                name: E::NAME,
                stable_id: E::STABLE_ID,
            })?;
        if export.signature != E::signature() {
            return Err(crate::ScriptCallError::SignatureMismatch { name: E::NAME });
        }
        if !export_effect_satisfies(export.effect, E::effect()) {
            return Err(crate::ScriptCallError::EffectMismatch { name: E::NAME });
        }
        let function = loaded
            .verified
            .module()
            .functions
            .get(
                usize::try_from(export.function)
                    .map_err(|_| crate::ScriptCallError::SignatureMismatch { name: E::NAME })?,
            )
            .ok_or(crate::ScriptCallError::SignatureMismatch { name: E::NAME })?;
        if function.effect != export.effect {
            return Err(crate::ScriptCallError::EffectMismatch { name: E::NAME });
        }
        if matches!(
            function.effect,
            nexa_bytecode::FunctionEffect::Migration | nexa_bytecode::FunctionEffect::Cleanup
        ) {
            return Err(crate::ScriptCallError::EffectNotCallable { name: E::NAME });
        }
        Ok((export.function, function.effect))
    }

    pub fn spawn_export<E: crate::ScriptExport>(
        &mut self,
        module: ModuleHandle,
        args: &E::Args,
        config: StepConfig,
    ) -> Result<TaskHandle, crate::ScriptCallError> {
        let function = self.resolve_export_index::<E>(module)?;
        let requirements = E::argument_requirements(args)?;
        let values = {
            let mut writer = crate::ScriptCallWriter::new(&mut self.heap, requirements)
                .map_err(|_| crate::ScriptCallError::ArgumentEncoding)?;
            let values = E::encode_args(&mut writer, args)?;
            writer
                .commit_arguments(values)
                .map_err(|_| crate::ScriptCallError::ArgumentEncoding)?
        };
        self.spawn_task_inner(module, function, &values, config)
            .map_err(|error| crate::ScriptCallError::Runtime(error.to_string()))
    }

    pub fn call_export<E: crate::ScriptExport>(
        &mut self,
        module: ModuleHandle,
        owner: ScopeHandle,
        args: &E::Args,
        policy: crate::MustCompletePolicy,
    ) -> Result<E::Output, crate::ScriptCallError> {
        self.call_export_metered::<E>(module, owner, args, policy)
            .map(|(output, _)| output)
    }

    pub fn call_export_metered<E: crate::ScriptExport>(
        &mut self,
        module: ModuleHandle,
        owner: ScopeHandle,
        args: &E::Args,
        policy: crate::MustCompletePolicy,
    ) -> Result<(E::Output, ExecutionCharge), crate::ScriptCallError> {
        let task = self.spawn_export::<E>(
            module,
            args,
            StepConfig {
                owner,
                priority: 1,
                fuel_slice: policy.fuel,
                cumulative_budget: policy.cumulative_budget,
                limits: crate::TaskLimits::default(),
            },
        )?;
        match self.poll_task(task, policy.fuel) {
            Ok(TaskPoll::Completed(value)) => {
                let reader = crate::ScriptOutputReader::new(&self.heap);
                let output = E::decode_output(&reader, value);
                let charge = self
                    .take_terminal_record(task)
                    .map_or_else(ExecutionCharge::default, |record| record.final_charge);
                Ok((output?, charge))
            }
            Ok(TaskPoll::Yielded(_)) => {
                let _ = self.cancel_task(task, CancelReason::HostCancelled);
                let _ = self.take_terminal_record(task);
                Err(crate::ScriptCallError::HandlerDidNotComplete)
            }
            Ok(TaskPoll::Waiting(_)) => {
                let _ = self.cancel_task(task, CancelReason::HostCancelled);
                let _ = self.take_terminal_record(task);
                Err(crate::ScriptCallError::HostWaitNotAllowed)
            }
            Ok(TaskPoll::Trapped(error)) => self
                .take_terminal_record(task)
                .and_then(|record| match record.reason {
                    TaskTerminalReason::Trapped(trap) => Some(trap),
                    _ => None,
                })
                .map_or_else(
                    || Err(crate::ScriptCallError::Runtime(error.to_string())),
                    |trap| Err(crate::ScriptCallError::HandlerTrapped(Box::new(trap))),
                ),
            Err(error) => Err(crate::ScriptCallError::Runtime(error.to_string())),
            Ok(TaskPoll::Cancelled(reason)) => {
                let _ = self.take_terminal_record(task);
                Err(crate::ScriptCallError::Runtime(format!(
                    "handler cancelled: {reason:?}"
                )))
            }
        }
    }

    /// WP89: calls an `@immediate` export straight through the predecoded
    /// interpreter. Fuel, traps, the script source stack, permissions,
    /// and GC roots apply exactly as on the Task path, but no Task,
    /// scheduler token, or tombstone is ever created: the continuation
    /// storage cycles through the realm pool (H1) and the call settles in
    /// one poll. Only exports declared `@immediate` qualify; every other
    /// effect keeps the full lifecycle path.
    pub fn call_export_immediate<E: crate::ScriptExport>(
        &mut self,
        module: ModuleHandle,
        args: &E::Args,
        policy: crate::MustCompletePolicy,
    ) -> Result<(E::Output, ExecutionCharge), crate::ScriptCallError> {
        let (function, effect) = self.resolve_export::<E>(module)?;
        if effect != FunctionEffect::Immediate {
            return Err(crate::ScriptCallError::EffectNotCallable { name: E::NAME });
        }
        let requirements = E::argument_requirements(args)?;
        let values = {
            let mut writer = crate::ScriptCallWriter::new(&mut self.heap, requirements)
                .map_err(|_| crate::ScriptCallError::ArgumentEncoding)?;
            let values = E::encode_args(&mut writer, args)?;
            writer
                .commit_arguments(values)
                .map_err(|_| crate::ScriptCallError::ArgumentEncoding)?
        };
        let loaded = self
            .modules
            .resolve(module.raw())
            .map_err(|error| crate::ScriptCallError::Runtime(format!("{error:?}")))?;
        let verified = Arc::clone(&loaded.verified);
        let executable = Arc::clone(&loaded.executable);
        let limits = crate::TaskLimits::default();
        if let Some(outcome) = crate::CheckedInterpreter::try_run_static_leaf(
            &verified,
            function,
            &values,
            FuelState::new(policy.fuel, 0, policy.cumulative_budget),
            &self.cost_table,
            &mut self.heap,
            &executable,
        )
        .map_err(|error| crate::ScriptCallError::Runtime(error.to_string()))?
        {
            return match outcome.result {
                Ok(value) => {
                    let reader = crate::ScriptOutputReader::new(&self.heap);
                    let output =
                        E::decode_output(&reader, value.unwrap_or(crate::RuntimeValue::Unit))?;
                    Ok((output, outcome.charge))
                }
                Err(trap) => Err(crate::ScriptCallError::HandlerTrapped(trap)),
            };
        }
        let reservation = reservation_for_module(&verified, limits.frames);
        let continuation = crate::InterpreterContinuation::new_with_storage(
            &verified,
            function,
            &values,
            limits.frames,
            reservation,
            self.continuation_pool.pop(),
        )
        .map_err(|error| crate::ScriptCallError::Runtime(error.to_string()))?;
        let mut recycled = None;
        let outcome = crate::CheckedInterpreter::poll_recycling(
            &verified,
            continuation,
            FuelState::new(policy.fuel, 0, policy.cumulative_budget),
            &self.cost_table,
            Some(&mut self.heap),
            Some(&executable),
            &mut recycled,
        );
        if let Some(storage) = recycled {
            self.recycle_continuation_storage(storage);
        }
        match outcome.map_err(|error| crate::ScriptCallError::Runtime(error.to_string()))? {
            InterpreterOutcome::Returned { value, charge, .. } => {
                let reader = crate::ScriptOutputReader::new(&self.heap);
                let output = E::decode_output(&reader, value.unwrap_or(crate::RuntimeValue::Unit))?;
                Ok((output, charge))
            }
            InterpreterOutcome::Trapped { trap, .. } => {
                Err(crate::ScriptCallError::HandlerTrapped(Box::new(trap)))
            }
            // The verifier rejects suspension points inside Immediate
            // functions; this arm is a defensive seal, not a reachable
            // path.
            InterpreterOutcome::Suspended { .. } | InterpreterOutcome::HostPending { .. } => {
                Err(crate::ScriptCallError::HandlerDidNotComplete)
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn poll_task_raw(
        &mut self,
        task: TaskHandle,
        fuel_slice: u64,
    ) -> Result<PollResult<Option<RuntimeValue>>, RealmError> {
        if self
            .tombstones
            .iter()
            .any(|(terminal_task, _)| *terminal_task == task)
        {
            return Err(RealmError::TerminalTask);
        }
        self.scheduler.deschedule(task);
        let snapshot = self.tasks.task_snapshot(task)?;
        crate::allocation::record(
            if snapshot.fuel.cumulative_used == 0 {
                crate::allocation::AllocationPhase::FirstSlice
            } else {
                crate::allocation::AllocationPhase::Resume
            },
            0,
        );
        match snapshot.state {
            TaskState::Ready => self.tasks.poll_task(task)?,
            TaskState::FuelYielded => self.tasks.resume_fuel_task(task)?,
            TaskState::ExplicitYielded => self.tasks.resume_explicit_task(task)?,
            TaskState::Waiting => return Ok(PollResult::Pending(PendingReason::HostRequest)),
            TaskState::Running => {}
            _ => return Err(RealmError::TerminalTask),
        }
        let execution = self.tasks.take_execution(task)?;
        let continuation = match execution {
            TaskExecution::Ready(continuation)
            | TaskExecution::Running(continuation)
            | TaskExecution::FuelYielded(continuation)
            | TaskExecution::ExplicitYielded(continuation)
            | TaskExecution::Cancelling(continuation)
            | TaskExecution::Cleanup(continuation) => continuation,
            TaskExecution::Waiting {
                continuation,
                request,
                destination,
                expected_type,
                async_result,
            } => {
                self.tasks.put_execution(
                    task,
                    TaskExecution::Waiting {
                        continuation,
                        request,
                        destination,
                        expected_type,
                        async_result,
                    },
                    snapshot.fuel,
                )?;
                return Err(RealmError::TaskWaiting);
            }
        };
        let fuel = FuelState::new(
            fuel_slice,
            snapshot.fuel.cumulative_used,
            snapshot.fuel.cumulative_limit,
        );
        let trace_start = self.trace_cursor();
        let module_raw = RawHandle::new(
            self.realm_id,
            snapshot.module_id,
            snapshot.module_generation,
        );
        let module = self
            .modules
            .resolve_mut(module_raw)
            .map_err(RealmError::ModuleHandle)?;
        if module.module_id != snapshot.module_id || module.epoch != snapshot.module_epoch {
            return Err(RealmError::MissingModule(snapshot.module_id));
        }
        let verified = Arc::clone(&module.verified);
        let executable = Arc::clone(&module.executable);
        let mut state_bridge = RealmStateBridge {
            registry: Arc::make_mut(&mut module.state),
        };
        let mut recycled_storage: Option<crate::FrameArena> = None;
        let outcome = if let Some(registry) = self.host_registry.as_deref_mut() {
            let mut bridge = RealmHostBridge {
                registry,
                resources: &mut self.resources,
                task,
                module_id: snapshot.module_id,
                epoch: snapshot.module_epoch,
                function_slots: &module.host_function_slots,
            };
            CheckedInterpreter::poll_with_host_heap_and_state(
                &verified,
                continuation,
                fuel,
                &self.cost_table,
                &mut bridge,
                &mut state_bridge,
                &mut self.heap,
                Some(&executable),
                Some(&mut recycled_storage),
            )?
        } else {
            CheckedInterpreter::poll_with_heap_and_state(
                &verified,
                continuation,
                fuel,
                &self.cost_table,
                &mut state_bridge,
                &mut self.heap,
                Some(&executable),
                Some(&mut recycled_storage),
            )?
        };
        // H1: terminal polls hand the arena storage back; suspensions never
        // set the slot. Pooled storage feeds the next task admission.
        if let Some(storage) = recycled_storage {
            self.recycle_continuation_storage(storage);
        }
        match outcome {
            InterpreterOutcome::Returned {
                value,
                charge,
                fuel,
            } => {
                let final_charge = self.tasks.record_charge(task, charge)?;
                self.finish_task(
                    task,
                    snapshot.module_epoch,
                    trace_start,
                    final_charge,
                    value,
                )?;
                let _ = fuel;
                Ok(PollResult::Completed {
                    value,
                    charge: final_charge,
                })
            }
            InterpreterOutcome::Suspended {
                continuation,
                reason,
                charge,
                fuel,
            } => {
                let final_charge = self.tasks.record_charge(task, charge)?;
                crate::allocation::record(crate::allocation::AllocationPhase::Promotion, 0);
                if continuation.cumulative_exhausted() {
                    let script_call_stack =
                        crate::ScriptCallStack::from_continuation(&verified, &continuation);
                    self.tasks
                        .put_execution(task, TaskExecution::Running(continuation), fuel)?;
                    if let Some(trap) = self.cancel_task_internal(
                        task,
                        CancelReason::BudgetExceeded,
                        snapshot.module_epoch,
                        trace_start,
                        final_charge,
                        Some(script_call_stack),
                    )? {
                        return Ok(PollResult::Trapped(trap));
                    }
                    return Ok(PollResult::Cancelled(CancelReason::BudgetExceeded));
                }
                let pending = match reason {
                    SuspendReason::Fuel => PendingReason::Fuel,
                    SuspendReason::ExplicitYield => PendingReason::ExplicitYield,
                    SuspendReason::HostRequest => PendingReason::HostRequest,
                };
                let execution = match reason {
                    SuspendReason::Fuel => {
                        self.tasks.yield_fuel_task(task)?;
                        TaskExecution::FuelYielded(continuation)
                    }
                    SuspendReason::ExplicitYield => {
                        self.tasks.yield_explicit_task(task)?;
                        TaskExecution::ExplicitYielded(continuation)
                    }
                    SuspendReason::HostRequest => {
                        return Err(RealmError::TaskWaiting);
                    }
                };
                self.tasks.put_execution(task, execution, fuel)?;
                self.scheduler.schedule(task, snapshot.priority);
                Ok(PollResult::Pending(pending))
            }
            InterpreterOutcome::HostPending {
                continuation,
                request,
                destination,
                expected_type,
                async_result,
                charge,
                fuel,
            } => {
                self.tasks.record_charge(task, charge)?;
                self.tasks.await_task(task)?;
                self.tasks.put_execution(
                    task,
                    TaskExecution::Waiting {
                        continuation,
                        request,
                        destination,
                        expected_type,
                        async_result,
                    },
                    fuel,
                )?;
                self.scheduler.wait_for(request, task);
                Ok(PollResult::Pending(PendingReason::HostRequest))
            }
            InterpreterOutcome::Trapped {
                mut trap, charge, ..
            } => {
                let final_charge = self.tasks.record_charge(task, charge)?;
                trap.attach_runtime_context(
                    RawHandle::new(
                        self.realm_id,
                        snapshot.module_id,
                        snapshot.module_generation,
                    ),
                    snapshot.module_epoch,
                    task.raw(),
                );
                self.trap_task(
                    task,
                    snapshot.module_epoch,
                    trace_start,
                    final_charge,
                    trap.clone(),
                )?;
                Ok(PollResult::Trapped(trap))
            }
        }
    }

    pub fn poll_task(
        &mut self,
        task: TaskHandle,
        fuel_slice: u64,
    ) -> Result<TaskPoll, RuntimeError> {
        let result = self
            .poll_task_raw(task, fuel_slice)
            .map_err(classify_task_handle_error)
            .map_err(RuntimeError::from)?;
        Ok(match result {
            PollResult::Completed { value, .. } => {
                TaskPoll::Completed(value.unwrap_or(RuntimeValue::Unit))
            }
            PollResult::Pending(PendingReason::Fuel) => TaskPoll::Yielded(YieldReason::Fuel),
            PollResult::Pending(PendingReason::ExplicitYield) => {
                TaskPoll::Yielded(YieldReason::Explicit)
            }
            PollResult::Pending(PendingReason::HostRequest) => {
                let request = match self.tasks.execution(task)? {
                    TaskExecution::Waiting { request, .. } => *request,
                    _ => return Err(RuntimeError::from(RealmError::TaskWaiting)),
                };
                TaskPoll::Waiting(request)
            }
            PollResult::Cancelled(reason) => TaskPoll::Cancelled(reason),
            PollResult::Trapped(trap) => {
                TaskPoll::Trapped(RuntimeError::Trap(crate::RuntimeTrap::from(&trap)))
            }
        })
    }

    pub fn cancel_scope(&mut self, scope: ScopeHandle) -> Result<usize, RealmError> {
        self.tasks.cancel_scope(scope)?;
        self.tasks.begin_scope_cancellation(scope)?;
        let tasks = self
            .tasks
            .task_handles()
            .into_iter()
            .filter(|task| {
                self.tasks
                    .task_snapshot(*task)
                    .is_ok_and(|snapshot| snapshot.owner == scope)
            })
            .collect::<Vec<_>>();
        for task in &tasks {
            self.cancel_task(*task, CancelReason::ScopeCancelled)?;
        }
        self.tasks.finish_scope_cancellation(scope)?;
        Ok(tasks.len())
    }

    pub fn cancel_task(
        &mut self,
        task: TaskHandle,
        reason: CancelReason,
    ) -> Result<TaskPoll, RuntimeError> {
        let snapshot = self
            .tasks
            .task_snapshot(task)
            .map_err(RealmError::from)
            .map_err(classify_task_handle_error)
            .map_err(RuntimeError::from)?;
        if snapshot.state == TaskState::Ready {
            self.tasks.poll_task(task)?;
        }
        let trace_start = self.trace_cursor();
        let trap = self
            .cancel_task_internal(
                task,
                reason,
                snapshot.module_epoch,
                trace_start,
                snapshot.charge,
                None,
            )
            .map_err(RuntimeError::from)?;
        Ok(trap.map_or(TaskPoll::Cancelled(reason), |trap| {
            TaskPoll::Trapped(RuntimeError::Trap(crate::RuntimeTrap::from(&trap)))
        }))
    }

    pub fn complete_request(
        &mut self,
        request: HostRequestHandle,
        result: HostResult,
    ) -> Result<CompletionDisposition, RuntimeError> {
        self.resources
            .complete_request(request, result)
            .map_err(RealmError::from)
            .map_err(RuntimeError::from)?;
        let delivered = self.drain_host_completions().map_err(RuntimeError::from)? != 0;
        Ok(if delivered {
            CompletionDisposition::Delivered
        } else {
            CompletionDisposition::Discarded
        })
    }

    pub fn abandon_request(&mut self, request: HostRequestHandle) -> Result<(), RuntimeError> {
        self.resources
            .complete_request(request, HostCompletionResult::Abandoned)
            .map_err(RealmError::from)
            .map_err(RuntimeError::from)?;
        let _ = self.drain_host_completions().map_err(RuntimeError::from)?;
        Ok(())
    }

    pub fn tick(&mut self, budget: TickBudget) -> Result<TickReport, RealmError> {
        let completions = self.drain_host_completions()?;
        let mut report = TickReport::default();
        for _ in 0..budget.max_tasks {
            let Some(task) = self.scheduler.pop_ready() else {
                break;
            };
            match self.poll_task_raw(task, budget.frame_fuel_budget) {
                Ok(PollResult::Completed { .. }) => report.completed += 1,
                Ok(PollResult::Cancelled(_)) => report.cancelled += 1,
                Ok(PollResult::Trapped(_)) => report.trapped += 1,
                Ok(PollResult::Pending(_))
                | Err(RealmError::TerminalTask | RealmError::TaskWaiting) => {}
                Err(error) => return Err(error),
            }
            report.polled += 1;
        }
        report.releases = self.flush_releases();
        if budget.collect_garbage {
            report.collection = Some(self.collect_garbage()?);
        }
        let _ = completions;
        Ok(report)
    }

    pub fn collect_garbage(&mut self) -> Result<CollectionStats, RealmError> {
        let roots = self.gc_roots()?;
        Ok(self.heap.collect(&roots)?)
    }

    /// Precise root snapshot shared by full and incremental collection:
    /// suspended task continuations, retained terminal values, module
    /// globals, stateful registries, and host staging roots.
    fn gc_roots(&mut self) -> Result<GcRoots, RealmError> {
        let mut roots = GcRoots::default();
        for task in self.tasks.task_handles() {
            let snapshot = self.tasks.task_snapshot(task)?;
            let module = self.module_for_task(snapshot)?;
            let continuation = self.tasks.execution(task)?.continuation();
            roots
                .suspended_tasks
                .extend(continuation.checked_gc_roots(&module.verified)?);
        }
        roots
            .suspended_tasks
            .extend(self.tombstones.iter().filter_map(|(_, record)| {
                let TaskTerminalReason::Completed(Some(value)) = &record.reason else {
                    return None;
                };
                terminal_value_gc_root(*value)
            }));
        for raw in self.modules.occupied_handles() {
            let module = self
                .modules
                .resolve(raw)
                .expect("occupied module handle resolves");
            roots.module_globals.extend_from_slice(&module.globals);
            roots.stateful_registry.extend(module.state.gc_roots());
            roots.staging_heap.extend_from_slice(&module.staging_roots);
        }
        Ok(roots)
    }

    /// One budgeted incremental collection step over the realm's precise
    /// roots (G2). Ordinary gameplay drives cycles through this entry;
    /// explicit full collection stays available for tests, inspection,
    /// and shutdown.
    pub fn collect_garbage_incremental(
        &mut self,
        budget: GcBudget,
    ) -> Result<IncrementalGcReport, RealmError> {
        let roots = self.gc_roots()?;
        let report = self.heap.collect_incremental(&roots, budget)?;
        if report.completed.is_some() {
            self.gc_cycle_baseline = self.heap.vm_allocation_counters().object_allocations;
        }
        Ok(report)
    }

    /// Water-mark trigger (G2): advances an active cycle, or starts one
    /// when live slots pass 3/4 of the ceiling or allocations since the
    /// last completed cycle exceed half the ceiling. Returns `None` when
    /// the heap is idle and no trigger fires.
    pub fn maybe_collect_garbage_incremental(
        &mut self,
        budget: GcBudget,
    ) -> Result<Option<IncrementalGcReport>, RealmError> {
        let ceiling = self.heap.max_objects() as usize;
        let cycle_active = self.heap.gc_phase() != GcPhase::Idle;
        let live_pressure = self.heap.live_len().saturating_mul(4) >= ceiling.saturating_mul(3);
        let allocated_since = self
            .heap
            .vm_allocation_counters()
            .object_allocations
            .saturating_sub(self.gc_cycle_baseline);
        let allocation_pressure = allocated_since >= (ceiling as u64).div_ceil(2);
        if !(cycle_active || live_pressure || allocation_pressure) {
            return Ok(None);
        }
        self.collect_garbage_incremental(budget).map(Some)
    }

    pub fn allocate(&mut self, object: Object) -> Result<GcRef, RealmError> {
        Ok(self.heap.allocate(object)?)
    }

    pub fn allocate_array(
        &mut self,
        type_id: StableId,
        element_type: ValueType,
    ) -> Result<RuntimeValue, RealmError> {
        Ok(self.heap.allocate_array(type_id, element_type)?)
    }

    pub fn allocate_class(
        &mut self,
        type_id: StableId,
        fields: &[RuntimeValue],
    ) -> Result<RuntimeValue, RealmError> {
        Ok(self.heap.allocate_class(type_id, fields)?)
    }

    pub fn allocate_map(
        &mut self,
        type_id: StableId,
        key_type: ValueType,
        value_type: ValueType,
    ) -> Result<RuntimeValue, RealmError> {
        Ok(self.heap.allocate_map(type_id, key_type, value_type)?)
    }

    pub fn array_length(&self, value: RuntimeValue) -> Result<usize, RealmError> {
        Ok(self.heap.array_len(value)?)
    }

    pub fn map_length(&self, value: RuntimeValue) -> Result<usize, RealmError> {
        Ok(self.heap.map_len(value)?)
    }

    pub fn allocate_buffer(
        &mut self,
        type_id: StableId,
        element_type: ValueType,
        values: &[RuntimeValue],
    ) -> Result<RuntimeValue, RealmError> {
        Ok(self.heap.allocate_buffer(type_id, element_type, values)?)
    }

    pub fn resolve_heap_object(&self, reference: GcRef) -> Result<&Object, RealmError> {
        Ok(self.heap.resolve(reference)?)
    }

    /// K5 companion to [`Self::resolve_heap_object`]: struct and class
    /// fields live in the collection arena, so read-only inspection of
    /// them resolves through the heap instead of destructuring an inline
    /// array out of the object header.
    pub fn resolve_heap_fields(
        &self,
        reference: GcRef,
    ) -> Result<crate::CollectionView<'_>, RealmError> {
        Ok(self.heap.object_fields(reference)?)
    }

    #[allow(dead_code)]
    pub(crate) fn create_host_request_for_runtime(
        &mut self,
        task: TaskHandle,
    ) -> Result<PendingHostRequest, RealmError> {
        self.require_host_capabilities()?;
        self.require_reload_idle()?;
        let snapshot = self.tasks.task_snapshot(task)?;
        Ok(self
            .resources
            .context(task, snapshot.module_id, snapshot.module_epoch)
            .create_request()?)
    }

    #[allow(dead_code)]
    pub(crate) fn wait_for_request(
        &mut self,
        task: TaskHandle,
        request: HostRequestHandle,
    ) -> Result<(), RealmError> {
        if !self.resources.owns_request(task, request) {
            return Err(RealmError::Host(HostRequestError::InvalidState));
        }
        let snapshot = self.tasks.task_snapshot(task)?;
        match snapshot.state {
            TaskState::Ready => self.tasks.poll_task(task)?,
            TaskState::FuelYielded => self.tasks.resume_fuel_task(task)?,
            TaskState::ExplicitYielded => self.tasks.resume_explicit_task(task)?,
            TaskState::Running => {}
            _ => return Err(RealmError::TaskWaiting),
        }
        let execution = self.tasks.take_execution(task)?;
        let continuation = match execution {
            TaskExecution::Ready(continuation)
            | TaskExecution::Running(continuation)
            | TaskExecution::FuelYielded(continuation)
            | TaskExecution::ExplicitYielded(continuation) => continuation,
            other => {
                self.tasks.put_execution(task, other, snapshot.fuel)?;
                return Err(RealmError::TaskWaiting);
            }
        };
        self.tasks.await_task(task)?;
        self.tasks.put_execution(
            task,
            TaskExecution::Waiting {
                continuation,
                request,
                destination: 0,
                expected_type: None,
                async_result: None,
            },
            snapshot.fuel,
        )?;
        self.scheduler.deschedule(task);
        self.scheduler.wait_for(request, task);
        Ok(())
    }

    pub fn with_resource_context<T>(
        &mut self,
        task: TaskHandle,
        operation: impl FnOnce(&mut crate::ResourceContext<'_>) -> T,
    ) -> Result<T, RealmError> {
        self.require_host_capabilities()?;
        self.require_reload_idle()?;
        let snapshot = self.tasks.task_snapshot(task)?;
        let mut context = self
            .resources
            .context(task, snapshot.module_id, snapshot.module_epoch);
        Ok(operation(&mut context))
    }

    pub fn create_resource_token(
        &mut self,
        task: TaskHandle,
        content_type: StableId,
        domain: RuntimeHostDomain,
    ) -> Result<ResourceTokenHandle, RealmError> {
        self.require_host_capabilities()?;
        self.require_reload_idle()?;
        let snapshot = self.tasks.task_snapshot(task)?;
        Ok(self
            .resources
            .context(task, snapshot.module_id, snapshot.module_epoch)
            .create_token(content_type, domain)?)
    }

    pub fn release_resource_token(
        &mut self,
        task: TaskHandle,
        token: ResourceTokenHandle,
    ) -> Result<(), RealmError> {
        self.resources.release_token(task, token)?;
        Ok(())
    }

    pub fn create_typed_snapshot(
        &mut self,
        task: TaskHandle,
        encoded: crate::EncodedSnapshot,
    ) -> Result<SnapshotHandle, RealmError> {
        self.require_host_capabilities()?;
        self.require_reload_idle()?;
        let snapshot = self.tasks.task_snapshot(task)?;
        Ok(self
            .resources
            .context(task, snapshot.module_id, snapshot.module_epoch)
            .create_typed_snapshot(encoded)?)
    }

    pub fn release_snapshot(
        &mut self,
        task: TaskHandle,
        snapshot: SnapshotHandle,
    ) -> Result<(), RealmError> {
        self.resources.release_snapshot(task, snapshot)?;
        Ok(())
    }

    pub fn snapshot_payload(&self, snapshot: SnapshotHandle) -> Result<&[u8], RealmError> {
        Ok(self.resources.snapshot_payload(snapshot)?)
    }

    pub fn snapshot_layout(
        &self,
        snapshot: SnapshotHandle,
    ) -> Result<crate::SnapshotLayout, RealmError> {
        Ok(self.resources.snapshot_layout(snapshot)?)
    }

    pub fn snapshot_view<'a, T>(&'a self, snapshot: SnapshotHandle) -> Result<T, RealmError>
    where
        T: crate::DecodeTypedSnapshot<'a>,
    {
        if snapshot.type_id() != T::TYPE_ID
            || self.snapshot_content_type(snapshot)? != T::CONTENT_TYPE
        {
            return Err(crate::HostRequestError::InvalidState.into());
        }
        let layout = self.snapshot_layout(snapshot)?;
        if layout.schema_hash != T::SCHEMA_HASH
            || layout.alignment != T::ALIGNMENT
            || layout.size as usize != self.snapshot_payload(snapshot)?.len()
        {
            return Err(crate::HostRequestError::InvalidState.into());
        }
        T::decode(crate::TypedSnapshotRef {
            payload: self.snapshot_payload(snapshot)?,
            layout,
        })
        .map_err(|_| crate::HostRequestError::InvalidState.into())
    }

    pub fn snapshot_external_bytes(&self, snapshot: SnapshotHandle) -> Result<usize, RealmError> {
        Ok(self.resources.snapshot_external_bytes(snapshot)?)
    }

    pub fn snapshot_content_type(&self, snapshot: SnapshotHandle) -> Result<StableId, RealmError> {
        Ok(self.resources.snapshot_content_type(snapshot)?)
    }

    pub fn attach_module_root(
        &mut self,
        module: ModuleHandle,
        root: GcRef,
    ) -> Result<(), RealmError> {
        self.heap.resolve(root)?;
        let module = self
            .modules
            .resolve_mut(module.raw())
            .map_err(RealmError::ModuleHandle)?;
        if !module.globals.contains(&root) {
            module.globals.push(root);
        }
        Ok(())
    }

    pub fn drop_module_root(
        &mut self,
        module: ModuleHandle,
        root: GcRef,
    ) -> Result<(), RealmError> {
        let module = self
            .modules
            .resolve_mut(module.raw())
            .map_err(RealmError::ModuleHandle)?;
        let index = module
            .globals
            .iter()
            .position(|candidate| *candidate == root)
            .ok_or(RealmError::Heap(HeapError::InvalidReference(root)))?;
        module.globals.swap_remove(index);
        Ok(())
    }

    #[must_use]
    pub const fn discarded_late_host_results(&self) -> u64 {
        self.resources.discarded_late_results()
    }

    #[must_use]
    pub fn completion_accounting(&self) -> crate::CompletionAccounting {
        self.resources.completion_accounting()
    }

    #[must_use]
    pub fn resource_snapshot(&self) -> crate::RuntimeResourceSnapshot {
        self.resources.model_snapshot()
    }

    /// Cumulative VM allocation/copy counters of this Realm's heap (WP13).
    #[must_use]
    pub const fn vm_allocation_counters(&self) -> crate::VmAllocationCounters {
        self.heap.vm_allocation_counters()
    }

    #[must_use]
    pub fn resource_ledger(&self) -> crate::RuntimeResourceLedger {
        let (tasks, scopes, continuations) = self.tasks.ledger_counts();
        let mut ledger = self.resources.resource_ledger();
        ledger.tasks = crate::ledger::count(tasks);
        ledger.scopes = crate::ledger::count(scopes);
        ledger.continuations = crate::ledger::count(continuations);
        ledger.scheduler_tokens = crate::ledger::count(self.scheduler.token_count());
        ledger.heap_objects = crate::ledger::count(self.heap.live_len());
        ledger.state_objects = crate::ledger::count(
            self.modules
                .occupied_handles_iter()
                .filter_map(|handle| self.modules.resolve(handle).ok())
                .map(|root| root.state.object_count())
                .sum(),
        );
        ledger.retired_modules = crate::ledger::count(
            self.retired_modules
                .entries
                .iter()
                .filter(|record| !record.released)
                .count(),
        );
        ledger
    }

    #[cfg(any(test, feature = "model-adapter"))]
    #[must_use]
    pub fn inspection_snapshot(&self) -> RealmInspectionSnapshot {
        let modules = self
            .modules
            .occupied_handles_iter()
            .filter_map(|raw| self.module_inspection(ModuleHandle(raw)))
            .collect::<Vec<_>>();
        let active_root = self
            .active_root
            .and_then(|module| self.module_inspection(module));
        let transaction = self.reload.transaction().ok();
        let candidate_root =
            transaction.and_then(|transaction| self.module_inspection(transaction.candidate));
        let tasks = self
            .tasks
            .task_handles_iter()
            .filter_map(|task| self.task_inspection(task))
            .collect::<Vec<_>>();
        let roots = RootInspection {
            module_globals: modules.iter().map(|module| module.module_gc_roots).sum(),
            stateful_registry: modules.iter().map(|module| module.state_gc_roots).sum(),
            staging_heap: modules.iter().map(|module| module.staging_gc_roots).sum(),
            suspended_tasks: self.suspended_task_root_count(),
            published_roots: self.root_publications.len(),
        };
        let reload = ReloadInspection {
            state: self.reload_inspection_state(),
            old_module: transaction.map(|transaction| transaction.old_module),
            candidate_module: transaction.map(|transaction| transaction.candidate),
            cancelled_tasks: transaction.map_or(self.last_reload_cancelled_tasks, |transaction| {
                transaction.cancelled_task_count
            }),
            detached_requests: transaction
                .map_or(self.last_reload_detached_requests, |transaction| {
                    transaction.detached_request_count
                }),
            total_cancelled_tasks: self.total_reload_cancelled_tasks,
            total_detached_requests: self.total_reload_detached_requests,
            late_completions_discarded: self.resources.discarded_late_results(),
            root_publications: self.root_publications.iter().copied().collect(),
        };
        RealmInspectionSnapshot {
            active_root,
            candidate_root,
            modules,
            retired_modules: self.retired_modules.entries.iter().copied().collect(),
            tasks,
            terminal_tasks: self.terminal_task_inspections(),
            resources: self.resource_ledger(),
            completion_accounting: self.completion_accounting(),
            reload,
            roots,
            heap: HeapInspection {
                live_objects: self.heap.live_len(),
                capacity: self.heap.capacity_limit(),
            },
            runtime_host: self.runtime_host.as_ref().map(RuntimeHost::state),
            runtime_host_releases: self
                .runtime_host
                .as_ref()
                .map_or_else(Vec::new, RuntimeHost::inspection_releases),
            terminal_records: self.tombstones.iter().cloned().collect(),
        }
    }

    #[cfg(any(test, feature = "model-adapter"))]
    fn module_inspection(&self, handle: ModuleHandle) -> Option<ModuleInspection> {
        let module = self.modules.resolve(handle.raw()).ok()?;
        Some(ModuleInspection {
            handle,
            generation: handle.raw().generation,
            module_id: module.module_id,
            epoch: module.epoch,
            lifecycle: module.lifecycle,
            stateful_domain: module.stateful_domain,
            state_objects: module.state.object_count(),
            module_gc_roots: module.globals.len(),
            state_gc_roots: module.state.gc_roots().len(),
            staging_gc_roots: module.staging_roots.len(),
        })
    }

    #[cfg(any(test, feature = "model-adapter"))]
    fn task_inspection(&self, handle: TaskHandle) -> Option<TaskInspection> {
        let task = self.tasks.task_snapshot(handle).ok()?;
        let execution = match self.tasks.execution(handle).ok()? {
            TaskExecution::Ready(_) => TaskExecutionInspection::Ready,
            TaskExecution::Running(_) => TaskExecutionInspection::Running,
            TaskExecution::FuelYielded(_) => TaskExecutionInspection::FuelYielded,
            TaskExecution::ExplicitYielded(_) => TaskExecutionInspection::ExplicitYielded,
            TaskExecution::Waiting {
                request,
                destination,
                ..
            } => TaskExecutionInspection::Waiting {
                request: *request,
                destination: *destination,
            },
            TaskExecution::Cancelling(_) => TaskExecutionInspection::Cancelling,
            TaskExecution::Cleanup(_) => TaskExecutionInspection::Cleanup,
        };
        let scheduler = match self.scheduler.checkpoint(handle) {
            crate::scheduler::SchedulerCheckpoint::Ready { .. } => SchedulerInspection::Ready,
            crate::scheduler::SchedulerCheckpoint::Waiting { request } => {
                SchedulerInspection::Waiting(request)
            }
            crate::scheduler::SchedulerCheckpoint::Detached => SchedulerInspection::Detached,
        };
        Some(TaskInspection {
            handle,
            state: task.state,
            execution,
            scheduler,
            module_id: task.module_id,
            module_generation: task.module_generation,
            epoch: task.module_epoch,
            ownership: self.resources.ownership(handle).unwrap_or_default(),
        })
    }

    #[cfg(any(test, feature = "model-adapter"))]
    fn terminal_task_inspections(&self) -> Vec<TerminalTaskInspection> {
        let mut inspections = self
            .tasks
            .task_handles_iter()
            .filter_map(|task| {
                let snapshot = self.tasks.task_snapshot(task).ok()?;
                let execution = self.tasks.execution(task).ok()?;
                let checkpoint = self.scheduler.checkpoint(task);
                let scheduler_token = match checkpoint {
                    crate::scheduler::SchedulerCheckpoint::Ready { sequence, .. } => Some(sequence),
                    crate::scheduler::SchedulerCheckpoint::Waiting { request } => {
                        Some(handle_identity(request.raw()))
                    }
                    crate::scheduler::SchedulerCheckpoint::Detached => None,
                };
                let request = match execution {
                    TaskExecution::Waiting { request, .. } => Some(*request),
                    _ => None,
                };
                Some(TerminalTaskInspection {
                    task,
                    state: snapshot.state,
                    continuation_id: snapshot.continuation_id,
                    continuation_resume_count: snapshot.continuation_resume_count,
                    scheduler_token,
                    request,
                    terminal_record_count: u32::try_from(
                        self.tombstones
                            .iter()
                            .filter(|(terminal, _)| *terminal == task)
                            .count(),
                    )
                    .unwrap_or(u32::MAX),
                })
            })
            .collect::<Vec<_>>();
        inspections.extend(
            self.tombstones
                .iter()
                .map(|(task, record)| TerminalTaskInspection {
                    task: *task,
                    state: record.state,
                    continuation_id: None,
                    continuation_resume_count: record.continuation_resume_count,
                    scheduler_token: None,
                    request: None,
                    terminal_record_count: 1,
                }),
        );
        inspections
    }

    #[cfg(any(test, feature = "model-adapter"))]
    fn suspended_task_root_count(&self) -> usize {
        self.tasks
            .task_handles_iter()
            .filter_map(|task| {
                let snapshot = self.tasks.task_snapshot(task).ok()?;
                let module = self.module_for_task(snapshot).ok()?;
                let execution = self.tasks.execution(task).ok()?;
                execution
                    .continuation()
                    .checked_gc_roots(&module.verified)
                    .ok()
            })
            .map(|roots| roots.len())
            .sum()
    }

    #[cfg(any(test, feature = "model-adapter"))]
    fn reload_inspection_state(&self) -> ReloadInspectionState {
        use crate::machines::reload::State;
        match self.reload.inspection_state() {
            None => self
                .active_root
                .and_then(|active| self.modules.resolve(active.raw()).ok())
                .filter(|module| module.lifecycle == ModuleLifecycle::ActivationFaulted)
                .map_or(ReloadInspectionState::Idle, |_| {
                    ReloadInspectionState::ActivationFaulted
                }),
            Some(State::Planned) => ReloadInspectionState::Idle,
            Some(State::Preparing) => ReloadInspectionState::Preparing,
            Some(State::Quiescing) => ReloadInspectionState::Quiescing,
            Some(State::Staging) => ReloadInspectionState::Staging,
            Some(State::Committing) => ReloadInspectionState::Committing,
            Some(State::Published) => ReloadInspectionState::Published,
            Some(State::Activating) => ReloadInspectionState::Activating,
            Some(State::Completed) => ReloadInspectionState::Completed,
            Some(State::RolledBack) => ReloadInspectionState::RolledBack,
            Some(State::ActivationFaulted) => ReloadInspectionState::ActivationFaulted,
        }
    }

    #[must_use]
    pub fn resource_invariants_hold(&self) -> bool {
        let terminal_tasks_have_no_continuation = self
            .tombstones
            .iter()
            .all(|(task, _)| self.tasks.execution(*task).is_err());
        let waiting_tasks_have_one_request = self.tasks.task_handles_iter().all(|task| {
            let Ok(snapshot) = self.tasks.task_snapshot(task) else {
                return false;
            };
            if snapshot.state != TaskState::Waiting {
                return true;
            }
            let Ok(TaskExecution::Waiting { request, .. }) = self.tasks.execution(task) else {
                return false;
            };
            self.resources.request_count_for_task(task) == 1
                && self.resources.owns_request(task, *request)
        });
        terminal_tasks_have_no_continuation && waiting_tasks_have_one_request
    }

    pub fn task_snapshot(&self, task: TaskHandle) -> Result<crate::TaskSnapshot, RealmError> {
        Ok(self.tasks.task_snapshot(task)?)
    }

    #[must_use]
    pub fn request_terminal_record(
        &self,
        request: HostRequestHandle,
    ) -> Option<&crate::RequestTerminalRecord> {
        self.resources.request_terminal_record(request)
    }

    #[must_use]
    pub fn terminal_record(&self, task: TaskHandle) -> Option<&TaskTerminalRecord> {
        self.tombstones
            .iter()
            .find_map(|(terminal_task, record)| (*terminal_task == task).then_some(record))
    }

    fn take_terminal_record(&mut self, task: TaskHandle) -> Option<TaskTerminalRecord> {
        let position = self
            .tombstones
            .iter()
            .position(|(terminal_task, _)| *terminal_task == task)?;
        self.tombstones.remove(position).map(|(_, record)| record)
    }

    #[must_use]
    pub fn trace(&self) -> &RuntimeTrace {
        self.tasks.trace()
    }

    pub fn set_trace_enabled(&mut self, enabled: bool) {
        self.tasks.set_trace_enabled(enabled);
    }

    fn drain_host_completions(&mut self) -> Result<usize, RealmError> {
        let mut count = 0;
        loop {
            let preflight = match self.resources.peek_completion() {
                Some(delivery) => self.preflight_delivery_writeback(&delivery)?,
                None => None,
            };
            let Some(delivery) = self.resources.pop_completion() else {
                break;
            };
            self.route_host_completion(&delivery, preflight)?;
            count += 1;
        }
        Ok(count)
    }

    fn preflight_delivery_writeback(
        &mut self,
        delivery: &HostCompletionDelivery,
    ) -> Result<Option<DeliveryWritebackPreflight>, RealmError> {
        if delivery.realm_id != self.realm_id {
            return Err(ReloadError::InvalidState.into());
        }
        let Some(task) = self.scheduler.task_waiting_for(delivery.request) else {
            return Ok(None);
        };
        let snapshot = self.tasks.task_snapshot(task)?;
        if snapshot.state != TaskState::Waiting {
            return Ok(None);
        }
        let result =
            self.preflight_result_writeback(task, delivery.request, snapshot, &delivery.result)?;
        Ok(Some(DeliveryWritebackPreflight {
            task,
            snapshot,
            result,
        }))
    }

    fn route_host_completion(
        &mut self,
        delivery: &HostCompletionDelivery,
        preflight: Option<DeliveryWritebackPreflight>,
    ) -> Result<bool, RealmError> {
        if delivery.realm_id != self.realm_id {
            return Err(ReloadError::InvalidState.into());
        }
        self.deliver_host_completion(delivery.request, preflight)
    }

    fn deliver_host_completion(
        &mut self,
        request: HostRequestHandle,
        preflight: Option<DeliveryWritebackPreflight>,
    ) -> Result<bool, RealmError> {
        let Some(preflight) = preflight else {
            return Ok(false);
        };
        if self.scheduler.task_waiting_for(request) != Some(preflight.task) {
            return Ok(false);
        }
        debug_assert_eq!(self.scheduler.wake_request(request), Some(preflight.task));
        self.commit_result_writeback(
            preflight.task,
            request,
            preflight.snapshot,
            preflight.result,
        )?;
        Ok(true)
    }

    #[allow(clippy::too_many_lines)]
    fn preflight_result_writeback(
        &mut self,
        task: TaskHandle,
        request: HostRequestHandle,
        snapshot: crate::TaskSnapshot,
        result: &HostCompletionResult,
    ) -> Result<ResultWritebackPreflight, RealmError> {
        let (waiting_request, expected_type, async_result) = match self.tasks.execution(task)? {
            TaskExecution::Waiting {
                request,
                expected_type,
                async_result,
                ..
            } => (*request, *expected_type, *async_result),
            _ => return Err(RealmError::TaskWaiting),
        };
        if waiting_request != request {
            return Err(RealmError::TaskWaiting);
        }

        let action = match result {
            HostCompletionResult::Success(payload) => {
                if let Some(result) = async_result {
                    let payload = {
                        let enum_types =
                            &self.module_for_task(snapshot)?.verified.module().enum_types;
                        let struct_types = &self
                            .module_for_task(snapshot)?
                            .verified
                            .module()
                            .struct_types;
                        let array_types = &self
                            .module_for_task(snapshot)?
                            .verified
                            .module()
                            .array_types;
                        let buffer_types = &self
                            .module_for_task(snapshot)?
                            .verified
                            .module()
                            .buffer_types;
                        plan_host_payload(
                            payload,
                            result.success,
                            enum_types,
                            struct_types,
                            array_types,
                            buffer_types,
                        )?
                    };
                    self.preflight_async_result(result, true, payload)?
                } else if let Some(expected_type) = expected_type {
                    let payload = {
                        let enum_types =
                            &self.module_for_task(snapshot)?.verified.module().enum_types;
                        let struct_types = &self
                            .module_for_task(snapshot)?
                            .verified
                            .module()
                            .struct_types;
                        let array_types = &self
                            .module_for_task(snapshot)?
                            .verified
                            .module()
                            .array_types;
                        let buffer_types = &self
                            .module_for_task(snapshot)?
                            .verified
                            .module()
                            .buffer_types;
                        plan_host_payload(
                            payload,
                            expected_type,
                            enum_types,
                            struct_types,
                            array_types,
                            buffer_types,
                        )?
                    };
                    validate_planned_payload(&self.heap, &payload)?;
                    let slots = planned_payload_slots(&payload)?;
                    if slots == 0 {
                        let PlannedResultPayload::Value(value) = payload else {
                            unreachable!("zero-slot planned payload is a runtime value");
                        };
                        ResultWritebackAction::ResumeDirect(value)
                    } else {
                        ResultWritebackAction::ResumeDirectPlanned {
                            payload,
                            heap: self.heap.preflight(slots)?,
                        }
                    }
                } else {
                    ResultWritebackAction::ResumeDirect(RuntimeValue::Unit)
                }
            }
            HostCompletionResult::Error(HostErrorPayload::Code(code)) => match async_result {
                Some(result) => self.preflight_async_error(snapshot, result, *code)?,
                None => ResultWritebackAction::TrapFailure(*code),
            },
            HostCompletionResult::Error(HostErrorPayload::Value(payload)) => {
                let Some(result) = async_result else {
                    return Ok(ResultWritebackPreflight {
                        action: ResultWritebackAction::TrapMessage(
                            "typed Host error requires async Result metadata",
                        ),
                    });
                };
                let payload = {
                    let module = self.module_for_task(snapshot)?.verified.module();
                    plan_host_payload(
                        payload,
                        result.error,
                        &module.enum_types,
                        &module.struct_types,
                        &module.array_types,
                        &module.buffer_types,
                    )?
                };
                self.preflight_async_result(result, false, payload)?
            }
            HostCompletionResult::Cancelled => match async_result {
                Some(result) if result.cancel_policy == CancelPolicy::ReturnError => self
                    .preflight_async_error(
                        snapshot,
                        result,
                        result.cancel_error.ok_or(RealmError::TaskWaiting)?,
                    )?,
                _ => ResultWritebackAction::Cancel,
            },
            HostCompletionResult::Abandoned => match async_result {
                Some(result) if result.abandon_policy == AbandonPolicy::ReturnError => self
                    .preflight_async_error(
                        snapshot,
                        result,
                        result.abandon_error.ok_or(RealmError::TaskWaiting)?,
                    )?,
                _ => ResultWritebackAction::TrapCode {
                    code: DiagnosticCode::new("NX5002"),
                    argument: 0,
                },
            },
        };
        Ok(ResultWritebackPreflight { action })
    }

    #[allow(clippy::cast_precision_loss)]
    fn preflight_async_error(
        &mut self,
        snapshot: crate::TaskSnapshot,
        result: AsyncResultType,
        code: u32,
    ) -> Result<ResultWritebackAction, RealmError> {
        let payload = match result.error {
            ValueType::I32 => PlannedResultPayload::Value(RuntimeValue::I32(i32::from_ne_bytes(
                code.to_ne_bytes(),
            ))),
            ValueType::I64 => PlannedResultPayload::Value(RuntimeValue::I64(i64::from(code))),
            ValueType::F32 => {
                PlannedResultPayload::Value(RuntimeValue::F32((code as f32).to_bits()))
            }
            ValueType::F64 => {
                PlannedResultPayload::Value(RuntimeValue::F64(f64::from(code).to_bits()))
            }
            ValueType::Rune if char::from_u32(code).is_some() => {
                PlannedResultPayload::Value(RuntimeValue::Rune(code))
            }
            ValueType::Rune => {
                return Ok(ResultWritebackAction::TrapMessage(
                    "host error rune is not a Unicode scalar value",
                ));
            }
            ValueType::Named(type_id) => {
                let variant = self
                    .module_for_task(snapshot)?
                    .verified
                    .module()
                    .enum_types
                    .iter()
                    .find(|enum_type| enum_type.type_id == type_id)
                    .and_then(|enum_type| {
                        enum_type
                            .variants
                            .iter()
                            .find(|variant| variant.tag == code)
                    });
                let Some(variant) = variant else {
                    return Ok(ResultWritebackAction::TrapFailure(code));
                };
                PlannedResultPayload::Enum {
                    type_id,
                    variant: variant.stable_id,
                    tag: variant.tag,
                    payload: None,
                }
            }
            ValueType::Bool | ValueType::String | ValueType::Ref => {
                return Ok(ResultWritebackAction::TrapMessage(
                    "host error payload type mismatch",
                ));
            }
        };
        self.preflight_async_result(result, false, payload)
    }

    fn preflight_async_result(
        &mut self,
        result: AsyncResultType,
        success: bool,
        payload: PlannedResultPayload,
    ) -> Result<ResultWritebackAction, RealmError> {
        validate_planned_payload(&self.heap, &payload)?;
        let slots = planned_payload_slots(&payload)?
            .checked_add(1)
            .ok_or(RealmError::Heap(HeapError::CapacityExhausted))?;
        let heap = self.heap.preflight(slots)?;
        Ok(ResultWritebackAction::ResumeAsync {
            result,
            success,
            payload,
            heap,
        })
    }

    fn commit_result_writeback(
        &mut self,
        task: TaskHandle,
        request: HostRequestHandle,
        snapshot: crate::TaskSnapshot,
        preflight: ResultWritebackPreflight,
    ) -> Result<(), RealmError> {
        let value = match preflight.action {
            ResultWritebackAction::ResumeDirect(value) => value,
            ResultWritebackAction::ResumeDirectPlanned { payload, mut heap } => {
                commit_planned_payload(&mut self.heap, &mut heap, payload)?
            }
            ResultWritebackAction::ResumeAsync {
                result,
                success,
                payload,
                mut heap,
            } => {
                let payload = commit_planned_payload(&mut self.heap, &mut heap, payload)?;
                let (variant, tag) = if success {
                    (StableId::from_parts(&["Result", "::Ok"]), 0)
                } else {
                    (StableId::from_parts(&["Result", "::Err"]), 1)
                };
                let reference = self.heap.commit(
                    &mut heap,
                    Object::Enum {
                        type_id: result.result_type,
                        variant,
                        tag,
                        payload: Some(payload),
                    },
                );
                debug_assert!(Heap::reservation_complete(&heap));
                RuntimeValue::NamedRef {
                    reference,
                    type_id: result.result_type,
                }
            }
            ResultWritebackAction::Cancel => {
                return self.cancel_waiting_host_task(task, snapshot);
            }
            ResultWritebackAction::TrapFailure(code) => {
                return self.trap_host_task(
                    task,
                    snapshot,
                    RuntimeMessage::Code {
                        code: DiagnosticCode::new("NX5003"),
                        argument: u64::from(code),
                    },
                );
            }
            ResultWritebackAction::TrapCode { code, argument } => {
                return self.trap_host_task(
                    task,
                    snapshot,
                    RuntimeMessage::Code { code, argument },
                );
            }
            ResultWritebackAction::TrapMessage(message) => {
                return self.trap_host_task(task, snapshot, RuntimeMessage::Static(message));
            }
        };

        let execution = self.tasks.take_execution(task)?;
        let TaskExecution::Waiting {
            mut continuation,
            request: waiting_request,
            destination,
            expected_type,
            ..
        } = execution
        else {
            return Err(RealmError::TaskWaiting);
        };
        if waiting_request != request {
            return Err(RealmError::TaskWaiting);
        }
        continuation.write_resume_value(destination, expected_type, value)?;
        self.tasks.resume_waiting_task(task)?;
        self.tasks
            .put_execution(task, TaskExecution::Running(continuation), snapshot.fuel)?;
        self.scheduler.schedule(task, snapshot.priority);
        Ok(())
    }

    fn cancel_waiting_host_task(
        &mut self,
        task: TaskHandle,
        snapshot: crate::TaskSnapshot,
    ) -> Result<(), RealmError> {
        let _ = self.cancel_task_internal(
            task,
            CancelReason::HostCancelled,
            snapshot.module_epoch,
            self.trace_cursor(),
            snapshot.charge,
            None,
        )?;
        Ok(())
    }

    fn trap_host_task(
        &mut self,
        task: TaskHandle,
        snapshot: crate::TaskSnapshot,
        message: RuntimeMessage,
    ) -> Result<(), RealmError> {
        let module = Arc::clone(&self.module_for_task(snapshot)?.verified);
        let continuation = self.tasks.execution(task)?.continuation();
        let mut trap = Trap::from_continuation(&module, continuation, TrapKind::Host, message);
        trap.attach_runtime_context(
            RawHandle::new(
                self.realm_id,
                snapshot.module_id,
                snapshot.module_generation,
            ),
            snapshot.module_epoch,
            task.raw(),
        );
        self.tasks.request_task_cancel(task)?;
        self.tasks.reach_task_safepoint(task)?;
        self.tasks.mark_execution_cancelling(task)?;
        self.trap_task(
            task,
            snapshot.module_epoch,
            self.trace_cursor(),
            snapshot.charge,
            trap,
        )
    }

    fn module_for_task(
        &self,
        snapshot: crate::TaskSnapshot,
    ) -> Result<&ModuleEpochRoot, RealmError> {
        resolve_task_module(&self.modules, self.realm_id, snapshot)
    }

    fn trace_cursor(&self) -> usize {
        self.tasks.trace().records().last().map_or(0, |record| {
            usize::try_from(record.sequence.saturating_add(1)).unwrap_or(usize::MAX)
        })
    }

    fn finish_task(
        &mut self,
        task: TaskHandle,
        epoch: u64,
        trace_start: usize,
        charge: ExecutionCharge,
        value: Option<RuntimeValue>,
    ) -> Result<(), RealmError> {
        crate::allocation::record(crate::allocation::AllocationPhase::TerminalCleanup, 0);
        let continuation_resume_count = self.tasks.task_snapshot(task)?.continuation_resume_count;
        self.scheduler.cancel_task(task);
        self.resources.cleanup_task(task, false)?;
        self.tasks.finish_task(task)?;
        self.record_terminal(
            task,
            TaskTerminalRecord {
                state: TaskState::Completed,
                reason: TaskTerminalReason::Completed(value),
                script_call_stack: None,
                module_epoch: epoch,
                continuation_resume_count,
                final_charge: charge,
                trace_range: trace_start..self.trace_cursor(),
            },
        );
        Ok(())
    }

    fn trap_task(
        &mut self,
        task: TaskHandle,
        epoch: u64,
        trace_start: usize,
        charge: ExecutionCharge,
        trap: Trap,
    ) -> Result<(), RealmError> {
        let continuation_resume_count = self.tasks.task_snapshot(task)?.continuation_resume_count;
        self.scheduler.cancel_task(task);
        self.resources.cleanup_task(task, false)?;
        self.tasks.trap_task(task)?;
        self.record_terminal(
            task,
            TaskTerminalRecord {
                state: TaskState::Trapped,
                script_call_stack: Some(trap.script_call_stack.clone()),
                reason: TaskTerminalReason::Trapped(trap),
                module_epoch: epoch,
                continuation_resume_count,
                final_charge: charge,
                trace_range: trace_start..self.trace_cursor(),
            },
        );
        Ok(())
    }

    fn cancel_task_internal(
        &mut self,
        task: TaskHandle,
        reason: CancelReason,
        epoch: u64,
        trace_start: usize,
        mut charge: ExecutionCharge,
        script_call_stack: Option<crate::ScriptCallStack>,
    ) -> Result<Option<Trap>, RealmError> {
        self.scheduler.cancel_task(task);
        self.tasks.request_task_cancel(task)?;
        self.tasks.reach_task_safepoint(task)?;
        self.tasks.mark_execution_cancelling(task)?;
        let snapshot = self.tasks.task_snapshot(task)?;
        let has_user_defer = self
            .tasks
            .execution(task)?
            .continuation()
            .arena()
            .defers_rev()
            .next()
            .is_some();
        let run_user_cleanup = reason != CancelReason::ReloadCommit && has_user_defer;
        if run_user_cleanup {
            self.tasks.begin_cleanup(task)?;
            self.tasks.mark_execution_cleanup(task)?;
        }
        let cleanup = if run_user_cleanup {
            let verified = Arc::clone(&self.module_for_task(snapshot)?.verified);
            let continuation = self.tasks.take_execution(task)?.into_continuation();
            if self.failure_injector.trigger_with_context(
                crate::RuntimeFailurePoint::CleanupTrap,
                Some(task),
                None,
            ) {
                Err(crate::Trap::from_continuation(
                    &verified,
                    &continuation,
                    crate::TrapKind::Host,
                    "injected cleanup trap",
                ))
            } else {
                CheckedInterpreter::run_cleanup(
                    &verified,
                    continuation,
                    snapshot.limits.max_cleanup_ops,
                    snapshot.limits.max_cleanup_fuel,
                    &self.cost_table,
                )?
            }
        } else {
            Ok(ExecutionCharge::default())
        };
        self.resources
            .cleanup_task(task, reason == CancelReason::ReloadCommit)?;
        let cleanup_charge = match cleanup {
            Ok(cleanup_charge) => cleanup_charge,
            Err(mut trap) => {
                trap.attach_runtime_context(
                    RawHandle::new(
                        self.realm_id,
                        snapshot.module_id,
                        snapshot.module_generation,
                    ),
                    snapshot.module_epoch,
                    task.raw(),
                );
                self.tasks.trap_task(task)?;
                self.record_terminal(
                    task,
                    TaskTerminalRecord {
                        state: TaskState::Trapped,
                        script_call_stack: Some(trap.script_call_stack.clone()),
                        reason: TaskTerminalReason::Trapped(trap.clone()),
                        module_epoch: epoch,
                        continuation_resume_count: snapshot.continuation_resume_count,
                        final_charge: charge,
                        trace_range: trace_start..self.trace_cursor(),
                    },
                );
                return Ok(Some(trap));
            }
        };
        charge.instructions = charge
            .instructions
            .saturating_add(cleanup_charge.instructions);
        charge.fuel_used = charge.fuel_used.saturating_add(cleanup_charge.fuel_used);
        if run_user_cleanup {
            self.tasks.finish_cleanup(task)?;
        } else {
            self.tasks.finish_cancel_without_cleanup(task)?;
        }
        self.record_terminal(
            task,
            TaskTerminalRecord {
                state: TaskState::Cancelled,
                reason: TaskTerminalReason::Cancelled(reason),
                script_call_stack,
                module_epoch: epoch,
                continuation_resume_count: snapshot.continuation_resume_count,
                final_charge: charge,
                trace_range: trace_start..self.trace_cursor(),
            },
        );
        Ok(None)
    }

    fn record_terminal(&mut self, task: TaskHandle, record: TaskTerminalRecord) {
        if self.tombstone_capacity == 0 {
            return;
        }
        if self.tombstones.len() == self.tombstone_capacity {
            self.tombstones.pop_front();
        }
        self.tombstones.push_back((task, record));
    }

    fn flush_releases(&mut self) -> usize {
        debug_assert!(
            self.runtime_host.is_some()
                || self.resources.model_snapshot() == crate::RuntimeResourceSnapshot::default(),
            "isolated realms cannot own host release records"
        );
        self.runtime_host
            .as_ref()
            .map_or(0, |_| self.resources.transfer_releases_to_host())
    }

    fn require_host_capabilities(&self) -> Result<(), RealmError> {
        self.runtime_host
            .as_ref()
            .map(|_| ())
            .ok_or(RealmError::HostCapabilitiesUnavailable)
    }

    fn fail_if_injected(&self, point: crate::RuntimeFailurePoint) -> Result<(), RealmError> {
        if self.failure_injector.trigger(point) {
            Err(RealmError::InjectedFailure(point))
        } else {
            Ok(())
        }
    }

    fn require_reload_idle(&self) -> Result<(), RealmError> {
        if self.reload.active() {
            Err(ReloadError::InvalidState.into())
        } else {
            Ok(())
        }
    }
}

impl StagedCellTransaction<'_> {
    #[must_use]
    pub const fn candidate(&self) -> ModuleHandle {
        self.candidate
    }

    #[must_use]
    pub const fn task(&self) -> TaskHandle {
        self.task
    }

    #[must_use]
    pub const fn active_root(&self) -> Option<ModuleHandle> {
        self.realm.active_root()
    }

    pub fn candidate_lifecycle(&self) -> Result<ModuleLifecycle, RealmError> {
        self.realm.module_lifecycle(self.candidate)
    }

    #[must_use]
    pub fn output_reader(&self) -> crate::ScriptOutputReader<'_> {
        crate::ScriptOutputReader::new(&self.realm.heap)
    }

    /// Deliver an explicitly completed Host request while the candidate is
    /// staged. Completions submitted by an asynchronous `RuntimeHost` are also
    /// drained automatically by [`Self::poll`].
    pub fn complete_request(
        &mut self,
        request: HostRequestHandle,
        result: HostResult,
    ) -> Result<CompletionDisposition, RuntimeError> {
        self.realm.complete_request(request, result)
    }

    pub fn abandon_request(&mut self, request: HostRequestHandle) -> Result<(), RuntimeError> {
        self.realm.abandon_request(request)
    }

    pub fn poll(&mut self) -> Result<TransactionalCellPoll, TransactionalCellFailure> {
        if self.finished {
            return Err(TransactionalCellFailure {
                cause: TransactionalCellFailureCause::AlreadyFinished,
                rollback_error: None,
            });
        }
        if let Some((value, charge)) = self.ready {
            return Ok(TransactionalCellPoll::ReadyToCommit { value, charge });
        }
        if let Err(error) = self.realm.drain_host_completions() {
            return Err(self.fail(TransactionalCellFailureCause::Runtime(Box::new(
                RuntimeError::from(error),
            ))));
        }
        if let Some(record) = self.realm.terminal_record(self.task).cloned() {
            let cause = match record.reason {
                TaskTerminalReason::Cancelled(reason) => {
                    TransactionalCellFailureCause::Cancelled(reason)
                }
                TaskTerminalReason::Trapped(trap) => TransactionalCellFailureCause::Trapped(
                    Box::new(RuntimeError::Trap(crate::RuntimeTrap::from(&trap))),
                ),
                TaskTerminalReason::Completed(value) => {
                    let value = value.unwrap_or(RuntimeValue::Unit);
                    return self.finish_poll(value, record.final_charge);
                }
            };
            return Err(self.fail(cause));
        }
        match self.realm.poll_task_raw(self.task, self.fuel_slice) {
            Ok(PollResult::Completed { value, charge }) => {
                let value = value.unwrap_or(RuntimeValue::Unit);
                self.finish_poll(value, charge)
            }
            Ok(PollResult::Pending(PendingReason::Fuel)) => {
                Ok(TransactionalCellPoll::Yielded(YieldReason::Fuel))
            }
            Ok(PollResult::Pending(PendingReason::ExplicitYield)) => {
                Ok(TransactionalCellPoll::Yielded(YieldReason::Explicit))
            }
            Ok(PollResult::Pending(PendingReason::HostRequest)) => {
                let request = match self.realm.tasks.execution(self.task) {
                    Ok(TaskExecution::Waiting { request, .. }) => *request,
                    Ok(_) => {
                        return Err(self.fail(TransactionalCellFailureCause::Runtime(Box::new(
                            RuntimeError::from(RealmError::TaskWaiting),
                        ))));
                    }
                    Err(error) => {
                        return Err(self.fail(TransactionalCellFailureCause::Runtime(Box::new(
                            RuntimeError::from(RealmError::Runtime(error)),
                        ))));
                    }
                };
                Ok(TransactionalCellPoll::Waiting(request))
            }
            Ok(PollResult::Cancelled(reason)) => {
                Err(self.fail(TransactionalCellFailureCause::Cancelled(reason)))
            }
            Ok(PollResult::Trapped(trap)) => {
                let error = RuntimeError::Trap(crate::RuntimeTrap::from(&trap));
                Err(self.fail(TransactionalCellFailureCause::Trapped(Box::new(error))))
            }
            Err(error) => Err(self.fail(TransactionalCellFailureCause::Runtime(Box::new(
                RuntimeError::from(error),
            )))),
        }
    }

    fn finish_poll(
        &mut self,
        value: RuntimeValue,
        charge: ExecutionCharge,
    ) -> Result<TransactionalCellPoll, TransactionalCellFailure> {
        if let Err(error) = self.realm.validate_staged_cell_state() {
            return Err(self.fail(TransactionalCellFailureCause::Runtime(Box::new(
                RuntimeError::from(error),
            ))));
        }
        self.ready = Some((value, charge));
        Ok(TransactionalCellPoll::ReadyToCommit { value, charge })
    }

    pub fn commit(mut self) -> Result<TransactionalCellCommit, TransactionalCellFailure> {
        if self.finished {
            return Err(TransactionalCellFailure {
                cause: TransactionalCellFailureCause::AlreadyFinished,
                rollback_error: None,
            });
        }
        let Some((value, charge)) = self.ready else {
            return Err(self.fail(TransactionalCellFailureCause::NotReady));
        };
        let module = match self
            .realm
            .commit_staged_cell(&self.activation_arguments, self.activation_fuel)
        {
            Ok(module) => module,
            Err(error) => {
                return Err(self.fail(TransactionalCellFailureCause::Activation(Box::new(error))));
            }
        };
        let _ = self.realm.take_terminal_record(self.task);
        self.realm.flush_releases();
        let _ = self.heap_checkpoint.take();
        let _ = self.session_checkpoint.take();
        self.finished = true;
        Ok(TransactionalCellCommit {
            module,
            value,
            charge,
        })
    }

    pub fn cancel(
        mut self,
        reason: CancelReason,
    ) -> Result<TransactionalCellRollback, TransactionalCellFailure> {
        let cleanup = self.realm.cleanup_staged_cell(Some(self.task), reason);
        let restore_session = cleanup.is_ok();
        let rollback_error = cleanup.err().map(Box::new);
        self.restore_after_rollback(restore_session);
        self.finished = true;
        if rollback_error.is_some() {
            return Err(TransactionalCellFailure {
                cause: TransactionalCellFailureCause::Cancelled(reason),
                rollback_error,
            });
        }
        Ok(TransactionalCellRollback {
            candidate: self.candidate,
            reason,
        })
    }

    fn fail(&mut self, cause: TransactionalCellFailureCause) -> TransactionalCellFailure {
        let cleanup = self
            .realm
            .cleanup_staged_cell(Some(self.task), CancelReason::HostCancelled);
        let restore_session = cleanup.is_ok();
        let rollback_error = cleanup.err().map(Box::new);
        self.restore_after_rollback(restore_session);
        self.finished = true;
        TransactionalCellFailure {
            cause,
            rollback_error,
        }
    }

    fn restore_after_rollback(&mut self, restore_session: bool) {
        if let Some(checkpoint) = self.heap_checkpoint.take() {
            self.realm.heap.restore_checkpoint(checkpoint);
        }
        if restore_session {
            if let Some(checkpoint) = self.session_checkpoint.take() {
                self.realm
                    .restore_transactional_session_checkpoint(checkpoint);
            }
        } else {
            let _ = self.session_checkpoint.take();
        }
    }
}

impl Drop for StagedCellTransaction<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let cleanup = self
                .realm
                .cleanup_staged_cell(Some(self.task), CancelReason::HostCancelled);
            self.restore_after_rollback(cleanup.is_ok());
            self.finished = true;
        }
    }
}

#[cfg(any(test, feature = "model-adapter"))]
fn handle_identity(raw: RawHandle) -> u64 {
    u64::from(raw.realm_id).rotate_left(41)
        ^ u64::from(raw.index).rotate_left(19)
        ^ u64::from(raw.generation)
}

fn resolve_task_module(
    modules: &SlotPool<ModuleEpochRoot>,
    realm_id: u32,
    snapshot: crate::TaskSnapshot,
) -> Result<&ModuleEpochRoot, RealmError> {
    let raw = RawHandle::new(realm_id, snapshot.module_id, snapshot.module_generation);
    let module = modules.resolve(raw).map_err(RealmError::ModuleHandle)?;
    if module.module_id != snapshot.module_id || module.epoch != snapshot.module_epoch {
        return Err(RealmError::MissingModule(snapshot.module_id));
    }
    Ok(module)
}

impl Drop for RealmRuntime {
    fn drop(&mut self) {
        for task in self.tasks.task_handles_iter() {
            let _ = self.resources.cleanup_task(task, true);
        }
        self.flush_releases();
        if let Some(runtime_host) = &self.runtime_host {
            runtime_host.unregister_realm();
        }
    }
}

fn module_requires_host_capabilities(module: &nexa_bytecode::Module) -> bool {
    if !module.host_imports.is_empty() {
        return true;
    }
    let mut visited = BTreeSet::new();
    let mut requires = |ty| requires_host_capabilities(module, ty, &mut visited);
    module.functions.iter().any(|function| {
        function
            .signature
            .parameters
            .iter()
            .copied()
            .any(&mut requires)
            || function.signature.result.is_some_and(&mut requires)
    }) || module.exports.iter().any(|export| {
        export
            .signature
            .parameters
            .iter()
            .copied()
            .any(&mut requires)
            || export.signature.result.is_some_and(&mut requires)
    }) || module.enum_types.iter().any(|enum_type| {
        enum_type
            .variants
            .iter()
            .filter_map(|variant| variant.payload_type)
            .any(&mut requires)
    }) || module
        .state_schema
        .types
        .iter()
        .any(|state_type| state_type.fields.iter().any(|field| requires(field.ty)))
}

fn validate_transactional_state_transition(
    old: &nexa_bytecode::Module,
    candidate: &nexa_bytecode::Module,
    environment: StableId,
) -> Result<(), RealmError> {
    let [old_type] = old.state_schema.types.as_slice() else {
        return Err(RealmError::InvalidTransactionalStateExtension);
    };
    let [candidate_type] = candidate.state_schema.types.as_slice() else {
        return Err(RealmError::InvalidTransactionalStateExtension);
    };
    if old_type.stable_id != environment
        || candidate_type.stable_id != environment
        || candidate_type.version != old_type.version
        || candidate_type.fields.len() <= old_type.fields.len()
        || candidate_type.fields[..old_type.fields.len()] != old_type.fields[..]
        || candidate.reload_metadata.migration_entry.is_some()
        || candidate.reload_metadata.activation_entry.is_some()
        || candidate
            .functions
            .iter()
            .any(|function| function.effect == FunctionEffect::Migration)
    {
        return Err(RealmError::InvalidTransactionalStateExtension);
    }
    Ok(())
}

fn migrated_graph_has_invalid_gc_root(heap: &Heap, state: &StatefulRegistry) -> bool {
    state
        .gc_roots()
        .into_iter()
        .any(|reference| heap.resolve(reference).is_err())
}

fn migration_requirement_error(
    available: MigrationLimits,
    required: MigrationLimitRequirements,
) -> Option<MigrationLimitError> {
    available.validate_requirements(required).err()
}

fn requires_host_capabilities(
    module: &nexa_bytecode::Module,
    ty: ValueType,
    visited: &mut BTreeSet<StableId>,
) -> bool {
    let ValueType::Named(type_id) = ty else {
        return false;
    };
    if [
        StableId::from_name("HostRequest"),
        StableId::from_name("HostError"),
        StableId::from_name("Buffer"),
    ]
    .contains(&type_id)
        || module
            .resource_token_types
            .iter()
            .any(|token| token.type_id == type_id)
        || module
            .snapshot_types
            .iter()
            .any(|snapshot| snapshot.type_id == type_id)
    {
        return true;
    }
    if !visited.insert(type_id) {
        return false;
    }
    if let Some(enum_type) = module
        .enum_types
        .iter()
        .find(|enum_type| enum_type.type_id == type_id)
        && enum_type
            .variants
            .iter()
            .filter_map(|variant| variant.payload_type)
            .any(|payload| requires_host_capabilities(module, payload, visited))
    {
        return true;
    }
    if let Some(struct_type) = module
        .struct_types
        .iter()
        .find(|struct_type| struct_type.type_id == type_id)
        && struct_type
            .fields
            .iter()
            .any(|field| requires_host_capabilities(module, field.ty, visited))
    {
        return true;
    }
    if let Some(class_type) = module
        .class_types
        .iter()
        .find(|class_type| class_type.type_id == type_id)
        && class_type
            .fields
            .iter()
            .any(|field| requires_host_capabilities(module, field.ty, visited))
    {
        return true;
    }
    module
        .state_schema
        .types
        .iter()
        .find(|state_type| state_type.stable_id == type_id)
        .is_some_and(|state_type| {
            state_type
                .fields
                .iter()
                .any(|field| requires_host_capabilities(module, field.ty, visited))
        })
}

/// WP89 effect satisfaction: an export satisfies the caller's declared
/// effect when they match exactly, or when the module strengthened an
/// Ordinary declaration to Immediate. Immediate keeps the synchronous
/// single-poll ABI while granting strictly fewer rights (the verifier
/// rejects suspension points inside Immediate functions), so every caller
/// compiled against the Ordinary declaration remains sound. No other
/// effect pair is substitutable.
fn export_effect_satisfies(found: FunctionEffect, declared: FunctionEffect) -> bool {
    found == declared
        || (declared == FunctionEffect::Ordinary && found == FunctionEffect::Immediate)
}

fn reservation_for_module(
    module: &VerifiedModule,
    limits: crate::FrameLimits,
) -> ContinuationReservation {
    let max_registers = module
        .module()
        .functions
        .iter()
        .map(|function| u32::from(function.registers))
        .max()
        .unwrap_or(0);
    let max_depth = module
        .module()
        .functions
        .iter()
        .map(|function| u32::from(function.max_static_call_depth))
        .max()
        .unwrap_or(1)
        .max(1);
    ContinuationReservation {
        frame_capacity: limits.max_call_depth.min(max_depth),
        register_capacity: u32::try_from(
            limits.max_frame_bytes / std::mem::size_of::<RuntimeValue>(),
        )
        .unwrap_or(u32::MAX)
        .min(max_registers.saturating_mul(max_depth)),
        defer_capacity: limits.max_defer_records,
    }
}

const fn terminal_value_gc_root(value: RuntimeValue) -> Option<GcRef> {
    match value {
        RuntimeValue::String { reference, .. }
        | RuntimeValue::Struct { reference, .. }
        | RuntimeValue::Ref(reference)
        | RuntimeValue::NamedRef { reference, .. } => Some(reference),
        RuntimeValue::I32(_)
        | RuntimeValue::I64(_)
        | RuntimeValue::F32(_)
        | RuntimeValue::F64(_)
        | RuntimeValue::Bool(_)
        | RuntimeValue::Rune(_)
        | RuntimeValue::HostRequest(_)
        | RuntimeValue::ResourceToken(_)
        | RuntimeValue::Snapshot(_)
        | RuntimeValue::Opaque { .. }
        | RuntimeValue::MigrationOldObject(_)
        | RuntimeValue::MigrationStagingObject(_)
        | RuntimeValue::StateHandle { .. }
        | RuntimeValue::Unit => None,
    }
}

fn classify_task_handle_error(error: RealmError) -> RealmError {
    match error {
        RealmError::Runtime(RuntimeError::Task(crate::TaskError::Handle(
            crate::HandleError::WrongRealm { .. },
        ))) => RealmError::CrossRealmTaskHandle,
        RealmError::Runtime(RuntimeError::Task(crate::TaskError::Handle(
            crate::HandleError::StaleGeneration { .. }
            | crate::HandleError::Vacant { .. }
            | crate::HandleError::Retired { .. }
            | crate::HandleError::OutOfRange { .. },
        ))) => RealmError::StaleTaskHandle,
        error => error,
    }
}

fn restart_reload_error(error: RealmError) -> ReloadError {
    match error {
        RealmError::Reload(error) => error,
        RealmError::HostContractIdMismatch
        | RealmError::MissingHostFunctionAuthority(_)
        | RealmError::HostFunctionAuthorityMismatch { .. } => ReloadError::HostContractIdMismatch,
        error => ReloadError::Migration(RuntimeMessage::inline(&error.to_string())),
    }
}
