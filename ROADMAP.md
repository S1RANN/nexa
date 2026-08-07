# Nexa Roadmap

```text
Nexa Internal Pivot M1 = COMPLETE
Nexa M2 Embedding & Snake Pilot = COMPLETE
Nexa M3 Developer Loop & Diagnostics = COMPLETE
NexaEngine API = COMPLETE
Automatic Candidate Compilation = COMPLETE
Candidate Generation Terminal Accounting = COMPLETE
Candidate Freshness Commit Guard = COMPLETE
Last Known Good Reload = COMPLETE
Unified Diagnostics = COMPLETE
Source-level Runtime Stack Traces = COMPLETE
Package-aware CLI = COMPLETE
Editor Diagnostics = COMPLETE
nexa-embed v1 = COMPLETE
Typed Script Export = COMPLETE
Snake Core = COMPLETE
Built-in Package Pilot = COMPLETE
Official DLC Pilot = COMPLETE
Trusted Local Mod Pilot = COMPLETE
Repository Slimming = COMPLETE
Rust Host Binding v1 = COMPLETE
Task Runtime Stabilization = COMPLETE
Restart Reload v1 = COMPLETE
Combat Dogfood Loop = COMPLETE
Nexa M4 Language Scale Foundation = COMPLETE
Nexa M4R1 Language Surface Reset = COMPLETE
Nexa M5 Deep Performance Optimization = COMPLETE
Nexa Language v2 = COMPLETE
Contract Syntax v3 = COMPLETE
Structured Codegen v2 = COMPLETE
Standalone Profile v1 = COMPLETE
REPL v1 = COMPLETE
Multiple Entrypoint Model = COMPLETE
```

## Current position

The former MVR reached a verified Gate 1 v2.9 decision of **STOP** because
H1/H2/H3 were stable failures. The repository then pivoted to an internal,
Rust-only gameplay language.

```text
Gate 1 v2.9 STOP (historical tag)
→ Internal Pivot M1
   → repository slimming
   → .contract.nexa single-source Host binding
   → stable high-level Task lifecycle
   → Restart Reload v1
   → Combat dogfood loop
```

Internal Pivot M1 and Nexa M2 are complete. M2 adds a generic
package-oriented embedding layer, typed Rust-to-Nexa entrypoint calls, and a
playable Snake pilot using the same package contract for first-party content,
licensed content, and reviewed local extensions. Its finalization gates cover
the full workspace, package lifecycle stress, resource stability, and dispatch
latency.

M3R1 closes the correctness and evidence gaps in M3. The public
facade is `NexaEngine`; stable source snapshots compile on one bounded worker,
and only `engine.tick()` may commit the newest verified Candidate. Failed or
superseded Candidates leave the active Runtime and Last Known Good artifact
unchanged. Compiler, Verifier, Reload, Runtime, resource, persistence, and
shutdown failures share one bounded `EngineDiagnostic` model. `nexa check`,
`nexa dev`, and the diagnostic-only LSP reuse that same compiler path.

The revision specifically requires lossless Worker backpressure and terminal
accounting, Engine diagnostics observed through real product paths, trustworthy
phase metrics, real project source policies, and exact Contract/LSP locations.
M3R3 adds the remaining freshness boundary: the Engine refreshes the desired
source identity immediately before commit and rejects stale Candidate work at
every development-pipeline stage.

M1 final closure includes the generated-registry
positive path, real invalid-event Runtime calls, deterministic fuzz corpus
replay, and independently observable final gates.

## Active product

- Typed gameplay source, bytecode, verifier, and interpreter.
- One `.contract.nexa` schema per Host API with deterministic generated Rust
  binding.
- Bounded Task polling with explicit Yield, Waiting, cancellation, and traps.
- Host Request/Token/Snapshot ownership and terminal resource invariants.
- Typed state migration with Preserve, Replace, and Delete.
- Stop-the-module Restart Reload with late-completion discard by epoch.
- Differential models, fuzz smoke, absolute allocation and latency budgets.

The finalization contract uses a handwritten `BusinessHostV1` for all 20 IDL
mutations and executes each patched binding through its generated Registry. It
drives Realm differential and fuzz inputs through a real `RealmRuntime`, closes
public Request/Waiting construction bypasses, and proves exactly-once
Request/Token/Snapshot release through the generated Combat Host binding.
Restart rollback never restores quiesced Tasks.

## M1 finalization scope

Finalization changes are limited to real Business Host binding validation,
closing Task lifecycle bypasses, driving differential and fuzz checks through
the real Runtime, exact Host resource release, and local completion gates.

This milestone does not add new syntax, user generics, `dynamic` or
`interface`, C++ or C# bindings, JIT/AOT, LSP/DAP, UGC or untrusted bytecode,
seamless old-Task migration, completion replay queues, multi-version business
execution, cross-Module state Reload, or a new Gate/Contract/Receipt evidence
system.

The final-closure batch also explicitly excludes JIT/AOT, new language syntax,
`dynamic`, `interface`, user generics, C++/C# bindings, UGC or untrusted
bytecode, LSP/DAP, advanced seamless Reload, old-Task restoration, completion
replay buffers, and any new Gate/Contract/Receipt system.

## Deliberately removed

- Gate-version-specific runners, decision tools, fixtures, contracts, and Raw
  evidence in the active tree.
- Seamless old-continuation migration and old-Task resume.
- Intermediate completion replay, old-module business routing, and concurrent
  old/new code execution.
- Cross-run micro-delta performance decision systems.

## M2 scope

M2 standardizes `PackageSource`, source policy, capabilities, activation,
entitlements, one-Realm-per-package lifecycle, typed entrypoint dispatch, and
automatic Runtime maintenance in `nexa-embed`. The Snake domain translates
built-in, DLC, and local Mod directories into those generic concepts.

M2 does not add JIT/AOT, remote packages, a public Mod marketplace, hostile
code sandboxing, network or arbitrary filesystem permissions, new language
syntax, LSP/DAP, old-Task migration, completion replay, or multi-version
business execution.

## M3 scope

M3 is limited to naming, the package development loop, diagnostics, source
identity and stack metadata, inspection DTOs, package-aware CLI commands, and
editor Problems. It does not include Pluie integration, sustained dogfood data
collection, JIT/AOT, an optimizing compiler, new syntax, user generics,
`dynamic`, `interface`, cross-package dependencies, cross-module state,
seamless old-Task migration, completion replay, remote Mods, an untrusted-code
sandbox, completion/refactoring LSP features, or DAP debugging.

## M3R1 scope

M3R1 changes only the existing M3 development loop, diagnostics, metrics,
package-aware CLI, Contract locations, URI handling, editor diagnostics, and
their
finalization gates. It does not add language syntax, multi-file modules, user
generics, JIT, interpreter optimization, a full semantic LSP, DAP, Pluie,
remote packages, or untrusted execution.

## M3R2 scope

M3R2 changes only Candidate Generation terminal accounting before Worker
admission and its finalization evidence. It adds no language, Runtime, Package,
LSP, editor, or optimization capability. The immutable M3R1 tag is retained.

## M3R3 scope

M3R3 changes only Candidate freshness and its finalization evidence. Each
Package tracks a fail-closed `desired_hash`; source removal or unreadable source
clears that identity. The Engine refreshes the source at the Runtime commit
safe point and requires both the latest Generation and exact desired hash.
Stale unqueued, awaiting, pending, in-flight, result-queue, and ready
Candidates terminate as superseded and cannot replace the active Runtime or
Last Known Good artifact.

The immutable M3, M3R1, and M3R2 completion tags remain unchanged. M3R3 adds no
language, Runtime, Package, LSP, editor, or optimization capability.

## Completed Contract Syntax v3 milestone

Contract Syntax v3 is the completed post-M5 migration milestone. It replaces the
old Contract file suffix and enclosing container with one flat
`*.contract.nexa` source headed by `contract Name;`. It also unifies
`SourceProfile::Contract`, renames the public crate/API/CLI/editor surface to
Contract terminology, migrates all examples and fixtures, and adds dedicated
syntax, semantics, Descriptor, codegen, CLI, LSP, and migration gates.

This milestone does not change Nexa Language v2, Host Contract schema v2,
Contract Descriptor v2 framing, Bytecode v7, Host/Nexa direction, resource
semantics, or generated Host/Nexa binding shapes. An equivalent container
migration preserves Stable IDs and normalized Descriptor bytes; required
syntax-version metadata renames are provenance only. The Build Fingerprint
records `CONTRACT_SYNTAX_VERSION = 3`.

Every row in `baseline/abi/CONTRACT_V3_ACCEPTANCE.md` passes on audited
candidate `c9c73b7b7ad85b6e0a938a103a28754725661857`. M6 remains deferred;
M7 and M8 did not start as part of this work.

## Completed M4/M4R1 foundation

M3R3 completes Candidate freshness and commit safety. M4 Language Scale
Foundation is complete and remains marked by the immutable annotated
`language-scale-m4-complete` tag. It adds compile-time Source Modules,
deterministic static local libraries, shared incremental analysis, cross-file
diagnostics and debugging, language ergonomics, a minimal standard library,
and pure Package Tests.

Source Modules are statically linked into one Package Artifact. Runtime
isolation remains one Realm and one Epoch per Application Package, and Reload
remains a Package-level Restart operation.

M4R1 is a breaking language and toolchain reset built on that completed
foundation. It replaces source declarations from M4 with path-derived modules,
`use`, `let mut`, field-level `mut`, `async fn`, postfix `.await`,
`@state(version = N) class`, and attribute-based lifecycle roles. It freezes
Struct and Enum as value types, Class as a sealed GC reference type, and
removes source-level compatibility aliases.

The Contract surface uses `contract`, `host`, `nexa`, `handle`, `fn`, and
`async fn`. Validated Contracts lower to a structured ABI Descriptor v2 and a
semantic Binding Model before Rust is generated through token syntax trees.
Generated bindings do not expose legacy export indexes or source-level Request
types.

The same release also finalizes two normal execution profiles. Standalone
Packages expose a typed `main(args: Array<string>) -> i32`; single-file scripts
lower ordered top-level statements to a synthetic main. REPL v1 sends every
cell through the production syntax, analysis, Typed IR, Bytecode, verifier, and
Runtime pipeline while preserving committed session state across cells.

The M4R1 completion contract re-runs all M1–M4 behavior on the new syntax, the
scale and reload stress suites, object-model and async matrices, structured
codegen, Standalone, REPL, and multiple-entrypoint Snake tests on one clean
tagged commit.

## After M4R1

```text
M5 Deep Performance Optimization = COMPLETE
M6 LLVM JIT = DEFER
M7 Full Semantic LSP = NOT STARTED
M8 DAP = NOT STARTED
```

M5 completed its two gated stages: M5a froze ValueLayout, Typed IR,
typed collections, and ExecutableModule; M5b completed incremental GC,
Task/Host/Engine fast paths, caches, the product corpus, and final performance
qualification. The formal same-machine comparison met all four targets with
no unexplained p95/p99 regression. Its normative evidence and decision rules
live under `baseline/performance/`.
The bounded public release attestation is
`baseline/performance/M5_RELEASE_SUMMARY.json`; exact regenerated samples stay
under the ignored `target/nexa-artifacts/m5/` tree.

M6 LLVM JIT is **DEFER**, not rejected. Warm V8 still leads all three
comparable pure-computation workloads by more than 1.5x, but M5 has no
per-workload CPU sample proving interpreter execution is at least 40% in two
products and no LLVM prototype proving compilation-cost amortization within a
frozen call/frame budget. A future JIT proposal must establish both before it
can change this decision.

M4R1 does not include user generics, traits or interfaces, closures,
inheritance, dynamic dispatch, operator overloading, macros, reflection,
`dynamic`/`any`, unwind exceptions, pointer syntax, a borrow checker, shared
memory threads, remote registries, untrusted-code sandboxing, JIT/AOT, a full
semantic LSP, or DAP.
