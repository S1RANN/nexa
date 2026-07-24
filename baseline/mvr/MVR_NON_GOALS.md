# Nexa MVR Non-goals 1.0

The MVR deliberately does not implement or validate:

- dynamic typing or interfaces
- user-defined generics, const generics, macros, inheritance
- C, C++, or C# bindings
- untrusted code or a security sandbox
- AOT or JIT
- cross-module stateful references or reload groups
- read/write leases
- compatible ABI adapters or independent host/script version evolution
- strict deterministic scheduling
- LSP, DAP, package registry, or graphical profiler
- seamless online hot patching
- complete-runtime frame stability under an incremental GC

These capabilities must not appear in parser syntax, bytecode, public APIs, examples, or empty
future-facing abstractions during Gate 1.
