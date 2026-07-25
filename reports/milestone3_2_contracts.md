# Milestone 3.2 — Runtime/Language Contract Closure

Status: complete.

## Explicit Realm capabilities

`RealmRuntime::isolated(config)` admits pure modules, tasks, GC objects, and local state.
Module admission rejects HostImports and host-owned resource types. Request, token, snapshot, and
raw resource-context APIs return `HostCapabilitiesUnavailable`.

`RealmRuntime::hosted(config, runtime_host, host_registry)` requires a registry interface hash and
owns both the registry and the process-level release sink. The ambiguous registry-only constructor
is removed. Realm Drop transfers reserved releases to RuntimeHost.

The public release domains are VM, Render, Audio, and IO. The incomplete shared Custom bucket is
removed and a custom-domain registry remains deferred.

## Enum, Option, and Result substrate

Bytecode v3 carries `EnumType`/`EnumVariant` metadata. `ENUM_NEW`, `ENUM_TAG`, and
`ENUM_PAYLOAD` are encoded, decoded, typed by the verifier, and executed through the GC heap.
`option_type(T)` and `result_type(T, E)` produce canonical builtin metadata; both payload and
payload-free variants share the heap representation.

## Typed asynchronous host results

Async HostImports contain the Result type ID, success and error payload types, `CancelPolicy`, and
`AbandonPolicy`. The verifier requires matching Result metadata.

IDL request functions return `request<Result<Success, Error>>`; policies and types are part of the
exact interface hash. Generated bindings emit typed error enums and typed completion-ticket
wrappers.

Completion behavior is:

```text
Success   -> Result::Ok(payload)
Error     -> Result::Err(error)
Cancelled -> ReturnError or task cancellation
Abandoned -> ReturnError or HostTrap
```

Host panics, ABI mismatches, and malformed bytecode remain traps.

## Stateful identity and strict migration

`StateHandle` is keyed by `StatefulDomainId`, StableId, and generation. Candidate roots inherit the
old Stateful Domain while module epoch identity changes.

Migration reads immutable old fields through `STATE_OLD_FIELD_GET` and uses explicit Preserve,
Replace, Delete, and Finish operations. Same-schema untouched migration is an identity clone.
Changed-schema untouched migration returns `MigrationNoOutput`; changed-schema staging must be
finished and must account for every old identity. Replace increments generation, Preserve retains
it, and graph/handle validation runs before publication.

## Acceptance evidence

Workspace tests cover isolated admission/resource rejection, enum allocation/tag/payload,
typed `Result::Err` task resumption, policy-sensitive IDL hashing, generated completion wrappers,
Stateful Domain continuity, explicit migration output, and the existing multi-epoch differential
suite.
