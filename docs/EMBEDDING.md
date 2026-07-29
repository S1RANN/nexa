# Embedding Nexa

`nexa-embed` is the default application-facing API for trusted local Nexa
packages. It owns IDL validation, compilation, verification, stable export
lookup, one Realm per enabled package, root Scope lifetime, MustComplete
handlers, garbage collection, release draining, change scanning, selection
persistence, and shutdown.

Generate bindings from one NIDL file:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    nexa_idl::build::generate("app_api.nidl")
}
```

The generated module provides:

- `CANONICAL_IDL`, `INTERFACE_HASH`, and `contract()`;
- the Host trait and `registry(host)`;
- one `ScriptExport` marker and typed argument/output types per export.

Applications construct a package source and call only high-level operations:

```rust
let mut embed = NexaEmbed::builder(generated::contract())
    .host_factory(|context| generated::registry(AppHost::new(context)))
    .package_source(source)
    .entitlements(entitlements)
    .storage_dir("user-data/extensions")
    .require_export::<generated::OnEvent>()
    .build()?;
embed.discover()?;
embed.enable_defaults()?;
let outputs = embed.dispatch::<generated::OnEvent>(&args);
embed.tick()?;
embed.shutdown()?;
```

`call` targets one enabled package. `dispatch` invokes every enabled package by
priority descending and package ID ascending, and returns a
`PackageOutput<T>` carrying host-owned provenance. A package failure is
reported in that result and does not stop later packages.

Generated argument encoders calculate object, collection, and string capacity
before writing. Commit publishes the complete graph at once. Output decoding
copies owned Rust values out of the Realm. `MustCompletePolicy` rejects fuel
yield, explicit yield, Host wait, and trap; M2 event handlers cannot suspend.

Call `tick` once at the application's safe point. It advances Runtime work,
processes cancellation, collects unreachable argument/output graphs, drains
release records, updates faults, and performs deterministic development change
scans. Do not access Realm release queues from application code.

Call `shutdown` on the normal exit path. `Drop` attempts cleanup but cannot
report an error.

See `examples/hello-runtime` for the minimal integration and
`examples/snake-game` for package lifecycle, capabilities, entitlements,
runtime settings, typed state, and fault isolation.
