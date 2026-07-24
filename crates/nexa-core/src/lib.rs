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
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in name.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self(hash)
    }
}

#[must_use]
pub fn machine_state_id(machine: &str, state: &str) -> StableId {
    StableId::from_name(&format!("{machine}::State::{state}"))
}

#[must_use]
pub fn machine_event_id(machine: &str, event: &str) -> StableId {
    StableId::from_name(&format!("{machine}::Event::{event}"))
}

/// Hashes the invariant-visible state shared by the runtime trace and reference model.
#[must_use]
pub fn machine_invariant_hash(
    machine: &str,
    state: &str,
    owner_scope: Option<RawHandle>,
    resources: &[(&str, i64)],
) -> u64 {
    let mut canonical = format!("{machine}::{state}");
    if let Some(owner) = owner_scope {
        use std::fmt::Write as _;
        write!(
            canonical,
            "::owner={}:{}:{}",
            owner.realm_id, owner.index, owner.generation
        )
        .expect("writing String cannot fail");
    }
    for (resource, amount) in resources {
        use std::fmt::Write as _;
        write!(canonical, "::{resource}={amount}").expect("writing String cannot fail");
    }
    StableId::from_name(&canonical).0
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

/// Resource accounting changes caused by one transition.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceDelta {
    pub resource: String,
    pub amount: i64,
}

/// Versioned trace record emitted by generated state-machine transitions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceRecord {
    pub schema_version: u16,
    pub sequence: u64,
    pub machine_kind: MachineKind,
    pub machine_id: u64,
    pub transition_id: StableId,
    pub old_state: StableId,
    pub event: StableId,
    pub new_state: StableId,
    pub realm_id: u32,
    pub module_epoch: u64,
    pub owner_scope: Option<RawHandle>,
    pub resource_deltas: Vec<ResourceDelta>,
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
