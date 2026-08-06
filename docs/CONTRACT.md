# Nexa Contract v3

Status: Contract Syntax v3 ACTIVE

A Contract defines the typed boundary between a Rust Host and Nexa packages.
Store it in one `*.contract.nexa` file and begin with one file-level Header:

```nexa
/// Snake game Host API.
@stable("snake-api")
contract SnakeApi;
```

The Header is the first non-comment declaration and ends with `;`. Every
following declaration belongs to that Contract; there is no enclosing
Contract block.

## Complete example

```nexa
/// Snake game Host API.
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
    fn choose_food_spawn(context: Cell) -> Option<Cell>;
}
```

`host` functions are implemented by Rust and callable from Nexa. `nexa`
functions are implemented by a Nexa Package and callable from Rust. A Contract
may omit either block but may contain at most one of each.

## Files and project configuration

The only Contract suffix is `.contract.nexa`. Select the current Host Contract
in the project manifest:

```toml
contract = "snake_api.contract.nexa"
```

The project resolver requires a readable file with the correct suffix inside
the allowed project root. It rejects `..` or symbolic-link escapes and rejects
a second selected Contract. Contract files are not executable modules and do
not get a path-derived `package::...` module name.

The compiler, CLI, LSP, VS Code extension, and Zed extension use the same
source-profile decision. Opening a `.contract.nexa` file selects the **Nexa
Contract** language mode.

## Declarations and naming

The Contract profile supports:

```nexa
struct Position {
    x: f32,
    y: f32,
}

enum Event {
    Started,
    Moved(Position),
}

handle Entity;
```

Contract, struct, enum, handle, and Variant names use `PascalCase`. Function,
field, and parameter names use `snake_case`. Struct fields and tuple-Variant
payloads are ordered ABI data. Type declarations and functions may appear in
any top-level order.

Function declarations end in semicolons. A function without `-> type` returns
Unit:

```nexa
host {
    fn log(message: string);
}
```

## Types

The complete Contract type surface is:

```text
i32 i64 f32 f64 bool rune string
Array<T> Buffer<T> Option<T> Result<T, E>
Token<Handle> Snapshot<Struct>
declared struct, enum, or handle names
```

Generic builtin names are case-sensitive. User-defined generics, `void`,
nullable references, pointers, and source-visible request or future types are
not supported. `Token<Entity>` is tied to the declared `Entity` Handle's
Stable ID and cannot be used as another Handle's token. `Snapshot<State>` is
similarly tied to a declared Struct.

## Host functions and attributes

A synchronous Host call uses `fn`; an asynchronous call uses `async fn`:

```nexa
host {
    @fuel(2)
    @capability("console.write")
    fn log(message: string);

    @cancel(cancel_task)
    @abandon(trap)
    async fn load(id: string) -> Result<Profile, LoadError>;
}
```

Host attributes are:

| Attribute | Valid on | Meaning |
| --- | --- | --- |
| `@fuel(n)` | Host sync or async function | Non-zero base Fuel charge |
| `@capability("name")` | Host sync or async function | Required Host capability |
| `@cancel(return_error)` | Host async function | Complete cancellation through the declared error |
| `@cancel(cancel_task)` | Host async function | Cancel the waiting Nexa task |
| `@abandon(return_error)` | Host async function | Complete abandonment through the declared error |
| `@abandon(trap)` | Host async function | Trap on abandonment |

Absent Fuel normalizes to `1`; absent async policies normalize to
`return_error`. Async Host functions return `Result<S, E>`. An enum error used
with `return_error` needs a unit `Cancelled` or `Abandoned` Variant as
applicable; `i32` uses the fixed Runtime codes.

`@stable("name")` is valid on the Header and source-bearing declarations. It
preserves lookup identity within the same owner scope and declaration category
when a source name changes. Its value may contain ASCII letters, digits, `_`,
`-`, `.`, `:`, or `/`.

## Nexa entrypoints

Functions in `nexa {}` define legal typed entrypoint signatures:

```nexa
nexa {
    fn on_event(event: Event) -> Array<Command>;
    fn inspect_state() -> Option<string>;
    async fn rebuild_index() -> Result<Index, BuildError>;
}
```

Required versus optional is chosen by the Host's typed embedding API, not in
the Contract declaration. Async Nexa entrypoints carry the Task effect but do
not accept Host cancellation, abandonment, Fuel, or capability attributes.

## Comments, diagnostics, and recovery

Line comments, non-nested block comments, and `///` documentation comments are
supported. A documentation group attaches to the next declaration, including
the Header. Documentation, ordinary comments, whitespace, and formatting do
not affect ABI identity.

The parser reports precise spans for missing or misplaced Headers, a missing
Header semicolon, repeated Headers, unsupported top-level declarations, and a
Contract Header in executable source. Error recovery continues through later
items so one early error does not hide all subsequent declarations.

## Public frontend

The public semantic pipeline is:

```text
UTF-8 source + SourceProfile::Contract
→ ContractSyntaxTree
→ ContractAst
→ ValidatedContract
→ ContractDescriptor
→ BindingModel
→ generated Rust
```

The shared syntax crate exposes `parse_contract`, `lex_contract`, and
`parse_contract_ast`. The high-level API is exposed from `nexa-contract` /
`nexa_contract`:

```rust
let contract = nexa_contract::parse_contract(source)?;
let descriptor = nexa_contract::abi_descriptor(&contract);
let fingerprint = nexa_contract::contract_fingerprint(&contract);
let rust = nexa_contract::generate_rust(&contract)?;
```

Build scripts generate from the new source path:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    nexa_contract::build::generate("app_api.contract.nexa")?;
    Ok(())
}
```

The generated module contains the typed Host trait and registry, Contract
types, async completion tickets, typed Nexa entrypoint markers, canonical
Descriptor v2 bytes, and fingerprints. Generated source begins with a
deterministic provenance comment such as:

```rust
// Generated from app_api.contract.nexa.
```

Contract Syntax v3 changes the source container only. It preserves equivalent
declaration Stable IDs, normalized Descriptor payload, Descriptor v2 framing,
Host schema v2, and generated Host/Nexa binding shapes. The required
syntax-version metadata rename is provenance. The Build Fingerprint records
`CONTRACT_SYNTAX_VERSION = 3`.

## CLI

Use the Contract command group directly:

```bash
nexa contract check snake_api.contract.nexa
nexa contract generate snake_api.contract.nexa
```

`nexa check`, `build`, `test`, `dev`, and `lock` also resolve the manifest's
`contract` input. Machine-readable output uses `contractPath`,
`contractSyntaxVersion`, and `contractDiagnostic`.

See [Migrating to Contract v3](MIGRATING_TO_CONTRACT_V3.md) for the one-time
source, API, CLI, and editor migration. There is no compatibility parser or
public alias for the superseded surface.
