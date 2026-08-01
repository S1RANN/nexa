# Nexa NIDL v2

Version: **2.0.0**

Status: **COMPLETE**

The structured syntax version carried by parsed Contract inputs is:

```text
NIDL_SYNTAX_VERSION: u16 = 2
```

The validated Host Contract schema consumed by Runtime and embedding is:

```text
HOST_CONTRACT_SCHEMA_VERSION: u32 = 2
```

Neither value is a display string or a compatibility range.

This document is the normative source-language and semantic-model contract for
Nexa Interface Definition Language v2. NIDL v2 is an intentional breaking
surface. A v2 frontend does not accept, translate, or preserve the former
`interface`, `opaque`, `sync`, `request`, `export`, `void`, lowercase generic,
or source-visible Request forms.

## Contract direction

One `.nidl` source defines exactly one contract:

```nidl
contract Snake {
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

    enum LoadError {
        Missing,
        Denied,
        Cancelled,
    }

    enum SnakeEvent {
        GameStarted(GameSnapshot),
        GameEnded(GameSnapshot),
    }

    enum SnakeCommand {
        AddScore(i32),
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
        fn on_event(event: SnakeEvent) -> Array<SnakeCommand>;
        fn choose_food_spawn(context: FoodSpawnContext) -> Option<Cell>;
    }
}
```

The two directions are fixed:

- `host` functions are implemented by the Rust Host and callable by Nexa.
- `nexa` functions are implemented by a Nexa Package and callable by the Host.

The words import and export may be used inside compiler or Runtime
implementations, but are not NIDL v2 source syntax.

## Lexical rules

Identifiers are Unicode-independent ASCII identifiers matching
`[A-Za-z_][A-Za-z0-9_]*`. Keywords and primitive type names are reserved.
String literals use the common Nexa quoted-string escape rules. Numeric
attribute arguments are unsigned decimal integers.

NIDL v2 accepts:

```nidl
// ordinary line comment
/* ordinary block comment */
/// documentation for the next declaration
```

Block comments nest according to the shared `nexa-syntax` lexer rules.
Documentation comments are retained in `NidlAst`; ordinary comments remain in
the lossless syntax tree. Comments, documentation text, whitespace, line
endings, and formatting never participate in an ABI fingerprint.

## Grammar

The following grammar is normative modulo separators and trivia:

```text
source          := documentation? attribute* "contract" TypeName
                   "{" contract_item* "}"
contract_item   := documentation? attribute*
                   (struct_decl | enum_decl | handle_decl)
                 | documentation? attribute* host_block
                 | documentation? attribute* nexa_block

host_block      := "host" "{" host_function* "}"
nexa_block      := "nexa" "{" nexa_function* "}"

struct_decl     := "struct" TypeName "{"
                     field ("," field)* ","?
                   "}"
enum_decl       := "enum" TypeName "{"
                     variant ("," variant)* ","?
                   "}"
handle_decl     := "handle" TypeName ";"

field           := documentation? attribute* FieldName ":" type
variant         := documentation? attribute* VariantName
                 | documentation? attribute* VariantName "(" type ")"

host_function   := documentation? attribute*
                   ("async")? "fn" FunctionName signature ";"
nexa_function   := documentation? attribute*
                   ("async")? "fn" FunctionName signature ";"
signature       := "(" parameter_list? ")" ("->" type)?
parameter_list  := parameter ("," parameter)* ","?
parameter       := documentation? attribute* ParameterName ":" type

attribute       := "@" AttributeName
                   ("(" attribute_argument_list? ")")?
attribute_argument_list
                := attribute_argument ("," attribute_argument)* ","?
attribute_argument
                := Identifier | UnsignedDecimal | StringLiteral

documentation   := DocComment+

type            := "i32" | "i64" | "f32" | "f64"
                 | "bool" | "rune" | "string"
                 | "Array" "<" type ">"
                 | "Buffer" "<" type ">"
                 | "Option" "<" type ">"
                 | "Result" "<" type "," type ">"
                 | "Token" "<" type ">"
                 | "Snapshot" "<" type ">"
                 | TypeName
```

A contract may contain at most one `host` block and at most one `nexa` block.
Either block may be absent. Their textual order has no meaning. Type
declarations may appear before, between, or after the blocks; references are
resolved over the complete contract rather than declaration order.

The syntax tree accepts and preserves attributes on source-bearing
declarations so placement errors retain exact spans. `@stable("...")` is valid
on the Contract, a struct, enum, handle, field, Variant, parameter, or Host or
Nexa function. The scheduling and capability attributes listed below are
valid only on Host functions. Attributes on a `host` or `nexa` block, and all
other placements, are validation errors. Documentation may attach to each
source-bearing declaration and remains ABI-insignificant.

NIDL v2 has no `void` type. Omitting `-> type` means Unit. Unit cannot otherwise
be named, stored in a field, supplied as a parameter, or used as a generic
argument.

## Type surface

The spellings in the grammar are the complete v2 type surface:

| NIDL type | Meaning |
| --- | --- |
| `i32`, `i64` | Signed fixed-width integer |
| `f32`, `f64` | IEEE-754 scalar under Nexa deterministic-float rules |
| `bool` | Boolean |
| `rune` | Unicode scalar value |
| `string` | Nexa UTF-8 string value |
| `Array<T>` | Typed Nexa array |
| `Buffer<T>` | Typed copy buffer |
| `Option<T>` | `None` or one `T` |
| `Result<T, E>` | `Ok(T)` or `Err(E)` |
| `Token<T>` | Typed Host resource token; `T` must be a declared `handle` |
| `Snapshot<T>` | Typed immutable snapshot |
| `NamedType` | A struct, enum, or handle declared by this contract |

Generic constructors are always PascalCase and have exactly the arity shown.
They are builtins, not user-defined generics. Bare `Array`, `Token`, or
`Snapshot` is invalid. `array<T>`, `buffer<T>`, `option<T>`, `result<T, E>`,
`token<T>`, and `snapshot<T>` are invalid.

`request<T>`, `host_request<T>`, and `Request<T>` are not source types. An
asynchronous Host request and its completion ticket exist only after ABI
lowering. The declared result of an `async fn` is the value observed by Nexa
after postfix `.await`.

`Token<H>` is never type-erased. Validation resolves `H` to the exact Stable
ID of one declared Handle and rejects every non-Handle argument. ABI lowering
derives a domain-separated resource-token type identity from that Handle
Stable ID. Consequently `Token<Entity>` and `Token<Subscription>` are
different ABI and Runtime value types even when their wrappers have the same
machine layout; neither can be passed, decoded, released, or forged as the
other. The named Handle identity is encoded in function and declaration
descriptors and therefore participates in every affected fingerprint.

Likewise, `Snapshot<S>` requires `S` to be a declared Struct and retains that
Struct's Stable ID as its content identity.

## Attributes

Attributes are preserved in source order by `NidlAst` and normalized by
`ValidatedContract`. Unknown attributes, duplicate singleton attributes,
incorrect placement, incorrect arity, and invalid values are errors.

The v2 Host function attributes are:

| Attribute | Placement | Meaning |
| --- | --- | --- |
| `@fuel(n)` | Host `fn` or `async fn` | Non-zero deterministic base Fuel charge |
| `@cancel(return_error)` | Host `async fn` | Complete cancellation as the declared error path |
| `@cancel(cancel_task)` | Host `async fn` | Cancel the waiting Nexa task |
| `@abandon(return_error)` | Host `async fn` | Abandoned request completes as an error |
| `@abandon(trap)` | Host `async fn` | Abandoned request traps |
| `@capability("name")` | Host `fn` or `async fn` | Required Host capability |

Both Host and Nexa entrypoints may be `async fn`. Only an asynchronous Host
function has cancellation and abandonment policy. Every asynchronous Host
function must return `Result<S, E>`; an asynchronous Nexa entrypoint has its
Task effect encoded in the entrypoint descriptor but does not take
`@cancel`, `@abandon`, `@fuel`, or `@capability`.

Absent `@fuel` normalizes to `1`. Absent async policies normalize to
`@cancel(return_error)` and `@abandon(return_error)`. Capability attributes may
repeat only with distinct, non-empty canonical capability names. Policy and
Fuel values affect the Host function fingerprint; capability names are sorted
as a set before descriptor encoding. Source spelling and attribute order do
not affect identity after normalization.

`@cancel` and `@abandon` on synchronous functions are invalid. Attributes do
not turn a `nexa` entrypoint into required or optional: the Host usage site
selects that policy through typed embedding APIs.

For an asynchronous Host function returning `Result<S, E>`, a normalized
`@cancel(return_error)` requires either:

- `E = i32`, where cancellation produces `-2` (the signed interpretation of
  `u32::MAX - 1`); or
- `E` is a declared enum containing a unit Variant named exactly `Cancelled`.

A normalized `@abandon(return_error)` requires either:

- `E = i32`, where abandonment produces `-1` (the signed interpretation of
  `u32::MAX`); or
- `E` is a declared enum containing a unit Variant named exactly `Abandoned`.

A payload-carrying Variant with either name does not satisfy the rule. When
both policies normalize to `return_error`, an enum error type must contain
both unit Variants. `@cancel(cancel_task)` does not require `Cancelled`, and
`@abandon(trap)` does not require `Abandoned`; the general
`Result<S, E>` requirement for asynchronous Host functions still applies.
Validation points at the explicit policy attribute when present and otherwise
at `E`.

## Stable declaration identity

`@stable("name")` accepts exactly one non-empty string whose bytes are ASCII
alphanumeric or `_`, `-`, `.`, `:`, or `/`. It is valid on the Contract,
struct, enum, handle, field, Variant, parameter, and Host or Nexa function.
It is not valid on a direction block.

Without `@stable`, a declaration Stable ID is derived from its Contract,
owner scope, declaration category, and source name. With `@stable`, the
explicit string replaces the declaration's terminal source-name component;
the Contract, owner scope, and category remain domain separators. Renaming a
declaration with an explicit stable name therefore preserves its lookup
identity only within that same scope and category. The same explicit string
may produce different IDs in different scopes or categories.

Every resolved Stable ID is collision-checked across the complete Contract.
Equality between two distinct declarations is a validation error with both
source origins. Descriptor v2 encodes the resolved Stable ID and source name;
it does not encode the raw `@stable` attribute text a second time. Consequently
`@stable` can preserve lookup identity across a rename while the source-name
change still changes the declaration fingerprint.

## Naming

The validator enforces:

- Contract, struct, enum, handle, and enum variant names are `PascalCase`.
- Host and Nexa function names are `snake_case`.
- Parameters and struct fields are `snake_case`.
- Names are unique in their semantic namespace.
- A type name cannot collide with a builtin type constructor.

The semantic model also computes every generated Rust identifier before
codegen and rejects collisions there. This includes case-conversion
collisions, keyword collisions, and collisions among a marker, its `Args`,
`Output`, ticket, wrapper, trait, or registry names. Codegen does not silently
append suffixes and does not use raw identifiers to hide an invalid NIDL name.

## The sole frontend pipeline

NIDL v2 uses one frontend:

```text
UTF-8 source
→ nexa-syntax NIDL SyntaxTree
→ NidlAst
→ ValidatedContract
```

`nexa-idl` directly consumes the lossless `nexa-syntax` tree. It must not own a
second tokenizer, token enum, cursor parser, comment pre-pass, or alternate
grammar. Syntax diagnostics and all subsequent semantic diagnostics refer to
ranges in the same source snapshot.

## `NidlAst`

`NidlAst` is the source-faithful semantic input. It retains at least:

```text
NidlAst
  source origin and complete source range
  Contract
    source name and span
    attributes and documentation
    type declarations
      complete declaration, name, field, and variant spans
      attributes and documentation
    optional Host block
      block span, functions, signatures, attributes, documentation
    optional Nexa block
      block span, functions, signatures, attributes, documentation
```

Every span is a half-open UTF-8 byte range associated with its source URI.
The model retains original names and ordering. It does not resolve names,
assign ABI types, synthesize defaults, or calculate identities.

Documentation is attached only when a `///` group immediately precedes the
documented item with no intervening item. Keeping documentation in `NidlAst`
does not make it ABI-significant.

## `ValidatedContract`

`ValidatedContract` is the only input accepted by Descriptor v2 construction
or binding-model construction. Validation is one centralized pass that:

1. validates all naming and namespace rules;
2. resolves every named and generic type;
3. rejects unknown types and wrong generic arity;
4. rejects layouts with an infinitely recursive by-value cycle;
5. validates attribute placement, values, defaults, and conflicts;
6. validates function direction and asynchronous policy;
7. computes and collision-checks all Rust names;
8. assigns and collision-checks declaration Stable IDs;
9. retains source origins for every validated node; and
10. produces no output when any error remains.

Handles, typed tokens, and snapshots are identity-bearing ABI leaves and
therefore break an inline-layout recursion. A struct, enum payload, `Option`,
or `Result` edge remains part of recursive layout analysis. Implementations
must diagnose the cycle at the participating type references, not at byte
range `0..0`.

Stable IDs are domain-separated by Contract, owner scope, declaration
category, and the source or explicit stable name. Type, Host function, Nexa
entrypoint, field, Variant, and parameter namespaces use different domains.
Stable-ID equality between distinct validated declarations is a hard error.

`ValidatedContract` contains resolved ABI types, normalized attributes,
declaration Stable IDs, prevalidated Rust names, source origins, and the
declaration-level fingerprints defined by
`CONTRACT_DESCRIPTOR_V2.md`. Backends must not repeat semantic decisions by
walking `NidlAst`.

## Rejected legacy surface

The active v2 parser rejects all of these as syntax rather than translating
them:

```text
interface Snake { ... }
opaque Entity;
sync fn format_score(...);
request(return_error, trap) fn load(...);
export OnEvent(...);
fn stop() -> void;
request<T>
host_request<T>
Request<T>
array<T>
buffer<T>
option<T>
result<T, E>
token<T>
snapshot<T>
```

Historical tags and documents may describe those forms, but they are not
compatibility inputs to NIDL v2.
