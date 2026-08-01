# Nexa diagnostics

Status: M4 COMPLETE; M4R1 COMPLETE

`nexa::Diagnostic` remains the leaf truth for parser, type checker, compiler,
Verifier, Runtime, and Reload errors. `nexa-embed` adds package identity,
source files, development generation, entrypoint/event context, sanitized task
identity, related locations, and textual fixes through `EngineDiagnostic`.
It does not translate compiler codes into a generic package failure.

## Stable stages and codes

Engine stages cover source discovery, manifest, policy, entitlement, parse,
type check, compile, verify, load, entrypoint, handler, migration, activation,
Reload, Runtime, resource, persistence, and shutdown.

Engine-only codes occupy the stable `NX7xxx` range:

- `NX7001`–`NX7004`: source, manifest, policy, and entitlement;
- `NX7010`–`NX7011`: required entrypoint presence and signature;
- `NX7101`–`NX7103`: MustComplete yield, wait, and trap;
- `NX7201`–`NX7202`: pre-commit rollback and activation fault;
- `NX7302`–`NX7303`: persistence and shutdown. `NX7301` is intentionally unregistered until a
  fallible release sink exists in the product path.

Compiler and Runtime failures retain their existing `NX1xxx`–`NX5xxx` code.
The single diagnostic registry and corpus test define the observable code set.

Every registered `NX7xxx` code is exercised through a real Engine product
path. The evidence harness captures the emitted `EngineDiagnostic`, renders
that same object as Human, JSON, and NDJSON, and runs each scenario twice for
determinism. The corpus rejects direct construction of a target Engine code and
requires `registered = observedThroughRealPaths` with
`directDiagnosticConstruction = 0`.

## Source identity

Each Candidate carries a `SourceFileRegistry`. Package-relative paths are
normalized, parent traversal and absolute paths are rejected, and files are
sorted before deterministic `FileId` assignment. A file stores its source and
line-start index, allowing byte spans to render as human line/column and LSP
UTF-16 ranges.

Runtime traps preserve module epoch, sanitized package-local task ID, source
span, Nexa script frames, and an optional Host-call boundary. Function metadata
uses stable IDs and definition spans; missing sidecar data falls back to
`<unknown function>` and never exposes a Bytecode slot as source identity.

## Renderers

The shared renderer supports:

```text
DiagnosticRenderer::human
DiagnosticRenderer::json
DiagnosticRenderer::ndjson
```

The JSON schema is version `1` and includes code, severity, stage, package,
file, range, message, related locations, notes, fixes, and safe execution
context. Human output includes source snippets and related positions.

Diagnostic messages are limited to 64 KiB. The Engine retains at most 64
records per package and 512 globally, drops the oldest record first, increments
a dropped counter, and exposes a bounded summary. Development events, Reload
reports, and per-package metrics are also bounded.
