# Nexa Source Modules

Status: M4R1 COMPLETE

An Application Package has exactly one `source_root = "src"` and one Entry
Module. A source file's package-relative path is its Module identity; no source
declaration is required:

```text
src/food/effects.nexa
→ package::food::effects
```

Path segments contain only lowercase ASCII letters, digits, and underscores.
The loader rejects duplicate Modules, case-folding collisions, non-UTF-8
paths, symlinks, root escapes, nested source roots, and paths with invalid
segments.

## Use declarations

`use` binds a namespace:

```nexa
use package::food::types;
use package::shared::math as math;
use self::helpers;
use super::shared;
use snake_common::score;
use host::snake;
use std::string as text;
```

Without `as`, the final path segment is the local namespace. The supported
roots are:

- `package::` for the current Package.
- `self::` for the current Module.
- `super::` for the parent Module. It cannot escape the Package root.
- `host::` for the current Host Contract.
- `std::` for the standard library.
- A Manifest dependency alias for a statically linked dependency Package.

Wildcard selection, selective use, re-export, dynamic loading, and conditional
use are not supported.

Module cycles are rejected. Cycle diagnostics report the complete normalized
cycle chain.

## Visibility

- No modifier: visible only inside the declaring Module.
- `pub(package)`: visible to all Modules in the current Package.
- `pub`: visible to the Package and static dependency consumers.

Members inherit the visibility of their enclosing nominal type. A public
signature cannot expose a less-visible type. Contract-declared Nexa entrypoints
and lifecycle-attributed functions can only be implemented by the Application
Entry Module and must be `pub`.

## Stable symbol identity

Analysis assigns two different identities:

- `DefinitionId` is compact and valid only inside one Analysis Revision.
- `StableSymbolId` is derived from Package ID, Module Path, Symbol Kind, and
  Symbol Name.

Moving or renaming a public or Package-visible symbol normally changes its
stable identity. Use an explicit identity when persistence or an external
contract requires continuity:

```nexa
@stable("classic-score-policy")
pub fn calculate_score(value: i32) -> i32 {
    return value;
}
```

`@stable` is valid on `pub` and `pub(package)` top-level declarations and on
fields of an `@state` Class. Its value must match
`[A-Za-z][A-Za-z0-9._-]{0,127}` and be unique within the Package. Analysis
rejects both duplicate explicit identities and collisions between distinct
canonical identities that truncate to the same Runtime 64-bit ID.

Function signatures are not part of `StableSymbolId`. Public signatures,
effects, type kind, field mutability, layouts, evaluated public Const values,
and Contract entrypoint signatures instead contribute to the 256-bit Public API
Fingerprint.

## Runtime boundary

Source Modules are a compile-time organization mechanism. The compiler
deterministically links the Application and its static Library closure into
one Bytecode Module and one Package Artifact. The Runtime continues to use one
Realm, one Epoch, and Package-level Restart Reload.
