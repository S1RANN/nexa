# NIDL v2

Status: M4R1 NIDL v2 COMPLETE

NIDL defines the typed boundary between a Rust Host and Nexa packages. Version
2 uses one `contract` with explicit call-direction blocks:

```text
host { ... }  Rust implements; Nexa calls
nexa { ... }  Nexa packages implement; Rust calls
```

Neither block means a network endpoint. Both describe calls inside the Host's
verified runtime boundary.

## Complete example

```nidl
contract Snake {
    /// One board coordinate.
    struct Cell {
        x: i32,
        y: i32,
    }

    struct GameSnapshot {
        score: i32,
    }

    struct Profile {
        display_name: string,
    }

    struct FoodSpawnContext {
        width: i32,
        height: i32,
    }

    struct FoodEatenContext {
        cell: Cell,
    }

    enum LoadError {
        Missing,
        Denied,
        Cancelled,
    }

    enum FoodEffect {
        None,
        Grow(i32),
    }

    enum Event {
        GameStarted(GameSnapshot),
        FoodEaten(GameSnapshot),
        GameEnded(GameSnapshot),
    }

    enum Command {
        AddScore(i32),
        ResizeSnake(i32),
        ShowToast(string),
    }

    handle Entity;

    host {
        fn format_score(score: i32) -> string;

        @fuel(8)
        @cancel(return_error)
        @abandon(trap)
        @capability("profile.read")
        async fn load_profile(
            id: string,
        ) -> Result<Profile, LoadError>;
    }

    nexa {
        fn on_event(event: Event) -> Array<Command>;
        fn choose_food_spawn(context: FoodSpawnContext) -> Option<Cell>;
        fn calculate_food_effect(context: FoodEatenContext) -> FoodEffect;
    }
}
```

A contract contains at most one `host` block and at most one `nexa` block.
Their textual order has no semantic meaning. Type declarations may appear
alongside the blocks and may reference declarations later in the same
contract. One source defines exactly one contract.

## Declarations and names

NIDL v2 supports:

```nidl
struct Position {
    x: f32,
    y: f32,
}

enum LookupError {
    Missing,
    Denied(string),
    Invalid(string),
}

handle Entity;
```

Contracts, structs, enums, handles, and variants use `PascalCase`. Functions,
parameters, and fields use `snake_case`. Name validation happens before code
generation; the generator never guesses a corrected spelling. Distinct source
names that map to the same Rust identifier are rejected with source spans.
Identifiers are ASCII and match `[A-Za-z_][A-Za-z0-9_]*`.
A declared type name cannot collide with a built-in type constructor.

Struct fields, enum variants, and record-payload fields use commas and may
have a trailing comma. Function declarations end in semicolons. A function
with no result omits `->`; its semantic result is Unit:

```nidl
host {
    fn log(message: string);
}
```

## Types

The complete surface spelling is:

```text
i32 i64 f32 f64 bool rune string
Array<T>
Buffer<T>
Option<T>
Result<T, E>
Token<T>
Snapshot<T>
NamedType
```

Generic wrapper names are case-sensitive. User-defined generic declarations,
`void`, nullable references, pointers, and surface request/Future types are
not part of NIDL v2. In particular, asynchronous Host calls still lower to a
runtime completion ticket, but the contract never names `request<T>`,
`host_request<T>`, or `Request<T>`.

Omitted return types lower to Unit. Unit has no source type name and cannot be
used as a parameter, field, or generic argument.

Recursive value layouts are rejected. Use a `handle` when the Host must carry
an opaque identity rather than placing an unbounded recursive value into the
ABI.

`Token<H>` requires `H` to be a declared `handle`. The generated and Runtime
token type identity is derived from that exact Handle Stable ID, so
`Token<Entity>` cannot be substituted for `Token<Subscription>` even though
both are represented by resource handles. Decode, dispatch, and release all
validate the typed identity. `Snapshot<S>` similarly requires a declared
Struct and retains its exact content identity.

## Host functions and attributes

A synchronous Host call uses `fn`; an asynchronous Host call uses
`async fn`:

```nidl
host {
    @fuel(2)
    @capability("console.write")
    fn write_line(message: string);

    @fuel(8)
    @cancel(return_error)
    @abandon(trap)
    async fn load(id: string) -> Result<string, i32>;
}
```

Supported attributes are:

| Attribute | Applies to | Meaning |
| --- | --- | --- |
| `@fuel(N)` | Host `fn` and `async fn` | Deterministic call charge; `N` is a positive `u32`. |
| `@capability("name")` | Host `fn` and `async fn` | Capability required before dispatch. |
| `@cancel(return_error)` | Host `async fn` | Resolve cancellation through the declared error result. |
| `@cancel(cancel_task)` | Host `async fn` | Cancel the owning task. |
| `@abandon(return_error)` | Host `async fn` | Resolve an abandoned request through the declared error result. |
| `@abandon(trap)` | Host `async fn` | Trap when the request is abandoned. |

Unknown attributes, duplicate arguments, invalid values, and attributes on an
unsupported declaration are validation errors. A `nexa` entrypoint does not
take Host scheduling attributes.

Absent `@fuel` normalizes to 1. An asynchronous function without explicit
policies normalizes to `@cancel(return_error)` and
`@abandon(return_error)`. `@capability` may repeat for distinct, non-empty
canonical names. Attribute source order has no ABI meaning after
normalization; normalized fuel, policies, and capability set are ABI
significant.

Every asynchronous Host function returns `Result<S, E>`. For
`@cancel(return_error)`, `E` must be `i32` or an enum containing a unit
`Cancelled` variant. For `@abandon(return_error)`, `E` must be `i32` or an enum
containing a unit `Abandoned` variant. If both policies use `return_error`, an
enum must contain both variants. A payload-carrying variant does not qualify.
With `i32`, cancellation is `-2` and abandonment is `-1`.
`@cancel(cancel_task)` and `@abandon(trap)` do not require the corresponding
variant.

`@stable("name")` is the other NIDL declaration attribute. It is valid on the
contract, struct, enum, handle, field, variant, parameter, and Host or Nexa
function. It fixes that declaration's lookup identity within its owner scope
and category. The non-empty name may contain ASCII alphanumeric bytes and
`_`, `-`, `.`, `:`, or `/`. Every resolved ID is collision-checked; the
descriptor encodes the resolved ID rather than the raw attribute text.

## Nexa entrypoints

Functions in `nexa {}` define legal typed entrypoint signatures. The contract
does not label them required or optional:

```nidl
nexa {
    fn on_event(event: Event) -> Array<Command>;
    fn inspect_state() -> Option<string>;
    async fn refresh_state() -> Option<string>;
}
```

NIDL v2 `nexa` entrypoints may use `fn` or `async fn`. The latter carries the
Task effect in its entrypoint fingerprint. Nexa entrypoints do not take Host
cancellation, abandonment, fuel, or capability attributes.

The Rust Host chooses required entrypoints with
`NexaEngineBuilder::require_export::<E>()`. A package may omit any other
entrypoint. The Host queries and calls optional entries with typed marker APIs:

```rust
engine.has_export::<generated::InspectState>(&package_id);
engine.call_optional::<generated::InspectState>(&package_id, &args);
engine.dispatch_optional::<generated::InspectState>(&args);
```

If a package implements a declared entrypoint with the wrong signature,
package loading fails. Missing optional entrypoints are valid.

## Comments and source locations

NIDL v2 accepts line comments, block comments, and documentation comments:

```nidl
// Implementation note.
/* A longer note. */
/// Visible documentation for the following declaration.
```

The lossless syntax tree and NIDL AST retain exact URI, byte span, attributes,
and documentation. Ordinary comments, documentation text, whitespace, and
formatting do not affect ABI identity. Block comments follow the shared
lexer's nesting rules.

## Frontend and validation

NIDL v2 has one frontend:

```text
UTF-8 source
→ nexa-syntax NIDL SyntaxTree
→ NidlAst
→ ValidatedContract
```

`NidlAst` is source-faithful and retains original order, names, spans,
documentation, and attributes. `ValidatedContract` resolves types, rejects
recursive by-value layouts, normalizes attributes, validates naming and Rust
name collisions, assigns Stable IDs, and calculates declaration
fingerprints. Descriptor construction and binding generation accept only a
successful `ValidatedContract`; neither backend repeats semantic decisions
from raw source.

## ABI descriptor and fingerprints

Validation produces an ABI Descriptor v2 using structured binary encoding:

```text
version tag
fixed enum tags
length-prefixed strings
ordered parameters
ordered fields
ordered variants
typed attributes and values
```

Each declaration, full-contract, and effective-contract input begins with a
length-prefixed `nexa.contract-descriptor`, a `u32` encoding of descriptor
version 2, and one of the exact domains `type-layout`, `host-function`,
`nexa-entrypoint`, `full-contract`, or `effective-contract`. All string,
byte-array, and sequence lengths are `u32`. The fingerprint is BLAKE3 of those
complete bytes with no second envelope.

Formatted source text is never used directly as hash input. These order rules
are part of the ABI:

| Construct | Order behavior |
| --- | --- |
| Struct fields | source order is semantic |
| Enum variants | source order is semantic |
| Function parameters | source order is semantic |
| Top-level declarations | source order is ignored |
| `host` / `nexa` blocks | source order is ignored |
| Normalized attributes and capabilities | canonical set order |
| Comments and formatting | never semantic |

Each type layout, Host function, and Nexa entrypoint has its own 32-byte
BLAKE3 fingerprint with a descriptor-version and domain prefix. The contract
fingerprint describes the complete validated contract. An effective package
fingerprint includes only the shared type closure and Host surface it uses,
the entrypoints the Host requires from it, and optional entrypoints it actually
implements. Adding an unrelated optional entrypoint therefore does not
invalidate every package.

Stable IDs are lookup identities; fingerprints are semantic change identities.
They are independently collision-checked, and one is not obtained by
truncating the other.

Generated `CONTRACT_DESCRIPTOR` is the complete framed `full-contract` input;
`CONTRACT_FINGERPRINT` is its 32-byte BLAKE3 digest.

## Rust binding generation

The public programmatic pipeline is:

```rust
let ast = nexa_idl::parse_ast(source)?;
let contract = nexa_idl::validate(&ast)?;
let descriptor = nexa_idl::abi_descriptor(&contract);
let fingerprint = nexa_idl::contract_fingerprint(&contract);
let rust = nexa_idl::generate_rust(&contract)?;
```

`nexa_idl::parse(source)` combines parsing and validation. Build scripts use:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    nexa_idl::build::generate("app_api.nidl")?;
    Ok(())
}
```

Binding generation is deterministic and structured:

```text
ValidatedContract
→ BindingModel
→ proc_macro2 TokenStream
→ syn validation
→ prettyplease formatting
→ syn re-validation
→ OUT_DIR
```

For `contract Snake`, generated Rust includes:

- the `SnakeHost` trait and `GeneratedHostRegistry<H>`;
- exact `SOURCE`, canonical `CONTRACT_DESCRIPTOR`,
  `CONTRACT_RUNTIME_ID`, and the 32-byte `CONTRACT_FINGERPRINT`;
- source types with their validated Rust identifiers;
- `<Handle>Token` and `<Struct>Snapshot` wrappers for typed `Token<Handle>`
  and `Snapshot<Struct>` values;
- `OnEvent`, `OnEventArgs`, and `OnEventOutput` for `on_event`;
- `OnEvent::NAME == "on_event"`.

Generated bindings contain no legacy export alias, canonical string hash, or
function-index constant. See [EMBEDDING.md](EMBEDDING.md) for Engine setup and
Host dispatch.

## Migration from the old surface

NIDL v2 intentionally has no compatibility parser:

| Removed surface | NIDL v2 |
| --- | --- |
| `interface Name` | `contract Name` |
| `opaque Entity` | `handle Entity` |
| `sync fn` | `fn` |
| `request(...) fn` | attributes plus `async fn` |
| `export Name(...)` | a function in `nexa {}` |
| `array<T>` | `Array<T>` |
| `void` | omit the return arrow |

Old syntax is diagnosed; it is not silently translated. Regenerate Rust
bindings and rebuild the Host whenever the effective Host contract changes.
