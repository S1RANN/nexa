# Nexa Embed API

Version: **1.0.0**

Status: M4 COMPLETE; M4R1 COMPLETE

`nexa-embed` is the generic, package-oriented boundary between a Rust
application and Nexa Runtime. Applications provide a generated `HostContract`,
a per-package `HostRegistryFactory`, one or more `PackageSource` values, an
optional entitlement resolver, and an optional storage directory.

The application-owned facade is `NexaEngine`, built by
`NexaEngineBuilder`; the crate remains named `nexa-embed`. The lifecycle entry
points are `discover`, `enable_defaults`, `enable`,
`disable`, `reload`, `reload_changed`, `call`, `dispatch`, `tick`, and
`shutdown`. The application never needs Realm, Module, Scope, Task, release
queue, or raw `RuntimeValue` operations.

Generated bindings expose the validated Contract v3 source, its full ABI
Descriptor v2 and full Contract fingerprint, a `HostContract`, a Registry
factory, and a typed marker for every Nexa entrypoint. Each resolved Package
build derives its own effective Descriptor and fingerprint from that full
authority. Each marker owns argument requirements, transactional encoding,
expected signature, and owned output decoding. Entrypoint lookup is by marker
and stable ID, never by a caller-authored numeric position or string.

`require_export::<E>()` selects a globally Required typed entrypoint.
`has_export::<E>(package_id)`, `call_optional::<E>(package_id, args)`, and
`dispatch_optional::<E>(args)` inspect or call Optional typed entrypoints.
Absence is legal for Optional entrypoints; an implementation with the wrong
signature rejects the Package at load time.

Schema 2 project TOML expresses the same Host selection with
`required_entrypoints = ["on_event"]`. Values are exact `snake_case` names
from the Contract's `nexa {}` block. An empty list makes the surface Optional;
an omitted key selects the complete Contract surface. The legacy
`required_exports` key is rejected rather than aliased. The Rust method name
`require_export::<E>()` is intentionally retained by the M4R1 contract.

M2 handlers use `MustCompletePolicy`. Completion is decoded. Fuel or explicit
yield, Host wait, trap, missing required entrypoint, and signature mismatch
are terminal errors for that package invocation. Argument allocation is
preflighted and committed as one transaction, so a failed encode cannot
publish a partial object graph.

For a resolved Application build, the effective Contract selection is the
canonical union of Host functions and shared types referenced by the root
Package and every linked local Library, plus Required and root-implemented
Nexa entrypoints. Its raw 32-byte fingerprint is a field of the cumulative
Build Fingerprint. Unused Optional Contract declarations do not affect that
Package identity.

`dispatch` returns provenance-bearing `PackageOutput<T>` values. Package ID,
source ID, trust, and effective capabilities come from the host-owned package
record and cannot be supplied by script.

Explicit `shutdown` is normative. `Drop` is best-effort only.

When `DevelopmentConfig` is enabled, source changes are stabilized by content
hash, compiled and verified by one bounded worker, and returned to the calling
thread. Only `NexaEngine::tick()` may commit the newest generation. A failed,
stale, or Host-contract-incompatible Candidate cannot replace the active
Runtime or Last Known Good artifact.

The first observation of a changed hash creates a Candidate Generation.
Pre-queue Generations are explicitly tracked and receive a terminal outcome
when replaced, reverted to active content, disabled, removed, or shut down.
`PackageInspection::desired_hash` identifies the source content the Engine
currently intends to run. It is `None` when discovery cannot produce a valid
Candidate, including missing or unreadable source, so source failure closes the
commit gate instead of preserving a previously observed identity.

Immediately before entering Restart Reload, `tick` refreshes the Package
source and requires both the latest Candidate Generation and an exact match
between Candidate hash and `desired_hash`. Work made stale by a new hash,
reversion to active content, or a return to previously terminal content is
terminated across the unqueued, awaiting-queue, pending, in-flight,
result-queue, and ready-Candidate stages. It cannot become active even when it
completed compilation before the source changed.

`tick` returns `EngineTickReport`; `inspection` returns bounded, read-only
Engine, Package, development, diagnostic, Reload, and metric DTOs. These APIs
do not expose Realm, Runtime Host objects, or raw handles.

The immutable M1 through M3R3 completion tags remain historical boundaries.
M4 is complete at `language-scale-m4-complete`. M4R1 completed the breaking
Language v2, prior Contract surface, structured binding, Standalone, REPL, and
multiple-entrypoint surfaces.
