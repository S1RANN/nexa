# Nexa M4 Language Additions

Status: M4 COMPLETE

M4 adds small language features needed to keep multi-file Packages readable.
It does not add user generics, traits, closures, macros, dynamic dispatch, or
reflection.

## Comments and documentation

Nexa source accepts `//` line comments, non-nested `/* ... */` block comments,
and `///` documentation comments. Documentation comments attach to the next
declaration and remain in the lossless Syntax Tree, but do not affect Public
API or Build Fingerprints.

NIDL remains comment-free in M4.

## Typed constants

Top-level constants require an explicit type:

```nexa
pub const BASE_SCORE: i32 = 10;
```

Const evaluation accepts literals, arithmetic, Boolean comparisons, other
Consts, and pure Struct or Enum construction. It cannot invoke user
functions. A public Const's type and evaluated value are part of the Public
API Fingerprint.

## Loop control

`break` and `continue` target the innermost `while` or statically bounded
`for`. `for` remains an explicit typed-analysis node instead of being expanded
by the Parser. Function-frame `defer` blocks retain LIFO semantics; a
`break` or `continue` does not execute them because it does not leave the
function frame.

## String interpolation

`${expression}` interpolates a string, integer, float, Boolean, or rune using
deterministic, locale-free formatting:

```nexa
let label: string = "score";
let message: string = "${label}: ${score}";
```

`\${` produces a literal `${`. Bytecode v5 verifies and meters scalar
conversion instructions like every other instruction, and includes them in
Source Maps and Root Maps.

## Package tests

`@test` declarations and the separate `tests/**/*.nexa` source tree are
specified in [Package Tests](PACKAGE_TESTS.md).
