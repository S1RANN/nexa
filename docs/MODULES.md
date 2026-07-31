# Nexa Source Modules

Status: M4 COMPLETE

An Application Package has exactly one `source_root = "src"` and one Entry
Module. A module declaration is required in every `.nexa` source file and must
match its package-relative path:

```text
src/food/effects.nexa
```

```nexa
module food.effects;
```

Module segments contain only lowercase ASCII letters, digits, and underscores.
The loader rejects duplicate modules, case-folding collisions, non-UTF-8
paths, symlinks, root escapes, nested source roots, and declarations that do
not match their path.

## Imports

Imports always bind a namespace:

```nexa
import food.types;
import shared.math as math;
import snake_common.score as score;
import host as snake;
```

The first form binds `types`; `as` changes the local namespace. A dependency
alias is the first segment of an imported dependency path. `host` is a
reserved external module and must have an explicit alias. Wildcard, selective,
conditional, dynamic, and re-export imports are not supported.

Module cycles are rejected. Cycle diagnostics report the complete normalized
cycle chain.

## Visibility

- No modifier: visible only inside the declaring Module.
- `pub(package)`: visible to all Modules in the current Package.
- `pub`: visible to the Package and static dependency consumers.

Members inherit the visibility of their enclosing nominal type. A public
signature cannot expose a less-visible type. Required Exports and lifecycle
functions can only be declared by the Application Entry Module and must be
`pub`.

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
Stateful fields. Its value must match
`[A-Za-z][A-Za-z0-9._-]{0,127}` and be unique within the Package. Analysis
rejects both duplicate explicit identities and collisions between distinct
canonical identities that truncate to the same Runtime 64-bit ID.

Function signatures are not part of `StableSymbolId`. Public signatures,
effects, layouts, and evaluated public Const values instead contribute to the
256-bit Public API Fingerprint.

## Runtime boundary

Source Modules are a compile-time organization mechanism. The compiler
deterministically links the Application and its static Library closure into
one Bytecode Module and one Package Artifact. The Runtime continues to use one
Realm, one Epoch, and Package-level Restart Reload.
