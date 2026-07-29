# Nexa Embed API

Status: M2 COMPLETE

`nexa-embed` is the generic, package-oriented boundary between a Rust
application and Nexa Runtime. Applications provide a generated `HostContract`,
a per-package `HostRegistryFactory`, one or more `PackageSource` values, an
optional entitlement resolver, and an optional storage directory.

The lifecycle entry points are `discover`, `enable_defaults`, `enable`,
`disable`, `reload`, `reload_changed`, `call`, `dispatch`, `tick`, and
`shutdown`. The application never needs Realm, Module, Scope, Task, release
queue, or raw `RuntimeValue` operations.

Generated bindings expose canonical NIDL, its exact interface hash, a
`HostContract`, a registry factory, and a `ScriptExport` marker for every
export. `ScriptExport` owns argument requirements, transactional encoding,
expected signature, and owned output decoding. Export lookup is by stable ID,
never by a caller-authored function index.

M2 handlers use `MustCompletePolicy`. Completion is decoded. Fuel or explicit
yield, Host wait, trap, missing export, and signature mismatch are terminal
errors for that package invocation. Argument allocation is preflighted and
committed as one transaction, so a failed encode cannot publish a partial
object graph.

`dispatch` returns provenance-bearing `PackageOutput<T>` values. Package ID,
source ID, trust, and effective capabilities come from the host-owned package
record and cannot be supplied by script.

Explicit `shutdown` is normative. `Drop` is best-effort only.
