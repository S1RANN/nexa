# Nexa Contract Language v3

Version: **3.0.0**

Status: **ACTIVE**

The structured syntax version carried by every parsed Contract input is:

```text
CONTRACT_SYNTAX_VERSION: u16 = 3
```

The validated Host Contract schema consumed by Runtime and embedding is:

```text
HOST_CONTRACT_SCHEMA_VERSION: u32 = 2
```

Neither value is a display string or a compatibility range. A Contract v3
frontend accepts only the source profile and grammar in this document.
Contract Syntax v3 does not change the Host schema or Descriptor wire schema:
`HOST_CONTRACT_SCHEMA_VERSION` and `ABI_DESCRIPTOR_VERSION` both remain `2`.

## Source profile and file identity

Nexa has two explicit source profiles:

```rust
pub enum SourceProfile {
    Executable,
    Contract,
}
```

One UTF-8 file whose name ends exactly in `.contract.nexa` defines exactly one
Contract. The Contract profile is selected by the shared source-profile
resolver when either of these is true:

- the source path has the `.contract.nexa` suffix; or
- the project manifest selects that exact path as its `contract` input.

The suffix and manifest selection must agree. A manifest-selected path with a
different suffix is an error, not an alternate Contract spelling. An
unselected `.contract.nexa` file still parses as Contract source, but a project
may resolve only one current Host Contract.

Contract files never enter the executable Source Module Graph and never derive
a `package::...` module identity from their path. Diagnostics, caches, the CLI,
the compiler, the LSP, and editor integrations all use the same profile result
and the same normalized source identity.

Project resolution validates the Contract path before parsing:

1. the file exists and is a regular readable file;
2. its final path component ends in `.contract.nexa`;
3. its canonical path remains inside the allowed project root;
4. neither `..` nor a symbolic-link traversal escapes that root; and
5. no second current Contract is selected for the project.

The source path and URI are diagnostic and cache identity only. They do not
participate in declaration Stable IDs or Contract Descriptor fingerprints.

## Complete example

```nexa
/// Snake game Host ABI.
@stable("snake-api")
contract SnakeApi;

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

enum LoadError {
    Missing,
    Denied,
    Cancelled,
    Abandoned,
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
    @abandon(return_error)
    @capability("profile.read")
    async fn load_profile(id: string) -> Result<Profile, LoadError>;
}

nexa {
    fn on_event(event: SnakeEvent) -> Array<SnakeCommand>;
}
```

The two directions are fixed:

- `host` functions are implemented by the Rust Host and callable by Nexa.
- `nexa` functions are implemented by a Nexa Package and callable by the Host.

Import and export may describe internal compiler or Runtime operations, but
they are not Contract source syntax.

## Lexical rules

The shared lexer reserves `contract`, `host`, `nexa`, and `handle` for the
Contract profile. Identifiers are Unicode-independent ASCII identifiers
matching `[A-Za-z_][A-Za-z0-9_]*`. Primitive type names and all Contract
keywords are reserved. String literals use the common Nexa quoted-string
escape rules. Numeric attribute arguments are unsigned decimal integers.

Contract source accepts:

```nexa
// ordinary line comment
/* ordinary non-nested block comment */
/// documentation for the next declaration
```

Documentation comments are retained in `ContractAst`; ordinary comments
remain in the lossless syntax tree. Comments, documentation text, whitespace,
line endings, and formatting never participate in an ABI fingerprint.

## Grammar

The following grammar is normative modulo separators and trivia:

```text
contract_file   := contract_header contract_item*
contract_header := documentation? attribute* "contract" TypeName ";"

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

`contract <Name>;` must be the first non-comment declaration and must appear
exactly once. Its preceding documentation and attributes attach to the Header.
Every following top-level declaration belongs to that Header's Contract.
There is no enclosing Contract block and therefore no Contract-closing brace.

A Contract may contain at most one `host` block and at most one `nexa` block.
Either block may be absent. Their textual order has no meaning. Type
declarations may appear before, between, or after them, and references resolve
over the complete Contract rather than declaration order.

The parser recovers after an invalid item without discarding later valid
declarations. It reports exact spans for at least:

- a missing Contract Header;
- a missing Header semicolon;
- a repeated Header;
- a Header after another declaration;
- an unsupported Contract top-level declaration; and
- a Contract Header encountered under the Executable profile.

An old extension or enclosing `contract Name { ... }` form is rejected with a
targeted migration diagnostic. It is never parsed by a compatibility grammar.

## Declarations, attributes, and types

Contract, struct, enum, handle, and enum variant names are `PascalCase`. Host
and Nexa function, parameter, and field names are `snake_case`. Names are
unique in their semantic namespace, and declared types cannot collide with a
builtin type constructor.

A function with no `-> type` has Unit result. Unit cannot otherwise be named,
stored in a field, supplied as a parameter, or used as a generic argument.
The complete type surface is:

| Contract type | Meaning |
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
| `Token<T>` | Typed Host resource token; `T` is a declared handle |
| `Snapshot<T>` | Typed immutable snapshot; `T` is a declared struct |
| `NamedType` | A struct, enum, or handle declared by this Contract |

Generic constructors are case-sensitive builtins with exactly the arity shown.
There are no user-defined generics, pointers, nullable references, `void`, or
source-visible request/future types. Async Host request state exists only after
ABI lowering.

`Token<H>` retains the exact Stable ID of declared Handle `H`; two token types
for different Handles are not interchangeable even if their machine layout is
equal. `Snapshot<S>` follows the same rule for declared Struct `S`.

The syntax tree preserves attributes on source-bearing declarations so invalid
placements keep exact spans. `@stable("...")` is valid on the Header, struct,
enum, handle, field, Variant, parameter, and Host or Nexa function. Attributes
on a direction block and all other placements are validation errors.

Host function attributes are:

| Attribute | Placement | Meaning |
| --- | --- | --- |
| `@fuel(n)` | Host `fn` or `async fn` | Non-zero deterministic base Fuel charge |
| `@cancel(return_error)` | Host `async fn` | Complete cancellation as the declared error path |
| `@cancel(cancel_task)` | Host `async fn` | Cancel the waiting Nexa task |
| `@abandon(return_error)` | Host `async fn` | Abandoned request completes as an error |
| `@abandon(trap)` | Host `async fn` | Abandoned request traps |
| `@capability("name")` | Host `fn` or `async fn` | Required Host capability |

Absent `@fuel` normalizes to `1`. Absent async policies normalize to
`@cancel(return_error)` and `@abandon(return_error)`. Capability attributes may
repeat only with distinct, non-empty canonical names and normalize as a sorted
set. Policy and Fuel values affect the Host function fingerprint; source
attribute order does not after normalization. `@cancel` and `@abandon` are
invalid on synchronous Host functions. Host policy attributes are invalid on
Nexa entrypoints.

Async Host functions must return `Result<S, E>`. A normalized
`@cancel(return_error)` requires either `E = i32`, which produces `-2`, or a
declared enum with a unit Variant named exactly `Cancelled`. A normalized
`@abandon(return_error)` requires either `E = i32`, which produces `-1`, or a
declared enum with a unit Variant named exactly `Abandoned`. Payload Variants
do not satisfy these rules. `cancel_task` and `trap` do not require the
corresponding Variant, but the general `Result<S, E>` rule still applies.
Both directions may use `async fn`; async Nexa entrypoints carry the Task
effect in their descriptors.

`@stable("name")` accepts one non-empty string of ASCII alphanumeric bytes or
`_`, `-`, `.`, `:`, `/`. Without it, a Stable ID derives from the Contract,
owner scope, declaration category, and source name. With it, the explicit
string replaces only the terminal source-name component. All resolved IDs are
collision-checked across the complete Contract.

The validator computes every generated Rust identifier before codegen and
rejects case-conversion, keyword, helper, marker/Args/Output/ticket, wrapper,
trait, registry, or Stable-ID collisions. Codegen neither appends suffixes nor
uses raw identifiers to hide an invalid Contract name.

Flattening the source container does not add an owner scope. A declaration's
owner remains the Contract Header Stable ID, so migrating an equivalent
Contract preserves Contract and declaration Stable IDs. Source names remain
descriptor data; `@stable` preserves lookup identity across a rename but does
not hide that source-name change from declaration fingerprints.

## Frontend and semantic model

Contract v3 has one frontend:

```text
UTF-8 source + SourceProfile::Contract
→ nexa-syntax ContractSyntaxTree
→ ContractAst
→ ValidatedContract
→ ContractDescriptor
→ BindingModel
```

`nexa-contract` consumes the shared lossless syntax tree. It owns no second
tokenizer, cursor parser, comment pre-pass, or alternate grammar. Syntax and
semantic diagnostics refer to ranges in the same source snapshot.

`ContractAst` is source-faithful and retains at least:

```text
ContractAst
  source origin and complete source range
  Contract Header
    source name, Stable-ID attribute, documentation, attributes, exact span
  flat declarations
    complete item, name, field, variant, and signature spans
    attributes and documentation
```

It retains original order and names but does not resolve types, synthesize
defaults, or calculate identities. Header documentation attaches only when a
`///` group immediately precedes the Header with no intervening declaration.
Every span is a half-open UTF-8 byte range associated with the exact source
URI. Documentation retention does not make it ABI-significant.

`ValidatedContract` is the only input accepted by Descriptor v2 or binding
construction. Central validation:

1. validates profile, Header, naming, and namespaces;
2. resolves named and generic types and validates arity;
3. rejects infinitely recursive by-value layouts;
4. validates attribute placement, values, defaults, and conflicts;
5. validates direction and async policy;
6. computes and collision-checks generated Rust names;
7. assigns and collision-checks declaration Stable IDs;
8. retains source origins for every validated node; and
9. produces no model when any error remains.

Handles, typed tokens, and snapshots are identity-bearing ABI leaves and break
inline-layout recursion. A struct field, enum payload, `Option`, or `Result`
edge remains part of recursive layout analysis. Cycle diagnostics identify the
participating type references rather than using a synthetic empty span. Stable
IDs are domain-separated by Contract, owner scope, declaration category, and
source or explicit stable name; each declaration category uses a distinct
domain, and a collision between declarations is a hard error with both source
origins.

`ValidatedContract` contains resolved ABI types, normalized attributes,
declaration Stable IDs, prevalidated Rust names, source origins, and the
declaration fingerprints defined by `CONTRACT_DESCRIPTOR_V2.md`. Backends do
not repeat semantic decisions by walking `ContractAst`.

The public names are `ContractSyntaxTree`, `ContractDiagnostic`,
`ContractAst`, `ContractType`, `ContractFunction`, `ContractStruct`,
`ContractEnum`, `ContractHandle`, `parse_contract`, and `validate_contract`.
No public alias exposes the superseded terminology.

## Compatibility boundary

Contract v3 has no compatibility parser, public alias, alternate file suffix,
CLI alias, editor association, or descriptor decoder for the superseded source
surface. An old file receives one migration diagnostic that names the required
`.contract.nexa` suffix and the flat `contract Name;` Header. Migration then
uses the v3 parser and validator only.

The following source forms remain invalid:

```text
interface Name { ... }
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

Historical tags may describe previous surfaces, but active products do not
select or translate them.
