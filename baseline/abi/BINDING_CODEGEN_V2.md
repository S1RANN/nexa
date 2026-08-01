# Nexa Structured Binding Codegen v2

Version: **2.0.0**

Status: **COMPLETE**

Structured Binding Codegen v2 is the only Rust generation path for NIDL
bindings and `nexa-machine` generated Rust. It consumes validated semantic
models and constructs Rust syntax with token ASTs. It does not concatenate
Rust source or Rust expressions.

## Required pipeline

NIDL binding generation is:

```text
nexa-syntax NIDL SyntaxTree
→ NidlAst
→ ValidatedContract
→ BindingModel
→ proc_macro2::TokenStream
→ syn::parse2::<syn::File>()
→ prettyplease::unparse()
→ syn::parse_file()
→ atomic write into OUT_DIR
```

The first `syn` parse validates the token backend before formatting. The second
parse validates the exact pretty-printed bytes that will be written. A failure
at either step is a generator error associated with the originating NIDL
declaration. Invalid output is never written over the previous generated file.

The generator dependencies are:

```text
proc-macro2
quote
syn
prettyplease
```

Backend emitters return `TokenStream` or a typed token fragment. A backend does
not return a `String` containing an item, expression, pattern, type, match arm,
or statement.

## `BindingModel`

`BindingModel` is the complete, backend-neutral Rust generation input. It is
constructed only from `ValidatedContract` and Descriptor v2 identities. The
Rust backend does not traverse `NidlAst` and does not make late semantic
decisions.

Every generated declaration model retains at least:

```text
source name
Rust Ident
Rust type name
Stable ID
resolved ABI type
CodecStrategy
BorrowStrategy
SourceOrigin
declaration fingerprint
```

The complete model contains:

```text
BindingModel
  contract identity, descriptor, and Rust module metadata
  generated type models
  Host trait method models
  async completion-ticket models
  Host registry dispatch models
  Nexa entrypoint marker models
  argument and output codec models
  token and snapshot wrapper models
```

`CodecStrategy` states how a value is decoded from Runtime arguments, encoded
as a Host return, encoded as a Nexa-call argument, and decoded as a Nexa-call
output. `BorrowStrategy` states whether a Host input is scalar-by-value,
borrowed string, borrowed aggregate view, typed handle, or owned value.
`SourceOrigin` carries the `.nidl` URI and declaration/name range for
diagnostics only; it is not emitted into ABI identity.

An unsupported type/codec/borrow combination is rejected while constructing
`BindingModel`. Token emission contains no fallback such as an empty body,
`unimplemented!()`, guessed codec, or textual placeholder.

## Name mapping

Name conversion is centralized before token generation:

```text
on_event             → OnEvent
choose_food_spawn     → ChooseFoodSpawn
on_event arguments   → OnEventArgs
on_event result      → OnEventOutput
load_profile ticket  → LoadProfileCompletionTicket
Snake contract       → SnakeHost
```

NIDL function and field names remain snake_case Rust method and field
identifiers. Contract, type, variant, marker, wrapper, argument, output, and
ticket names are PascalCase.

The conversion handles each snake_case segment independently and preserves no
source underscores in PascalCase output. Before codegen, validation rejects:

- two source names mapping to the same Rust identifier;
- a generated helper colliding with a declared type;
- a marker colliding with its own or another marker's `Args`, `Output`, or
  ticket name;
- a Rust keyword or reserved generated name; and
- two declarations mapping to the same Stable ID.

The backend never resolves a collision by appending a number, changing case,
using a raw identifier, or depending on declaration order.

## Generated ABI types

Type emission is structural and covers:

```text
Struct
Enum, including unit and single-value tuple variants
Handle wrapper
Token<T> wrapper
Snapshot<T> wrapper and typed snapshot codec
Entrypoint Args
Entrypoint Output
```

The ABI-to-Rust mapping begins with:

| NIDL | Generated Rust surface |
| --- | --- |
| `i32`, `i64`, `f32`, `f64`, `bool` | corresponding Rust scalar |
| `rune` | `char` |
| `string` | `String`, or borrowed `str` for a validated Host-input strategy |
| `Array<T>` | owned `Vec<T>`, or a typed borrowed Runtime view for Host input |
| `Buffer<T>` | typed Runtime copy-buffer value or borrowed view |
| `Option<T>` | `Option<T>` |
| `Result<T, E>` | `Result<T, E>` |
| declared struct or enum | generated named Rust type or borrowed decode view |
| declared handle | generated typed handle newtype |
| `Token<H>` | generated resource-token newtype bound to declared Handle `H` |
| `Snapshot<T>` | generated typed snapshot newtype |

The exact owned or borrowed form comes from `BorrowStrategy`; emitters do not
recompute it from syntax. Struct field and enum variant order remains the
validated ABI order. Generated wrappers validate their type identity when
converting from raw Runtime handles.

A generated `HToken` wrapper carries the domain-separated Runtime token type
identity derived from Handle `H`'s Stable ID. Conversion checks both the
resource-token type identity and its Handle content identity. Wrappers for two
different Handles are distinct Rust types and distinct ABI types; codegen must
not share a raw wrapper merely because their physical representation matches.

Every encode path computes requirements before allocation and writes through
the Runtime transaction API. Every decode path validates arity, Runtime value
kind, named type identity, variant identity, and complete aggregate shape.
Generation must not create unchecked positional casts or partial allocation
visible on failure.

## Generated Host trait

For:

```nidl
contract Snake {
    host {
        fn format_score(score: i32) -> string;
        async fn load_profile(id: string) -> Result<Profile, LoadError>;
    }
}
```

the model generates a trait whose public shape is headed by:

```rust
pub trait SnakeHost {
    fn format_score(/* generated context and typed arguments */);
    fn load_profile(/* generated context and typed arguments */);
}
```

Method names and typed argument order match NIDL. Borrowed inputs receive a
generated lifetime only where the `BorrowStrategy` requires one. Host errors,
Runtime resource context, and return conversion use the generated ABI
adapters; application code does not decode raw argument slots.

An asynchronous Host function generates a typed completion-ticket wrapper and
request dispatch metadata. The wrapper exposes completion in terms of the
declared result, for example `Result<Profile, LoadError>`. Request handles,
request result wrappers, cancellation internals, and abandonment internals are
Rust ABI/Runtime details and never reappear in NIDL source.

## Generated Host registry

The Host registry is constructed entirely from token AST:

```text
stable function ID dispatch
arity validation
typed argument decoding
Host trait call
sync return encoding
async completion-ticket construction
Fuel and policy metadata
capability metadata
```

Dispatch identity is a Stable ID validated against Descriptor v2. A private
dense thunk slot may exist inside a verified artifact, but generated public
bindings and embedding APIs neither expose nor accept a function index.

Match expressions, decode expressions, return expressions, and all error arms
are emitted with `quote!` and typed helper builders. No registry helper accepts
an arbitrary Rust expression string.

## Generated Nexa entrypoint markers

For:

```nidl
nexa {
    fn on_event(event: SnakeEvent) -> Array<SnakeCommand>;
}
```

the generated Rust surface includes:

```rust
pub enum OnEvent {}

pub struct OnEventArgs {
    pub event: SnakeEvent,
}

pub type OnEventOutput = Vec<SnakeCommand>;
```

The marker implements the typed Runtime entrypoint trait and provides:

```rust
OnEvent::NAME == "on_event"
```

It also carries the Descriptor v2 Stable ID and exact signature, computes
transactional argument requirements, encodes typed arguments, and decodes the
owned output. A no-argument entrypoint still receives a zero-sized
`NameArgs`. An omitted NIDL return becomes `type NameOutput = ();`.

The marker itself does not encode required versus optional. The same marker is
used by typed APIs such as global requirement, existence query, optional
single-Package call, and optional broadcast. Public entrypoint selection never
uses a source string or function index.

## Contract provenance

Generated bindings expose typed contract metadata derived from Descriptor v2:

```text
contract source name
NIDL syntax version = 2
Host Contract schema version = 2
ABI Descriptor version = 2
SOURCE = exact UTF-8 NIDL source snapshot
CONTRACT_DESCRIPTOR = complete canonical full-contract framed bytes
CONTRACT_FINGERPRINT = BLAKE3(CONTRACT_DESCRIPTOR)
CONTRACT_RUNTIME_ID = compact Runtime identity derived from the fingerprint
generated Host registry factory
typed entrypoint markers
```

`SOURCE` is retained for diagnostics and inspection but is not hashed and
cannot override structured descriptor identity. There is no `CONTRACT_ID`
alias.

## `nexa-machine` generation

`nexa-machine` uses the same structural boundary:

```text
validated machine semantic model
→ machine binding model
→ quote TokenStream
→ syn::parse2::<syn::File>()
→ prettyplease::unparse()
→ syn::parse_file()
→ output
```

It may share token helpers with `nexa-idl`, but cannot preserve a second large
`String`, `write!`, `writeln!`, or `format!` Rust generator. Machine transition
arms, state types, dispatch, and codecs are token nodes built from validated
model values.

## Determinism

Generation order follows Descriptor v2 canonical top-level ordering.
Semantically identical NIDL produces byte-identical pretty-printed Rust across
runs. Output does not depend on hash-map iteration, absolute paths, process
state, locale, source declaration order where the descriptor says order is
irrelevant, or comments.

Generated source begins with a deterministic do-not-edit marker. It contains
no timestamp, host path, random identifier, or compiler debug rendering.

## Removed legacy output

Structured Codegen v2 emits none of:

```text
LEGACY_FUNCTION_INDEX
legacy export aliases
public function-index constants or APIs
canonical formatted-string hashes
source-visible Request types
old interface/export terminology
large Rust source strings
string-built Rust expressions
```

The old generator is deleted rather than left behind as unused code. Generated
fixtures and consumers compile only against the v2 names and typed markers.

## Verification obligations

The structured-codegen gate must demonstrate:

1. every repository `.nidl` reaches `ValidatedContract`;
2. every generated file passes both required `syn` parses;
3. repeated generation is byte-for-byte deterministic;
4. generated bindings compile in their real Host consumers;
5. Host dispatch, decode, encode, async completion, token, and snapshot paths
   execute through generated code;
6. Nexa entrypoint marker signatures match verified Package bytecode;
7. Rust-name collision fixtures fail before token generation;
8. `nexa-machine` output follows the same parse/format/reparse pipeline; and
9. active source and generated output contain no legacy items listed above.
