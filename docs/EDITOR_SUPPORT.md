# Nexa editor support

Status: Language v3 support IN PROGRESS — Set type, collection iteration, pair binding

Nexa provides local syntax support for executable `.nexa` files and
`.contract.nexa` files in VS Code and Zed. The Contract file type is displayed
as **Nexa Contract**. Version `0.1.2` provides syntax highlighting, bracket handling,
indentation, outlines, and live compiler diagnostics through `nexa lsp`.
It provides line/block comment configuration and documentation-comment
highlighting. It does not provide semantic highlighting, completion, hover,
definition, references, rename, formatting, snippets, code actions, DAP, or
debugging.

The editor tooling is isolated under `editors/`. It is not part of the Rust
Workspace and does not change Runtime gates.

## Prerequisites

- Node.js 22 or newer
- pnpm 11.9.0
- Git, for preparing the local Zed grammar repository
- Zed installed with a Rust toolchain managed by rustup when installing a Dev
  Extension
- a built or installed `nexa` executable on `PATH`

## Build and verify

Run the following commands from the repository root:

```sh
pnpm --dir editors install --frozen-lockfile
pnpm --dir editors generate
pnpm --dir editors check
pnpm --dir editors package:vscode
pnpm --dir editors prepare:zed
```

The combined command is:

```sh
pnpm --dir editors package
```

`generate` updates the committed Tree-sitter outputs and both TextMate
grammars. `check` regenerates into a temporary directory, rejects drift,
parses the repository examples, compiles every Zed query, and verifies that
both editors attach to `nexa lsp`.

Build output is written to:

```text
target/nexa-editor-support/
├── vscode/nexa-language-support-0.1.2.vsix
└── zed/
    ├── extension.toml
    ├── Cargo.toml
    ├── src/
    ├── languages/
    └── tree-sitter-nexa/
```

The packaged Zed grammar is a temporary local Git repository. Its manifest
uses the grammar directory's canonical absolute `file://` URL and the exact
generated commit revision, so Zed can load it without a remote repository.

## VS Code installation

Install the local package:

```sh
code --install-extension \
  target/nexa-editor-support/vscode/nexa-language-support-0.1.2.vsix
```

Reload VS Code after upgrading the package. Remove it with:

```sh
code --uninstall-extension s1rann.nexa-language-support
```

Set `nexa.server.path` if the CLI is not available as `nexa`. Use
**Nexa: Restart Language Server** after replacing the executable.

## Zed installation

1. Run `pnpm --dir editors prepare:zed`.
2. Open Zed's Extensions page.
3. Choose **Install Dev Extension**, or run the
   `zed: install dev extension` action.
4. Select the absolute `target/nexa-editor-support/zed/` directory.

After rebuilding, run `zed: rebuild dev extension` to load the new files.
Remove or disable the local entry from Zed's Extensions page when it is no
longer needed.

The Zed extension resolves `nexa` from the Worktree shell environment and
starts it with the `lsp` argument.

The language server keeps unsaved document overlays by monotonically
increasing document version. A stale `didChange` cannot replace a newer
overlay, `didSave` republishes the current text, and `didClose` always clears
Problems before returning to disk state. Contract parser errors retain their exact
byte Span and are converted to UTF-16 LSP positions.

File URIs are handled by the standard URL implementation, including Unix
paths, Windows drive paths, UNC shares, Unicode, spaces, and encoded `#`, `%`,
and `?` characters.

## Supported language surface

Nexa support follows the current compiler Lexer and Parser:

- ASCII identifiers with `snake_case`, `PascalCase`, and
  `SCREAMING_SNAKE_CASE` diagnostics
- path-derived modules and `use` declarations rooted at `package::`, `self::`,
  `super::`, `host::`, `std::`, or a dependency alias
- mutable `let`, immutable block-local `const`, module `const`, and
  default-mutable parameters and fields
- Struct and Enum value declarations, Class reference declarations, and
  `@state(version = N)` metadata
- `async fn`, postfix `.await`, `yield`, `defer`, and attribute-based
  migration, activation, cleanup, and immediate functions
- strings, interpolation, Unicode runes, integers, floats, built-in generic
  collections, `Option`, `Result`, and Reload intrinsics
- namespace and associated-item `::`, member `.`, Range and update `..`
- `Set<T>` built-in generic type with `Set::new`, `insert`, `contains`, `remove`, `clear`, `len`
- single-binding collection `for` iteration over `Array`, `Buffer`, and `Set`
- pair-binding `for (key, value) in map` iteration over `Map<K, V>`

Contract support follows the shared `SourceProfile::Contract`,
`ContractSyntaxTree`, and validated Contract model:

- one first `contract Name;` Header with documentation and attributes
- flat `struct`, `enum`, and `handle` declarations
- `host {}` and `nexa {}` blocks
- synchronous and asynchronous Host/Nexa functions
- `@fuel`, `@cancel`, `@abandon`, and `@capability` attributes
- `Array`, `Buffer`, `Option`, `Result`, `Token`, and `Snapshot` types
- line, block, and documentation comments
- outlines for Contract, Struct, Enum, Handle, Host Function, and Nexa Function

The LSP selects the Contract profile from the URI and project configuration
using the same resolver as the CLI. Contract documents publish syntax, naming,
type-resolution, attribute/direction, Stable-ID collision, and generated Rust
name collision diagnostics. Editor parsing remains structural; compiler and
Contract validation remain authoritative.

## Updating the grammar

1. Update `editors/language-syntax.json` when the lexical vocabulary changes.
2. Update `editors/tree-sitter-nexa/grammar.js` for structural syntax changes.
3. Update Zed queries only when named syntax nodes or editor behavior changes.
4. Run `pnpm --dir editors generate`.
5. Run `pnpm --dir editors check`.
6. Repackage both extensions.

Do not edit generated `parser.c`, `grammar.json`, `node-types.json`, or
TextMate JSON files by hand.

## Known limitations

- Parsing is intended for editor structure and highlighting; compiler
  type-checking and semantic validation remain authoritative.
- The diagnostics-only LSP publishes compiler Problems for unsaved overlays,
  converts byte spans to UTF-16 positions, and clears diagnostics after fixes.
- Incomplete expressions or blocks may temporarily produce recovery nodes,
  but the extension remains usable.
- VS Code does not register `<` and `>` as global bracket pairs. This prevents
  bracket-pair colorization from treating the `>` in `->` and `=>` as an
  unmatched closing bracket; generic angle brackets remain syntax-highlighted.
- The Zed package contains an absolute local Grammar URL and must be rebuilt
  after moving or cloning the repository.
- Version `0.1.2` is for local contributor use only and is not published to
  the VS Code Marketplace or Zed Extension Gallery.
