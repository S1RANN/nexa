# Nexa Internal Language Bytecode 6

Version: **6.0.0**

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

Bytecode version 6 is the only accepted wire version. Every other version,
including v5, is rejected from the Header before section decoding; there is no
product compatibility path.

Version 6 includes typed scalar-to-string instructions for deterministic
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

Enum, typed async-result, state migration, collection, state-handle, exact
object-root, and Class write-barrier metadata are part of version 6. Async
HostImports must reference a matching builtin Result enum. Source-level
postfix `.await` is fully lowered before encoding and introduces no public
Future or Awaitable wire type.

The compiler lowers `None`, `Some`, `Ok`, `Err`, exhaustive enum `match`, and exact-error `?` to the
enum instructions. Migration source intrinsics lower to old-state reads, staging writes, explicit
Preserve/Replace/Delete decisions, and `STATE_FINISH`; examples may not patch migration bytecode in
Rust.

Compiler register counts are derived from a per-function plan. Local registers and the peak
expression, call-argument, match, and migration temporary windows are counted independently; no
fixed `locals + 8` allowance is permitted. The verifier independently checks every argument range.
