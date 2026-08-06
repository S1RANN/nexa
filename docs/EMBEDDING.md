# Embedding Nexa

Status: Nexa M4 COMPLETE; M4R1 Language Surface Reset COMPLETE

`nexa-embed` is the application-facing crate for trusted local Nexa packages.
It owns contract validation, package discovery, compilation, verification,
typed entrypoint lookup, one Realm per enabled package, reload, garbage
collection, release draining, and shutdown. Application code does not call the
interpreter or address a function by bytecode index.

## Define the Host contract

Contract v3 makes the call direction explicit:

```nexa
contract App;

enum Event {
    Started,
    Message(string),
}

enum Command {
    Log(string),
}

enum LoadError {
    Missing,
    Denied,
    Cancelled,
}

host {
    fn format_message(message: string) -> string;

    @fuel(8)
    @cancel(return_error)
    @abandon(trap)
    async fn load_message(id: string) -> Result<string, LoadError>;
}

nexa {
    fn on_event(event: Event) -> Array<Command>;
    fn inspect_state() -> Option<string>;
}
```

The Rust Host implements functions in `host {}` and Nexa code calls them
through `use host::app;`. Nexa packages implement functions in `nexa {}` and
the Rust Host calls them through generated typed entrypoint markers. A
contract declaration only defines a legal entrypoint signature; the Host
decides separately whether that entrypoint is required.

Generate bindings from a build script:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    nexa_contract::build::generate("app_api.contract.nexa")?;
    Ok(())
}
```

Include the generated file from the application crate:

```rust
mod generated {
    include!(concat!(env!("OUT_DIR"), "/app_api.rs"));
}
```

Generation consumes a validated Contract v3 source and emits deterministic Rust
through a structured token model. The generated module contains:

- the `AppHost` trait and a typed Host registry;
- Rust representations for contract structs, enums, handles, tokens, and
  snapshots;
- one marker plus argument and output types for each `nexa` function;
- the exact `SOURCE`, canonical `CONTRACT_DESCRIPTOR`,
  `CONTRACT_RUNTIME_ID`, and 32-byte `CONTRACT_FINGERPRINT`.

Those generated constants describe the full legal Contract. Package
compilation derives a separate effective Descriptor and fingerprint for the
surface selected by that linked Package closure.

For example, `fn on_event(...)` generates `OnEvent`, `OnEventArgs`, and
`OnEventOutput`; `OnEvent::NAME` is `"on_event"`. Generated code contains no
legacy function-index API.

Generated tokens remain typed end to end. A Contract `Token<Entity>` becomes an
`EntityToken` whose Rust and Runtime identities derive from the `Entity`
Handle Stable ID; it cannot decode or release a token for another Handle.

See [CONTRACT.md](CONTRACT.md) for the complete contract grammar, attributes, naming
rules, and ABI identity rules.

## Construct the Engine

Import the common facade with `nexa_embed::prelude::*` and construct one
Engine from the generated contract:

```rust
let mut engine = NexaEngine::builder(generated::contract())
    .host_factory(|context| generated::registry(ApplicationHost::new(context)))
    .package_source(source)
    .entitlements(entitlements)
    .storage_dir("user-data/extensions")
    .require_export::<generated::OnEvent>()
    .development(DevelopmentConfig::default())
    .build()?;

engine.discover()?;
engine.enable_defaults()?;
```

`require_export::<E>()` means every package enabled by this Engine must
implement `E`. Loading rejects a package that implements an entrypoint under a
wrong signature, whether the entrypoint is required or optional.

The prelude intentionally excludes Realm, Host Runtime, Task, Scope, Step, and
release-queue handles. Those are implementation details, not application
integration APIs.

## Call package entrypoints

Required entrypoints use typed single-package or broadcast calls. Broadcast
order is priority descending and package ID ascending. Each package result
carries Host-owned provenance; one package failure does not prevent later
packages from running.

```rust
let output = engine.call::<generated::OnEvent>(&package_id, &args)?;
let outputs = engine.dispatch::<generated::OnEvent>(&args);
```

Optional entrypoints preserve the distinction between “not implemented” and
“implemented but failed”:

```rust
if engine.has_export::<generated::InspectState>(&package_id) {
    match engine.call_optional::<generated::InspectState>(&package_id, &args) {
        None => unreachable!("the preceding typed query found the entrypoint"),
        Some(Ok(value)) => consume(value),
        Some(Err(error)) => report_package_error(error),
    }
}
```

The result shape is:

```text
None              entrypoint is not implemented
Some(Ok(value))   entrypoint ran successfully
Some(Err(error))  entrypoint exists but invocation failed
```

`dispatch_optional::<E>(&args)` returns a deterministically ordered vector for
enabled packages that actually implement `E`; order is priority descending,
then package ID ascending. Do not probe or call entrypoints by string name or
function index.

## Drive the safe point

Call `tick` at the application's safe point:

```rust
loop {
    let report = engine.tick()?;
    consume_events(report);
}
```

`tick` accepts completed candidates, commits only the newest build identity,
advances runtime work, processes cancellation, collects unreachable
argument/output graphs, drains release records, updates faults, and performs
deterministic development scans. Application code must not access a Realm
release queue directly.

Generated argument encoders reserve the complete object, collection, and
string capacity before publishing a graph. Output decoding copies owned Rust
values out of the Realm. Immediate handlers reject suspension, Host wait,
explicit yield, fuel yield, and traps.

Call `shutdown` on the normal exit path:

```rust
engine.shutdown()?;
```

`Drop` attempts cleanup but cannot report a shutdown error.

## Contract and package changes

One candidate identity includes the package ID, generation, and full build
fingerprint. The fingerprint covers the Package-specific effective Contract
fingerprint, manifest, lockfile, root source set, complete local Library
closure, language/compiler/Bytecode versions, and effective compiler options.
The effective Contract accumulates Host calls and shared types referenced
anywhere in that linked closure, plus Required and actually implemented Nexa
entrypoints. Unused Optional declarations remain outside it. A stale candidate
cannot commit after a source add, delete, rename, dependency retarget, lockfile
change, newly effective Contract declaration, or option change.

A changed Contract Host surface cannot be committed as a script-only reload.
Development mode emits `HostRebuildRequired`, retains Last Known Good, and
requires rebuilding the Rust application and generated bindings. Adding an
unrelated optional `nexa` entrypoint does not invalidate packages that do not
use or implement it.

See [DEVELOPMENT_LOOP.md](DEVELOPMENT_LOOP.md) for reload and freshness rules,
[STANDALONE.md](STANDALONE.md) for command-line applications, and
[REPL.md](REPL.md) for interactive sessions. `examples/hello-runtime` is the
minimal embedded integration; `examples/snake-game` demonstrates multiple
typed entrypoints, capabilities, entitlements, state, and package isolation.
