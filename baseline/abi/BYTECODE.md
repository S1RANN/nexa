# Nexa Internal Language Bytecode 1.0

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

Bytecode version 3 adds canonical enum metadata, typed async-result policy metadata, and the
`ENUM_NEW`, `ENUM_TAG`, `ENUM_PAYLOAD`, `STATE_OLD_FIELD_GET`, `STATE_PRESERVE`, `STATE_REPLACE`,
and `STATE_FINISH` instructions. Async HostImports must reference a matching builtin Result enum.

The compiler lowers `None`, `Some`, `Ok`, `Err`, exhaustive enum `match`, and exact-error `?` to the
enum instructions. Migration source intrinsics lower to old-state reads, staging writes, explicit
Preserve/Replace/Delete decisions, and `STATE_FINISH`; examples may not patch migration bytecode in
Rust.

Compiler register counts are derived from a per-function plan. Local registers and the peak
expression, call-argument, match, and migration temporary windows are counted independently; no
fixed `locals + 8` allowance is permitted. The verifier independently checks every argument range.
