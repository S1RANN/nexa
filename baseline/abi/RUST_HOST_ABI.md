# Rust Host ABI Specification 1.0

Generated Rust bindings are the sole supported MVR host integration.

The generated layer provides:

- host traits;
- typed argument/result thunks;
- host registry metadata;
- script export marker types;
- exact interface hash;
- test mocks.

Host panics must be contained and converted to structured host traps. Runtime internals, GC
pointers, frames, and mutable epoch roots are never exposed.
