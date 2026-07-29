# Nexa Roadmap

```text
Nexa Internal Pivot M1 = COMPLETE
Nexa M2 Embedding & Snake Pilot = COMPLETE
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
```

## Current position

The former MVR reached a verified Gate 1 v2.9 decision of **STOP** because
H1/H2/H3 were stable failures. The repository then pivoted to an internal,
Rust-only gameplay language.

```text
Gate 1 v2.9 STOP (historical tag)
→ Internal Pivot M1
   → repository slimming
   → .nidl single-source Host binding
   → stable high-level Task lifecycle
   → Restart Reload v1
   → Combat dogfood loop
```

Internal Pivot M1 and Nexa M2 are complete. M2 adds a generic
package-oriented embedding layer, typed Rust-to-Nexa export calls, and a
playable Snake pilot using the same package contract for first-party content,
licensed content, and reviewed local extensions. Its finalization gates cover
the full workspace, package lifecycle stress, resource stability, and dispatch
latency.

M1 final closure includes the generated-registry
positive path, real invalid-event Runtime calls, deterministic fuzz corpus
replay, and independently observable final gates.

## Active product

- Typed gameplay source, bytecode, verifier, and interpreter.
- One `.nidl` schema per Host API with deterministic generated Rust binding.
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
execution, cross-module stateful reload, or a new Gate/Contract/Receipt evidence
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
entitlements, one-Realm-per-package lifecycle, typed export dispatch, and
automatic Runtime maintenance in `nexa-embed`. The Snake domain translates
built-in, DLC, and local Mod directories into those generic concepts.

M2 does not add JIT/AOT, remote packages, a public Mod marketplace, hostile
code sandboxing, network or arbitrary filesystem permissions, new language
syntax, LSP/DAP, old-Task migration, completion replay, or multi-version
business execution.

## Next review

Dogfood the M2 embedding API and Snake pilot before approving broader
integration. Do not reopen seamless Reload or a general-product route without
new user evidence and a separately approved milestone.
