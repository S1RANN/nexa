# Internal Language Scope

Version: **3.0.0**

Status: **COMPLETE**

Nexa is a Rust-hosted internal gameplay language. The current product boundary
is one engine team, exact-build host APIs, verified bytecode, bounded tasks,
owned host resources, typed `@state`, and Restart Reload.

```text
Nexa M4 Language Scale Foundation = COMPLETE
Nexa M4R1 Language Surface Reset = COMPLETE
Nexa Language v2 = COMPLETE
Contract Syntax v3 = IN PROGRESS
Structured Codegen v2 = COMPLETE
Standalone Profile v1 = COMPLETE
REPL v1 = COMPLETE
Multiple Entrypoint Model = COMPLETE
```

In scope:

- Lexer, parser, type checker, compiler, bytecode verifier, and interpreter.
- Rust-only generated Host bindings from one `.contract.nexa` source.
- Fuel-bounded tasks, explicit async host requests, cancellation, and inspection.
- Pure state migration with Preserve, Replace, and Delete.
- Stop-the-module Restart Reload with commit-before-activation semantics.
- Differential models, fuzzing, allocation budgets, and the Combat dogfood app.
- Compile-time Source Modules linked into one deterministic Package Artifact.
- Controlled, lockfile-pinned local static Library dependencies.
- Shared incremental analysis and source-level cross-file diagnostics.
- Pure, deterministic Package Tests with a rejecting Host.
- Language v2 with path-derived modules, `use`, `let mut`, field-level `mut`,
  Struct/Enum value semantics, sealed Class reference semantics, `async fn`,
  postfix `.await`, and attribute-based state and lifecycle metadata.
- Contract v3 sources with explicit `host` and `nexa` surfaces, structured ABI
  Descriptor v2 fingerprints, and token-based deterministic Rust generation.
- Typed Required and Optional entrypoint selection without function indexes in
  normal product APIs.
- Standalone Package and single-file script execution through a typed
  `main(args: Array<string>) -> i32`.
- REPL v1 cells compiled and executed through the production frontend,
  verifier, and Runtime.

Out of scope:

- General-purpose language or stable third-party ABI.
- Seamless continuation migration, Completion Buffers, or old-task resume.
- Concurrent old/new business epochs or public retired-epoch routing.
- AOT/JIT, remote registries, dependency version solving, hostile-bytecode
  sandbox, full semantic LSP/DAP, and runtime multi-module transactions.
- User generics, traits or interfaces, closures and higher-order functions,
  dynamic dispatch, inheritance, operator overloading, macros, reflection,
  `dynamic`/`any`, unwind exceptions, pointer or borrow syntax, user
  finalizers, and shared-memory threads.

Source Modules and Library Packages are compile-time concepts. They do not
create extra Realms or independently reloadable Runtime Modules: an Application
and its resolved static dependency closure always produce one Artifact, one
Realm, one Epoch, and one Package-level Restart Reload boundary.

Language v2 and Contract Syntax v3 are intentional breaking surfaces. Active parsers,
descriptors, bytecode products, generated bindings, examples, and tools do not
provide aliases or decoders for former executable or Contract syntax,
canonical string hashes, legacy function indexes, schema 1 Packages, or
Bytecode v5. Historical documents and immutable tags remain archival evidence,
not compatibility contracts.

The former general-product MVR ended with `Gate 1 v2.9 = STOP`. Its complete
record is the annotated tag `gate1-v2.9-stop`.
