# Nexa Roadmap

```text
Nexa Internal Pivot M1 = COMPLETE
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

Internal Pivot M1 passed every Cargo and xtask gate on one commit with a clean
working tree.

## Active product

- Typed gameplay source, bytecode, verifier, and interpreter.
- One `.nidl` schema per Host API with deterministic generated Rust binding.
- Bounded Task polling with explicit Yield, Waiting, cancellation, and traps.
- Host Request/Token/Snapshot ownership and terminal resource invariants.
- Typed state migration with Preserve, Replace, and Delete.
- Stop-the-module Restart Reload with late-completion discard by epoch.
- Differential models, fuzz smoke, absolute allocation and latency budgets.

## Deliberately removed

- Gate-version-specific runners, decision tools, fixtures, contracts, and Raw
  evidence in the active tree.
- Seamless old-continuation migration and old-Task resume.
- Intermediate completion replay, old-module business routing, and concurrent
  old/new code execution.
- Cross-run micro-delta performance decision systems.

## Next review

After sustained Combat dogfood use, review ergonomics, diagnostics, missing
gameplay types, and integration costs. Do not reopen seamless Reload or a
general-product route without new user evidence and a separately approved
milestone.
