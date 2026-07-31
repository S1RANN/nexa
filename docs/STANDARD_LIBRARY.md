# Nexa M4 Standard Library

Status: M4 COMPLETE

The M4 standard library is versioned and supplied by the compiler as statically
linked Modules. It does not create a Realm and receives no ambient Host
capability. Its version and canonical descriptor are Build Fingerprint inputs.

The initial Modules are:

- `std.core`: Option/Result predicates and `unwrap_or`, numeric min/max, and
  deterministic scalar conversion.
- `std.math`: abs, clamp, floor, ceil, round, sqrt, sin, cos, plus Vec2/Vec3
  construction and add/subtract/scale/dot/length.
- `std.string`: scalar-counted len, contains, starts/ends-with, trim,
  substring, split, and explicit byte length.
- `std.collections`: Array and Map inspection plus local push/pop and
  insert/remove mutation.
- `std.debug`: deterministic assert and trap, with no implicit log authority.

Importing a library Module still creates an ordinary namespace:

```nexa
import std.math as math;
import std.string as text;
```

String indices, lengths, and substring ranges count Unicode Scalar Values.
`byte_len` is the only byte-counted length API. All operations are subject to
Verifier type flow, Fuel, GC roots, Root Maps, and Source Maps.
