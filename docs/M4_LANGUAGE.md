# Nexa Language v3 Surface

Status: Language v3 IN PROGRESS — generics, receiver methods, Set, collection iteration

Nexa Language v3 defines one source language for Packages, standalone programs, and
REPL cells. It includes monomorphized user functions and nominal types with
closed built-in bounds, but does not add user-defined traits, closures, dynamic
dispatch, inheritance, operator overloading, macros, reflection, raw pointers,
or nullable references.

## Naming and paths

Functions, parameters, fields, and local bindings use `snake_case`. Types and
Enum Variants use `PascalCase`; Consts use `SCREAMING_SNAKE_CASE`.

`::` selects a namespace, associated item, or Enum Variant. `.` selects a
field, method, or postfix operation:

```nexa
use std::core;

let limit = 8.max(MAX_SCORE);
let effect = FoodEffect::Grow(limit);
let label = effect.name;
```

Source files do not declare their Module. Their package-relative path is the
Module identity; see [Source Modules](MODULES.md).

## Generics

Functions, Structs, Enums, and non-state Classes may declare type parameters:

```nexa
struct Pair<T, U> {
    first: T,
    second: U,
}

enum Maybe<T> {
    None,
    Some(T),
}

class Box<T> {
    value: T,
}

fn identity<T>(value: T) -> T {
    return value;
}

fn smaller<T>(left: T, right: T) -> T
where
    T: Copy + PartialOrd,
{
    if left < right {
        return left;
    }
    return right;
}
```

Calls and constructors either provide every type argument or infer every type
argument. Inference uses function arguments, constructor fields, Enum payloads,
and an available expected result type. Direct angle brackets are used; there is
no turbofish and no partial `_` argument list.

```nexa
let inferred = Pair { first: "score", second: 100 };
let explicit = Pair<string, i32> { first: "score", second: 100 };
let present = Maybe::Some(10);
let empty: Maybe<i32> = Maybe::None;
let also_present = Maybe<i32>::Some(20);
```

The compiler monomorphizes each distinct concrete function and type argument
list and reuses identical instances. Type parameters never enter Bytecode or
the Runtime. A generic declaration's stable identity and canonical concrete
argument identities determine each instance identity; type parameter spelling
does not.

The closed bounds are `Copy`, `PartialEq`, `Eq`, `PartialOrd`, `Ord`, `Hash`,
`Display`, and `Add`/`Sub`/`Mul`/`Div`/`Rem`/`Neg` with `Output = T`. These
names do not form a user-extensible trait system. `Map<K, V>` requires
`K: Eq + Hash`.

Generic `@state` Classes are rejected. A non-generic state Class may contain
fully concrete generic instances. Migration, lifecycle, test, Contract, and
Host boundaries must remain fully concrete. Inline recursive Struct and Enum
layouts and non-converging recursive generic instances are rejected; Class
recursion that preserves its concrete arguments is allowed.

Numeric values expose `abs`, `min`, `max`, `clamp`, and `to_string`. `f32` and
`f64` also expose `floor`, `ceil`, `round`, `sqrt`, `sin`, and `cos`. The old
public type-suffixed functions such as `math::abs_i32` and `core::max_f64` are
not part of the language surface.

## Comments and documentation

Nexa executable and Contract source accept `//` line comments, non-nested
`/* ... */` block comments,
and `///` documentation comments. Documentation comments attach to the next
declaration and remain in the lossless Syntax Tree. Comments and documentation
do not affect Public API, Build, or ABI Fingerprints.

## Bindings and constants

`let` creates a mutable runtime binding. `const` creates a block-local binding
that is initialized exactly once and cannot be rebound. Module-level `const`
continues to declare a compile-time constant:

```nexa
const title = "classic";
let score = 0;
score += 10;
score *= 2;

let cell = Cell { x: 1, y: 2 };
cell.x -= 1;
```

Parameters and fields are mutable by default. Language v3 has no `mut` marker:

```nexa
fn advance(value: i32) -> i32 {
    value += 1;
    return value;
}
```

Nexa supports `+=`, `-=`, `*=`, `/=`, and `%=` on writable places. They use
the same type rules as their binary operators and evaluate a field receiver or
collection index exactly once. Prefix/postfix `++` and `--` remain unsupported.

Binding immutability is shallow. Starting from a `const` value, Struct field
selection remains read-only. Once evaluation reaches a Class, Array, Map, or
Buffer reference, mutations are governed by the referenced type rather than by
the outer `const` binding:

| Value kind | `const` prohibits | `const` permits |
| --- | --- | --- |
| Scalar | replacing the value | — |
| Struct / Enum | replacing the value or mutating an internal Place | — |
| Class | replacing the reference | mutating object fields |
| Array / Map / Buffer | replacing the reference | mutating container contents |
| String | replacing the reference/value | —; String is immutable |

This is not deep const. For example, a Class or Array reached through a field
of a `const` Struct may still be mutated, while replacing that Struct field is
rejected.

Module constants require an explicit type:

```nexa
pub const BASE_SCORE: i32 = 10;
```

Const evaluation accepts scalar and string literals, arithmetic, comparisons,
Boolean operations, other Consts, and Const-safe Tuple, Struct, Enum, Option,
or Result construction. It cannot allocate a Class, Array, Map, Buffer, Task,
Host Handle, Token, Snapshot, or persistent State value, and it cannot invoke
Host or user functions. A public Const's type and evaluated value are part of
the Public API Fingerprint.

## Structs, Enums, and Classes

Structs and Enums are value types. Struct and Class initialization use the same
`Type { ... }` form; the old constructor `new` keyword is not part of Language
v3. Enum construction uses an associated path:

```nexa
struct Cell {
    x: i32,
    y: i32,
}

enum FoodEffect {
    None,
    Grow(i32),
    Teleport {
        cell: Cell,
    },
}

let origin = Cell { x: 0, y: 0 };
let moved = Cell { x: 10, ..origin };
let effect = FoodEffect::Teleport { cell: moved };
```

Classes are sealed, non-null GC reference types with object identity. Struct
values and Class objects use the same brace initializer syntax:

```nexa
class Enemy {
    name: string,
    health: i32,
}

let enemy = Enemy {
    name: "asp",
    health: 100,
};
enemy.health = 80;
```

Both kinds support field initializer shorthand. `Type { name }` is exactly
equivalent to `Type { name: name }`, including when followed by an update:

```nexa
let name = "asp";
let enemy = Enemy { name, health: 100 };
let moved = Enemy { name, ..enemy };
```

Copying a Class value copies its reference. Class equality compares identity;
Struct equality is structural when every field is comparable. An absent Class
reference is represented with `Option<Enemy>`.

Persistent state is Class metadata:

```nexa
@state(version = 1)
class GameState {
    @stable("score")
    score: i32,
}
```

Special functions use attributes such as `@migration`, `@activation`,
`@cleanup`, and `@immediate`. Attribute combinations and their effect
restrictions are checked during analysis.

## Async functions

Asynchronous work is declared with `async fn`. Awaiting is a postfix operation
and is valid only inside an async function:

```nexa
async fn load_profile(id: i64) -> Result<Profile, LoadError> {
    let profile = host::load_profile(id).await?;
    return Result::Ok(profile);
}
```

An async result must be consumed by `.await` in the same expression. Nexa does
not expose Future, Poll, Pin, Waker, or another user-nameable pending type.
`yield` is also restricted to async functions, while `defer` bodies cannot
yield or await.

## Loop control

`break` and `continue` target the innermost `while` or statically bounded
`for`. `for` remains an explicit typed-analysis node instead of being expanded
by the Parser. Function-frame `defer` blocks retain LIFO semantics; a `break`
or `continue` does not execute them because it does not leave the function
frame.

## String interpolation

`${expression}` interpolates a string, integer, float, Boolean, rune, a
recursively formattable `Array<T>`, or a Struct/Class whose complete field
graph is formattable. Formatting is deterministic, locale-free, and follows
field declaration order:

```nexa
let label: string = "score";
let values = [3, 1, 4];
let message: string = "${label}: ${values}"; // score: [3, 1, 4]

let point = Point { x: 3, y: 4 };
let debug: string = "${point}"; // Point { x: 3, y: 4 }
```

Nested arrays use the same representation. A nominal field graph containing a
cycle or a Map, Tuple, Option, Result, Buffer, Set, Host value, or
resource-bearing value is rejected at compile time rather than producing a
partial string or performing unbounded object traversal.

`\${` produces a literal `${`. Conversion is verified, bounded, fuel-metered,
and represented in Source Maps and precise Root Maps.

## Package tests

`@test` declarations and the separate `tests/**/*.nexa` source tree are
specified in [Package Tests](PACKAGE_TESTS.md).
