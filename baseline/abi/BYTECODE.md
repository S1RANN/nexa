# Nexa Internal Language Bytecode 5

The bytecode is a fixed-endian, sectioned, versioned typed-register format. Runtime execution
accepts only a `VerifiedModule`.

Required sections:

```text
Header
Strings
Types
Constants
HostImports
Functions
Code
RootBitmaps
Safepoints
LoopBounds
SourceMap
ReloadMetadata
```

The core verifier independently validates section bounds, instruction boundaries, CFG edges,
register types, call signatures, field slots, root bitmaps, safepoints, frame quotas, host
signatures, and immediate-function WCET. Immediate loops require an explicit static upper bound;
recursive immediate call graphs are rejected.

Bytecode version 5 is the only accepted wire version. Old bytecode fixtures are
not decoded through a compatibility path.

Version 5 adds typed scalar-to-string instructions for deterministic
interpolation:

```text
STRING_TO_STRING
I32_TO_STRING
I64_TO_STRING
F32_TO_STRING
F64_TO_STRING
BOOL_TO_STRING
RUNE_TO_STRING
```

It also adds typed numeric less-than instructions used by non-unrolled range
loops. Conversion results are GC-rooted Strings; every conversion is covered
by Fuel accounting, Safepoints, Root Maps, the Verifier, and Source Maps.

Earlier enum, typed async-result, state migration, collection, and state-handle
metadata remains part of version 5. Async HostImports must reference a matching
builtin Result enum.

The compiler lowers `None`, `Some`, `Ok`, `Err`, exhaustive enum `match`, and exact-error `?` to the
enum instructions. Migration source intrinsics lower to old-state reads, staging writes, explicit
Preserve/Replace/Delete decisions, and `STATE_FINISH`; examples may not patch migration bytecode in
Rust.

Compiler register counts are derived from a per-function plan. Local registers and the peak
expression, call-argument, match, and migration temporary windows are counted independently; no
fixed `locals + 8` allowance is permitted. The verifier independently checks every argument range.
