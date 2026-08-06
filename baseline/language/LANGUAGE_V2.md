# Nexa Language v2

Version: **2**

Status: **COMPLETE**

The structured language version carried in build inputs, fingerprints, and
artifacts is:

```text
NEXA_LANGUAGE_VERSION: u16 = 2
```

It is not a display string or a SemVer compatibility range.

This document is the normative source-language specification for the Nexa v2
surface, module model, lossless syntax/AST contract, bindings, constants, and
declaration attributes. `COMPLETE` means these rules and their repository-wide
implementation and conformance evidence are frozen for M4R1.

The terms **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are normative.

## 1. Source text and lossless syntax

A Nexa source is UTF-8. A byte order mark, when accepted by the source loader,
is trivia and is not part of any identifier or semantic fingerprint. Source
ranges are half-open UTF-8 byte ranges. Diagnostics and syntax/AST nodes MUST
retain enough source identity and byte-range information to map back to the
original file without reparsing generated text.

The syntax layer is lossless:

- whitespace, line endings, ordinary comments, and documentation comments
  remain present in the syntax tree;
- tokens keep their original spelling and byte range;
- malformed input produces recovery nodes and diagnostics rather than
  discarding later declarations;
- trivia never changes name resolution, type checking, ABI identity, or a
  semantic fingerprint;
- AST construction MUST NOT perform name resolution or type inference.

ASCII identifiers use:

```ebnf
identifier       = ( "A" … "Z" | "a" … "z" | "_" ),
                   { "A" … "Z" | "a" … "z" | "0" … "9" | "_" } ;
```

Non-ASCII source text is allowed in strings, runes, and comments, but a
source-language identifier is ASCII. A diagnostic for an invalid identifier
MUST select the invalid token or code point, not the whole file.

## 2. Reserved words

The complete Nexa v2 keyword set is:

```text
as async await break class const continue defer else enum false fn
for if in let match mut new package pub return struct true use while
yield
```

No other word is a Nexa v2 keyword. In particular, these v1 spellings are not
keywords and do not introduce any declaration or expression:

```text
var module import task immediate migration activation cleanup stateful with
```

The lexer MUST either return those spellings as ordinary identifiers or, where
their surrounding tokens unambiguously form a removed v1 production, let the
parser issue a targeted legacy-syntax diagnostic. They MUST NOT remain hidden
aliases for v2 syntax.

The following removed forms are always rejected:

```nexa
var value = 1;
module ui.score;
import ui.commands;
task fn load() {}
await load();
stateful class State {}
migration fn migrate() {}
activation fn activate() {}
cleanup fn cleanup() {}
immediate fn score() -> i32 {}
value with { field: replacement }
```

## 3. Punctuation and path operators

`::` and `..` are distinct tokens. The lexer uses maximal munch, so neither is
returned as two shorter tokens.

```text
::  namespace qualification, type-associated items, and enum variants
.   field access, method access, and named postfix operations
..  ranges and struct/class update bases
```

Those roles are not interchangeable:

```nexa
snake::SnakeEvent       // namespace qualification
FoodEffect::Grow(2)     // enum constructor
Array::new()            // associated call
enemy.health            // field access
load().await             // postfix await
0..MAX_SCORE             // range
Cell { x: 2, ..cell }    // update base
```

A parser MUST NOT reinterpret a `.`-separated name as a module path or a
`::`-separated name as field access.

## 4. Naming

The following naming classes are mandatory:

| Entity | Required class |
|---|---|
| Function, parameter, field, local binding, module segment, `use` alias | `snake_case` |
| Struct, enum, class, enum variant, contract, named ABI type | `PascalCase` |
| Module `const` | `SCREAMING_SNAKE_CASE` |

For deterministic validation, the classes are:

```text
snake_case           [a-z][a-z0-9]*(?:_[a-z0-9]+)*
PascalCase           [A-Z][A-Za-z0-9]*
SCREAMING_SNAKE_CASE [A-Z][A-Z0-9]*(?:_[A-Z0-9]+)*
```

An underscore-only name and names with leading, trailing, or repeated
underscores do not satisfy a naming class. The compiler MUST emit a stable
diagnostic on the declared name. Generators MUST NOT silently rename Nexa
declarations or guess an intended spelling.

## 5. Modules are derived from source paths

A Package source file contains no module declaration. A production
`SourceUnit` obtains its only module identity from
`SourceUnit::expected_module_path()`:

```text
<source-root>/main.nexa             -> package::main
<source-root>/ui/score_overlay.nexa -> package::ui::score_overlay
```

The source root itself is not a module segment. Each path component and file
stem MUST be a valid `snake_case` module segment. Normalization MUST reject
duplicate module identities, case-folding collisions, non-UTF-8 paths,
symlinks, root escapes, and `.` or `..` path components.

There is no source/module consistency check because there is no source module
declaration. Virtual snippets, scripts, tests, and REPL cells MUST receive an
explicit synthetic source identity from their compilation profile; they still
cannot declare one in source.

The default stable identity of a declared symbol is derived from:

```text
Package ID
Module Path
Symbol Kind
Symbol Name
```

Moving a file therefore changes the default identity of its symbols. An
eligible explicit `@stable` identity can preserve the identity when continuity
is intentional.

## 6. Paths and `use`

### 6.1 Grammar

```ebnf
use-declaration = "use", use-path, [ "as", identifier ], ";" ;

use-path        = rooted-path | dependency-path ;

rooted-path     = path-root, "::", identifier,
                  { "::", identifier } ;

path-root       = "package" | "self" | "super" | "host" | "std" ;

dependency-path = identifier, "::", identifier,
                  { "::", identifier } ;
```

A `use` path has at least one segment after its root or dependency alias.
Every path segment, the path root, the optional alias, and the full declaration
have independent source spans in the AST.

Supported forms are:

```nexa
use package::food::effects;
use package::food::effects as effects;
use self::helpers;
use super::shared;
use host::snake;
use std::math;
use snake_common::score;
```

Wildcard imports, selective imports, re-exports, dynamic imports, and
conditional imports are not Nexa v2 features.

### 6.2 Root meaning

The following words have special path-root meaning only at the beginning of a
path:

| Root | Resolution base |
|---|---|
| `package::` | root of the current Package |
| `self::` | current Module |
| `super::` | parent of the current Module |
| `host::` | Host contracts available to this Package |
| `std::` | Nexa standard library |
| `<alias>::` | exact local static dependency named by the Package manifest |

`super::` MUST NOT traverse above the Package root. A dependency alias is not
inferred from a Package ID and MUST exist in the resolved manifest/lock input.
Host, standard-library, dependency, and source-module namespaces are distinct;
an ambiguous local binding is an error.

A successful `use` binds one namespace in the importing Module. Without `as`,
the bound name is the final path segment; with `as`, it is the alias. A
duplicate binding is rejected even if both declarations resolve to the same
target. A `use` declaration does not copy declarations and does not make them
public.

Resolution is category-aware:

- `::` between namespace segments resolves a module or declared namespace;
- `Type::item` resolves an associated constructor or enum variant;
- `value.field` resolves a field;
- `value.method(...)` resolves a method or intrinsic method surface.

A resolver MUST NOT fall back from one category to another merely because a
spelling exists.

## 7. File structure and compilation profiles

The common AST shape is:

```rust
NexaAst {
    uses: Vec<UseDeclaration>,
    declarations: Vec<Declaration>,
    top_level_statements: Vec<Statement>,
}
```

It contains no `ModuleDeclaration` and no `ImportDeclaration`.

Package mode permits only:

```text
use declarations
module const declarations
struct declarations
enum declarations
class declarations
function declarations
```

Top-level executable statements parsed in Package mode MUST remain in
`top_level_statements` so diagnostics retain exact source order and spans, then
analysis MUST reject them.

Script and REPL profiles use the same parser and AST. They MAY accept top-level
statements and lower them according to their own execution profile. Parsing
MUST preserve the exact relative order of all top-level statements; the parser
must not eagerly synthesize a function.

## 8. Declaration grammar

The v2 declaration surface relevant to this specification is:

```ebnf
declaration       = { attribute }, [ visibility ],
                    ( function-declaration
                    | const-declaration
                    | struct-declaration
                    | enum-declaration
                    | class-declaration ) ;

visibility        = "pub" | "pub", "(", "package", ")" ;

function-declaration
                  = [ "async" ], "fn", identifier,
                    "(", [ parameter-list ], ")",
                    [ "->", type ],
                    block ;

parameter-list    = parameter, { ",", parameter }, [ "," ] ;
parameter         = identifier, ":", type ;

const-declaration = "const", identifier, ":", type, "=", expression, ";" ;

struct-declaration
                  = "struct", identifier, "{",
                    { struct-field }, "}" ;

struct-field      = { attribute }, identifier, ":", type, "," ;

enum-declaration  = "enum", identifier, "{",
                    { enum-variant }, "}" ;

enum-variant      = { attribute }, identifier,
                    [ "(", [ type-list ], ")"
                    | "{", { record-payload-field }, "}" ],
                    "," ;

record-payload-field
                  = identifier, ":", type, "," ;

class-declaration = "class", identifier, "{",
                    { class-field }, "}" ;

class-field       = { attribute }, [ "mut" ],
                    identifier, ":", type, "," ;
```

Fields and variants use commas, and a final comma is supported. Semicolons
terminate statements and `const` declarations. A function without `-> type`
returns Unit; `void` is not a Nexa source type.

`mut` is accepted on class fields only. A `mut` struct field or enum payload
field is a semantic error located on `mut`.

No v2 declaration production contains `module`, `import`, `task`, `immediate`,
`migration`, `activation`, `cleanup`, or `stateful`.

## 9. AST semantic contract

The AST MUST preserve semantic token shapes and source ranges at least at this
granularity:

```rust
UseDeclaration {
    root: PathRoot,
    segments: Vec<Identifier>, // every Identifier has its own span
    alias: Option<Identifier>,
    range: Span,
}

Bind {
    mutable: bool,
    name: Identifier,
    ty: Option<TypeRef>,
    value: Expression,
    range: Span,
}

FieldDeclaration {
    mutable: bool,
    attributes: Vec<Attribute>,
    name: Identifier,
    ty: TypeRef,
    range: Span,
}

ExpressionKind::Await {
    operand: Box<Expression>,
}
```

All declarations, parameters, fields, variants, attributes, attribute
arguments, expressions, and statements carry their complete span. Error
recovery MUST use explicit error nodes or kinds, not fabricated valid names.

The type-declaration kind set is exactly:

```text
Struct
Enum
Class
```

Persistent state is metadata on a `Class`; it is not a fourth type kind.
Function asynchrony is an effect/flag on an ordinary function declaration.
Migration, activation, cleanup, and immediate roles are attributes, not
function kinds.

## 10. Bindings and assignment

### 10.1 Grammar

```ebnf
binding-statement = "let", [ "mut" ], identifier,
                    [ ":", type ], "=", expression, ";" ;
```

An initializer is mandatory. `var` is never accepted as an alternative.

### 10.2 `let`

`let` creates a runtime local binding. The binding cannot be assigned a new
value after initialization. It may occupy a register or frame slot and may be
an exact GC root. Binding immutability is not deep immutability:

- a value-type field can be changed only through a writable Place;
- a class reference in an immutable binding can still mutate a field declared
  `mut`;
- mutation through another alias remains visible where reference semantics
  apply.

### 10.3 `let mut`

`let mut` creates a rebindable runtime local. It also makes a Struct Place
rooted in that local writable, subject to the field/path rules in Object Model
v2. It does not override a class field's declaration-level mutability.

An assignment diagnostic MUST distinguish:

```text
immutable binding/rebinding
non-writable Struct Place
immutable Class field
type mismatch
```

and select the assigned Place as its primary span.

## 11. Module constants

A `const` declaration:

- occurs only at Module scope;
- has an explicit type;
- is fully evaluated during analysis;
- has no runtime local slot, mutable storage, Host effect, or GC identity.

Local `const` is rejected.

Const-safe result and intermediate types are:

```text
scalar values
string literals
Struct values
Enum values
Tuple values
Option values
Result values
```

These are never const-safe:

```text
Class
Array
Map
Buffer
Task or an internal awaitable
Host Handle
Token
Snapshot
StateHandle
```

The const evaluator supports:

```text
literals
basic arithmetic
comparison and boolean operations
references to already resolved const declarations
Struct, Enum, Tuple, Option, and Result construction
```

It MUST reject Host calls, system time, randomness, arbitrary user functions,
runtime allocation, unbounded loops, cyclic const dependencies, overflow
outside the declared scalar semantics, and any non-const-safe intermediate or
result. Evaluation is deterministic and bounded. The declared type and
evaluated value of a public `const` participate in the Package Public API
Fingerprint.

## 12. Construction, update, and postfix expressions

Struct construction never uses `new`:

```nexa
let cell = Cell {
    x: 1,
    y: 2,
};
```

Class construction always uses `new`:

```nexa
let enemy = new Enemy {
    name: "slime",
    health: 100,
};
```

Value and reference updates use `..` inside an explicit constructor:

```nexa
let moved = Cell {
    x: 10,
    ..cell
};

let copied = new Enemy {
    health: 50,
    ..enemy
};
```

There is at most one update base, it follows all explicit fields, and it has no
trailing comma requirement of its own. Duplicate fields, a wrong-kind base,
or missing fields without a base are errors. `value with { ... }` is not an
update syntax. Operand expressions are evaluated left-to-right in lexical
order: explicit field expressions first, then the base expression.

Postfix parsing is one left-associated chain:

```ebnf
postfix-expression = primary-expression,
                     { call-suffix
                     | index-suffix
                     | field-or-method-suffix
                     | await-suffix
                     | try-suffix } ;

await-suffix       = ".", "await" ;
try-suffix         = "?" ;
```

This structure MUST parse, without ad-hoc rewrites:

```nexa
load().await
load().await?
load().await?.field
client.connect().await?.fetch().await?
items().await?[0]
```

The AST operand of each `.await` is the complete expression to its left.
`.await` takes no parentheses and cannot be overloaded. Prefix `await expr` is
always rejected at `await`.

## 13. Attributes

### 13.1 Syntax and AST

```ebnf
attribute          = "@", identifier,
                     [ "(", [ attribute-arguments ], ")" ] ;

attribute-arguments
                   = attribute-argument,
                     { ",", attribute-argument }, [ "," ] ;

attribute-argument = [ identifier, "=" ], attribute-value ;
```

The AST represents positional and named arguments separately and retains every
argument in source order. It MUST retain duplicate and unknown arguments so
analysis can diagnose them; the parser MUST NOT overwrite one argument with
another.

For each built-in attribute, analysis validates:

```text
allowed declaration/member kind
positional arity
allowed named keys
argument value kind and range
duplicates
unknown keys
duplicates of a non-repeatable attribute
cross-attribute compatibility
```

The forms frozen here are:

| Attribute | Target and arguments |
|---|---|
| `@state(version = N)` | Class only; exactly one named positive integer version |
| `@stable("id")` | Eligible top-level symbol or state field; exactly one positional string |
| `@migration` | Function only; no arguments |
| `@activation` | Function only; no arguments |
| `@cleanup` | Function only; no arguments |
| `@immediate` | Function only; no arguments |
| `@test` | Function only; no arguments |
| `@fuel(N)` | An operation that explicitly permits a fuel attribute; exactly one non-negative integer |

An explicit stable ID matches:

```text
[A-Za-z][A-Za-z0-9._-]{0,127}
```

and is unique within the Package. `@stable` on a top-level declaration
requires `pub` or `pub(package)`. A state field MAY use it to preserve
persistent identity across source movement or renaming.

### 13.2 Role compatibility

The following matrix is normative:

| Combination | Result |
|---|---|
| `@migration` with `@activation` | rejected |
| `@migration` with `@cleanup` or `@immediate` | rejected |
| `@activation` with `@cleanup` or `@immediate` | rejected |
| `@cleanup` with `@immediate` | rejected |
| `@test` with any lifecycle role (`@migration`, `@activation`, `@cleanup`) | rejected |
| `@test` with `@immediate` | permitted by this matrix; Package Test signature/profile rules still apply |
| `@cleanup` on `async fn` | rejected |
| `@immediate` on `async fn` | rejected |
| `@state` on anything other than a Class | rejected |

`@migration`, `@activation`, `@cleanup`, and `@immediate` each assign one
special function role; two such roles cannot be inferred or silently ordered.
`@test` never implies `@activation`. Additional placement/signature
requirements of a lifecycle or test profile are checked after this matrix.

State is represented as:

```text
Type Kind      = Class
State Metadata = Some(version, field identities, schema)
```

The analyzer MUST NOT recreate a `Stateful` type kind.

## 14. Fingerprints and incremental invalidation

The Public API Fingerprint uses a structured canonical encoding and includes,
when applicable:

```text
resolved v2 path semantics
declaration visibility and stable identity
Struct, Enum, or Class kind
field order, type, and mutability
Enum variant order and payload
function signature and async effect
public const type and evaluated value
required Contract entry signature
```

The state-schema fingerprint includes `@state` metadata and stable field
identity. At minimum, moving a file, changing a `use`, changing field
mutability, adding/removing `async`, changing `@state`, or changing the
effective Contract Descriptor MUST invalidate exactly the cached semantic and
linked results that depend on that fact. Deleting or renaming a file MUST
remove its stale module and dependency edges.

## 15. Diagnostics

Every rejection mandated by this document emits a registered, stable
machine-readable diagnostic code. Human wording may improve, but the code,
primary source selection, and semantic category are stable within Language v2.

Diagnostics MUST:

- use the original URI/source identity;
- select the smallest useful primary span;
- attach related locations for collisions, duplicates, and prior declarations;
- distinguish syntax, naming, resolution, type, mutability, const, attribute,
  and visibility failures;
- never repair a spelling or choose a namespace implicitly;
- remain deterministic across repeated analysis of the same resolved input.

Required primary-span behavior:

| Failure | Primary span |
|---|---|
| Removed v1 declaration or expression form | removed introducer (`var`, `module`, prefix `await`, and so on) |
| Wrong naming class | declared identifier |
| Unknown/ambiguous `use` | failing path segment or ambiguous bound name |
| `super::` escapes Package | escaping `super` segment |
| Duplicate `use` binding | later alias/final segment; earlier binding is related |
| Top-level statement in Package mode | statement |
| Illegal `mut` field | `mut` |
| Immutable assignment | assigned Place |
| Local or untyped `const` | `const` or missing/invalid type site |
| Invalid const operation | smallest non-const expression |
| Prefix await | `await` |
| Unknown/duplicate attribute argument | argument name/value; original is related |
| Illegal attribute target or combination | attribute name; conflicting attribute is related |
| Recursive value layout | recursive type edge; declaration cycle is related |

Recovery MUST NOT cause a removed production to enter Typed IR or bytecode.

## 16. Explicit exclusions

Language v2 in M4R1 does not include:

```text
user-defined generics
traits or interfaces
closures or higher-order functions
dynamic dispatch
inheritance
operator overloading
macros
reflection
dynamic or any
exception unwinding
Box<T>
raw pointers, references, address operations, or a borrow checker
user finalizers
shared-memory threads
full semantic LSP
DAP
JIT or AOT
Pluie integration
an untrusted-code sandbox
a remote Package Registry
```

Compiler-internal types, GC metadata, task frames, and ABI lowering details do
not create source-level exceptions to this list.

## 17. Conformance boundary

An implementation conforms to this COMPLETE specification only when:

- the lexer, parser, lossless tree, AST, analysis, compiler, editor grammars,
  and diagnostics agree on the v2 surface;
- every removed spelling is absent from active semantic paths;
- Package, script, test, and REPL profiles reuse the same front end;
- public/state fingerprints and incremental invalidation reflect the v2 facts;
- positive and rejection matrices exercise each normative rule above.

Conformance does not require v1 source compatibility. Historical tags and
historical documentation may describe v1, but active product code cannot
decode or silently adapt it as Nexa v2.
