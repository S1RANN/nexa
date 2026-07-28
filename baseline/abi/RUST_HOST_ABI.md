# Rust Host ABI Specification 1.0

Generated Rust bindings are the sole supported internal-language host integration.

The generated layer provides:

- host traits;
- typed argument/result thunks;
- host registry metadata;
- script export marker types;
- exact interface hash;
- test mocks.

Hosted realms register against a process-level `RuntimeHost`. `RuntimeHost::close()` succeeds only
after every hosted realm is dropped, every completion reservation is released, and every
transferred release is drained. Closed hosts reject new realms. In debug builds, dropping the final
host handle without an explicit close prints the live realm, completion, and release counts.

The HostCall bridge accepts at most eight arguments in an inline `HostArgs` buffer. Scalar and
handle arguments are decoded without constructing an intermediate `Vec<HostValue>`.

Host panics must be contained and converted to structured host traps. Runtime internals, GC
pointers, frames, and mutable epoch roots are never exposed.
