# Nexa v2 Language Surface

Status: M4R1 COMPLETE

Nexa v2 defines one source language for Packages, standalone programs, and
REPL cells. It deliberately does not add user-defined generics, traits,
closures, dynamic dispatch, inheritance, operator overloading, macros,
reflection, raw pointers, or nullable references.

## Naming and paths

Functions, parameters, fields, and local bindings use `snake_case`. Types and
Enum Variants use `PascalCase`; Consts use `SCREAMING_SNAKE_CASE`.

`::` selects a namespace, associated item, or Enum Variant. `.` selects a
field, method, or postfix operation:

```nexa
use std::core;

let limit = core::max_i32(8, MAX_SCORE);
let effect = FoodEffect::Grow(limit);
let label = effect.name;
```

Source files do not declare their Module. Their package-relative path is the
Module identity; see [Source Modules](MODULES.md).

## Comments and documentation

Nexa and NIDL accept `//` line comments, non-nested `/* ... */` block comments,
and `///` documentation comments. Documentation comments attach to the next
declaration and remain in the lossless Syntax Tree. Comments and documentation
do not affect Public API, Build, or ABI Fingerprints.

## Bindings and constants

`let` creates a runtime local binding that cannot be rebound. `let mut` permits
rebinding and permits writes through a Struct place:

```nexa
let title = "classic";
let mut score = 0;
score += 10;
score *= 2;

let mut cell = Cell { x: 1, y: 2 };
cell.x -= 1;
```

Nexa supports `+=`, `-=`, `*=`, `/=`, and `%=` on writable places. They use
the same type rules as their binary operators and evaluate a field receiver or
collection index exactly once. Prefix/postfix `++` and `--` remain unsupported.

Binding mutability is shallow. A Class field can be changed only when that
field itself is declared `mut`, regardless of whether the binding is mutable.

Consts exist only at Module scope and require an explicit type:

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

Structs and Enums are value types. Struct creation does not use `new`, and
Enum construction uses an associated path:

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

Classes are sealed, non-null GC reference types with object identity. They
must be created with `new`; their field mutability is declared independently:

```nexa
class Enemy {
    name: string,
    mut health: i32,
}

let enemy = new Enemy {
    name: "asp",
    health: 100,
};
enemy.health = 80;
```

Copying a Class value copies its reference. Class equality compares identity;
Struct equality is structural when every field is comparable. An absent Class
reference is represented with `Option<Enemy>`.

Persistent state is Class metadata:

```nexa
@state(version = 1)
class GameState {
    @stable("score")
    mut score: i32,
}
```

Special functions use attributes such as `@migration`, `@activation`,
`@cleanup`, and `@immediate`. Attribute combinations and their effect
restrictions are checked during analysis.

## Async functions

Asynchronous work is declared with `async fn`. Awaiting is a postfix operation
and is valid only inside an async function:

```nexa
use host::snake;

async fn load_profile(id: i64) -> Result<Profile, LoadError> {
    let profile = snake::load_profile(id).await?;
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

`${expression}` interpolates a string, integer, float, Boolean, rune, or an
`Array<T>` whose elements are recursively formattable. Formatting is
deterministic and locale-free:

```nexa
let label: string = "score";
let values = [3, 1, 4];
let message: string = "${label}: ${values}"; // score: [3, 1, 4]
```

Nested arrays use the same representation. Nominal objects, Maps, Tuples,
Host values, and resource-bearing values are rejected rather than exposing
object internals or risking recursive object traversal.

`\${` produces a literal `${`. Conversion is verified, bounded, fuel-metered,
and represented in Source Maps and precise Root Maps.

## Package tests

`@test` declarations and the separate `tests/**/*.nexa` source tree are
specified in [Package Tests](PACKAGE_TESTS.md).
