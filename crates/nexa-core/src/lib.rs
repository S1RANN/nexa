//! Stable primitive identities and trace records shared by Nexa crates.

use std::fmt;

/// Identifies a source file inside one compilation session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u32);

/// Identifies a module in a verified bundle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleId(pub u32);

/// Identifies a function in a verified module.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FunctionId(pub u32);

/// Identifies a concrete runtime or bytecode type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeId(pub u32);

/// Identifies an isolated runtime realm.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RealmId(pub u32);

/// A generation-protected runtime identity.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RawHandle {
    pub realm_id: u32,
    pub index: u32,
    pub generation: u32,
}

impl RawHandle {
    #[must_use]
    pub const fn new(realm_id: u32, index: u32, generation: u32) -> Self {
        Self {
            realm_id,
            index,
            generation,
        }
    }
}

/// A half-open byte range in a source file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl SourceSpan {
    #[must_use]
    pub const fn new(file: FileId, start: u32, end: u32) -> Self {
        Self { file, start, end }
    }

    #[must_use]
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// Stable identifier derived from a normative symbolic name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StableId(pub u64);

impl StableId {
    /// Uses fixed FNV-1a instead of a process-randomized hash so generated IDs are reproducible.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        Self::from_parts(&[name])
    }

    #[must_use]
    pub fn from_parts(parts: &[&str]) -> Self {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for part in parts {
            for byte in part.bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        Self(hash)
    }
}

#[must_use]
pub fn machine_state_id(machine: &str, state: &str) -> StableId {
    StableId::from_parts(&[machine, "::State::", state])
}

#[must_use]
pub fn machine_event_id(machine: &str, event: &str) -> StableId {
    StableId::from_parts(&[machine, "::Event::", event])
}

#[must_use]
pub const fn machine_instance_id(handle: RawHandle) -> u64 {
    (handle.generation as u64) << 32 | handle.index as u64
}

/// Hashes the invariant-visible state shared by the runtime trace and reference model.
#[must_use]
pub fn machine_invariant_hash(
    machine: &str,
    state: &str,
    owner_scope: Option<RawHandle>,
    resources: &[(&str, i64)],
) -> u64 {
    let resource_ids = resources
        .iter()
        .map(|(resource, amount)| (StableId::from_name(resource), *amount));
    machine_invariant_hash_ids(
        StableId::from_name(machine),
        machine_state_id(machine, state),
        owner_scope,
        resource_ids,
    )
}

/// Allocation-free invariant hashing for runtime hot paths.
#[must_use]
pub fn machine_invariant_hash_ids(
    machine: StableId,
    state: StableId,
    owner_scope: Option<RawHandle>,
    resources: impl IntoIterator<Item = (StableId, i64)>,
) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut update = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    };
    update(&machine.0.to_le_bytes());
    update(&state.0.to_le_bytes());
    match owner_scope {
        Some(owner) => {
            update(&[1]);
            update(&owner.realm_id.to_le_bytes());
            update(&owner.index.to_le_bytes());
            update(&owner.generation.to_le_bytes());
        }
        None => update(&[0]),
    }
    for (resource, amount) in resources {
        update(&resource.0.to_le_bytes());
        update(&amount.to_le_bytes());
    }
    hash
}

impl fmt::Display for StableId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:016x}", self.0)
    }
}

/// Runtime state-machine category used by versioned traces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MachineKind {
    Task,
    Scope,
    Module,
    Reload,
    HostRequest,
    ResourceToken,
    ReleaseQueue,
    Custom(StableId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransitionDisposition {
    Applied,
    GuardRejected,
    Undefined,
}

/// Resource accounting changes caused by one transition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceDelta {
    pub resource: StableId,
    pub amount: i64,
}

pub const MAX_INLINE_RESOURCE_DELTAS: usize = 4;

/// Fixed-capacity transition deltas, sized for every generated Nexa machine transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineDeltas {
    values: [ResourceDelta; MAX_INLINE_RESOURCE_DELTAS],
    len: u8,
}

impl Default for InlineDeltas {
    fn default() -> Self {
        Self::new()
    }
}

impl InlineDeltas {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            values: [ResourceDelta {
                resource: StableId(0),
                amount: 0,
            }; MAX_INLINE_RESOURCE_DELTAS],
            len: 0,
        }
    }

    pub fn try_push(&mut self, delta: ResourceDelta) -> Result<(), ResourceDelta> {
        let index = usize::from(self.len);
        let Some(slot) = self.values.get_mut(index) else {
            return Err(delta);
        };
        *slot = delta;
        self.len += 1;
        Ok(())
    }

    #[must_use]
    pub const fn as_slice(&self) -> &[ResourceDelta] {
        self.values.split_at(self.len as usize).0
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &ResourceDelta> {
        self.as_slice().iter()
    }
}

/// Versioned trace record emitted by generated state-machine transitions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceRecord {
    pub schema_version: u16,
    pub sequence: u64,
    pub machine_kind: MachineKind,
    pub machine_id: u64,
    pub transition_id: StableId,
    pub disposition: TransitionDisposition,
    pub old_state: StableId,
    pub event: StableId,
    pub new_state: StableId,
    pub realm_id: u32,
    pub module_epoch: u64,
    pub owner_scope: Option<RawHandle>,
    pub resource_deltas: InlineDeltas,
    pub error_code: Option<u32>,
    pub invariant_hash: u64,
}

pub const TRACE_SCHEMA_VERSION: u16 = 1;

#[cfg(test)]
mod tests {
    use super::{
        FileId, RawHandle, SourceSpan, StableId, machine_event_id, machine_invariant_hash,
        machine_state_id,
    };

    #[test]
    fn stable_ids_are_reproducible_and_name_sensitive() {
        assert_eq!(
            StableId::from_name("TASK_CREATED_START_READY"),
            StableId::from_name("TASK_CREATED_START_READY")
        );
        assert_ne!(
            StableId::from_name("TASK_CREATED_START_READY"),
            StableId::from_name("TASK_READY_POLL_RUNNING")
        );
    }

    #[test]
    fn source_spans_are_half_open() {
        let span = SourceSpan::new(FileId(3), 4, 11);
        assert_eq!(span.len(), 7);
        assert!(!span.is_empty());
    }

    #[test]
    fn machine_trace_ids_and_invariants_are_canonical() {
        assert_eq!(
            machine_state_id("Task", "Ready"),
            StableId::from_name("Task::State::Ready")
        );
        assert_eq!(
            machine_event_id("Task", "Poll"),
            StableId::from_name("Task::Event::Poll")
        );
        let owner = RawHandle::new(1, 2, 3);
        assert_eq!(
            machine_invariant_hash("Task", "Ready", Some(owner), &[("task_slot", 1)]),
            machine_invariant_hash("Task", "Ready", Some(owner), &[("task_slot", 1)])
        );
    }
}
