# Nexa v2 Standard Library

Status: M4R1 COMPLETE

The standard library is versioned and supplied by the compiler as statically
linked namespaces. It does not create a Realm and receives no ambient Host
capability. Its version and canonical descriptor are Build Fingerprint inputs.

The initial namespaces are:

- `std::core`: Option and Result predicates and `unwrap_or`, numeric min/max,
  and deterministic scalar conversion.
- `std::math`: `abs`, `clamp`, `floor`, `ceil`, `round`, `sqrt`, `sin`, `cos`,
  plus Vec2/Vec3 construction and add/subtract/scale/dot/length.
- `std::string`: scalar-counted `len`, `contains`, `starts_with`, `ends_with`,
  `trim`, `substring`, `split`, and explicit `byte_len`.
- `std::collections`: Array and Map inspection plus local `push`, `pop`,
  `insert`, and `remove` mutation.
- `std::debug`: deterministic `assert` and `trap`, with no implicit logging
  authority.

Using a standard-library namespace creates an ordinary namespace binding:

```nexa
use std::math;
use std::string as text;

let bounded = math::clamp_i32(score, 0, 100);
let visible = text::trim(label);
```

Generic library types use PascalCase and associated construction uses `::`.
Type arguments are inferred at the call site when the binding supplies them:

```nexa
let mut scores: Array<i32> = Array::new();
scores.push(10);

let selected: Option<i32> = Option::Some(scores[0]);
```

String indices, lengths, and substring ranges count Unicode Scalar Values.
`byte_len` is the only byte-counted length API. Every operation remains
subject to verifier type flow, deterministic fuel charging, GC roots, precise
Root Maps, and Source Maps.
