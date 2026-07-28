# Internal Language Scope

Version: **1.0.0**

Status: **ACTIVE**

Nexa is a Rust-hosted internal gameplay language. The current product boundary
is one engine team, exact-build host APIs, verified bytecode, bounded tasks,
owned host resources, typed `@state`, and Restart Reload.

In scope:

- Lexer, parser, type checker, compiler, bytecode verifier, and interpreter.
- Rust-only generated host bindings from one `.nidl` source.
- Fuel-bounded tasks, explicit async host requests, cancellation, and inspection.
- Pure state migration with Preserve, Replace, and Delete.
- Stop-the-module Restart Reload with commit-before-activation semantics.
- Differential models, fuzzing, allocation budgets, and the Combat dogfood app.

Out of scope:

- General-purpose language or stable third-party ABI.
- Seamless continuation migration, Completion Buffers, or old-task resume.
- Concurrent old/new business epochs or public retired-epoch routing.
- AOT/JIT, package manager, hostile-bytecode sandbox, LSP/DAP, and multi-module
  transactions.

The former general-product MVR ended with `Gate 1 v2.9 = STOP`. Its complete
record is the annotated tag `gate1-v2.9-stop`.
