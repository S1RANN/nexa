# Nexa v2 Standard Library

Status: M4R1 COMPLETE

The standard library is versioned and supplied by the compiler. Operations that
belong to a value type use receiver methods and require no `use std::...`
declaration. Namespace functions remain available as compatibility aliases for
the frozen M4R1 descriptor; new code should prefer methods.

## Receiver methods

### Scalar and string values

Every scalar (`i32`, `i64`, `f32`, `f64`, `bool`, `rune`, and `string`) provides
`to_string()`.

`string` additionally provides:

- `len()`, `byte_len()`, `contains()`, `starts_with()`, `ends_with()`
- `substring()`, `trim()`, `split()`
- `equals()`, `concat()`, `rune_at()`, `hash()`

```nexa
let label = "  Nexa  ".trim();
let visible = label.contains("Nexa");
let parts = "a,b".split(",");
```

String indices, lengths, and substring ranges count Unicode Scalar Values.
`byte_len()` is the only byte-counted API.

### Option and Result

```nexa
let name = args.get(0).unwrap_or("world");
let ready = candidate.is_some();
let value = result.unwrap_or(0);
```

- `Option<T>`: `is_some()`, `is_none()`, `unwrap_or(fallback)`
- `Result<T, E>`: `is_ok()`, `is_err()`, `unwrap_or(fallback)`

### Array and Map

```nexa
let mut scores: Array<i32> = Array::new();
scores.push(10);
let first = scores.get(0); // Option<i32>
let value = first.unwrap_or(0);
```

- `Array<T>`: `len()`, `is_empty()`, `get()`, `push()`, `pop()`, `insert()`,
  `remove()`, `reserve()`, `capacity()`, `clear()`, `shrink_to_fit()`
- `Map<K, V>`: `len()`, `contains()`, `get()`, `set()`/`insert()`, `remove()`,
  `clear()`

Array indexing (`values[index]`) remains the trapping/direct form. `get(index)`
is the bounds-checked form and returns `Option<T>`.

## Namespaced modules

Operations that do not naturally belong to one receiver remain namespaced:

- `std::math`: numeric `abs`/`clamp`, deterministic floor/ceil/round/sqrt/sin/cos,
  and Vec2/Vec3 construction and arithmetic.
- `std::debug`: deterministic `assert` and `trap`, with no implicit logging
  authority.

```nexa
use std::math;
use std::debug;

let bounded = math::clamp_i32(score, 0, 100);
debug::assert(bounded >= 0);
```

The compatibility namespaces `std::core`, `std::string`, and
`std::collections` retain their M4R1 free-function aliases so existing package
artifacts and fingerprints remain valid. They are not the preferred source API.

Every operation remains subject to verifier type flow, deterministic fuel
charging, GC roots, precise Root Maps, and Source Maps. The standard library
has no ambient Host capability.
