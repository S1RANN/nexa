# Nexa MVR Scope 1.0

## Verifiable hypotheses

- **H1a — Exact-build IDL:** generated Rust host bindings reduce glue code and move interface
  mismatches to build or load time.
- **H2a — Fast task:** the first-slice completion path has acceptable overhead while promotion
  preserves a continuation without fallible allocation.
- **H3a — Single-module stateful reload:** pause/stage/commit preserves registered state and
  improves development iteration for a naturally bounded subsystem.

## Required implementation

### Language

- statically typed functions and `task fn`
- scalars, strings, runes, structs, classes, enums
- compiler-built-in `Array`, `Map`, `Option`, `Result`, `Task`, `Buffer`
- `@stateful class`, same-module `StateHandle`
- `await`, non-suspending `defer`

### Runtime

- typed register bytecode and core verifier
- checked register interpreter
- generation handles and owner scopes
- fast task admission, first-slice execution, non-failing promotion
- fuel, safepoints, cancellation
- stop-the-world non-moving mark/sweep GC
- host request and host resource token tracking
- runtime-owned release queue
- copy buffers and immutable snapshots
- single-module pause/stage/commit reload

### Integration

- Rust host binding generated from exact-hash IDL
- CLI, trace, bounded model explorer, benchmark harness

## Gate 1 conclusions

Gate 1 may conclude only H1a, H2a, and H3a. It may not extrapolate to their deferred counterparts.
