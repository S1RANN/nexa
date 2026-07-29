# Nexa editor support

Nexa provides local syntax support for `.nexa` and `.nidl` files in VS Code
and Zed. Version `0.1.2` provides syntax highlighting, bracket handling,
indentation, outlines, and live compiler diagnostics through `nexa lsp`.
It does not provide comments, semantic highlighting, completion, hover,
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

## Supported language surface

Nexa support follows the current compiler Lexer and Parser:

- ASCII identifiers
- module, import, type, function, field, parameter, and expression syntax
- task, immediate, migration, and cleanup effects
- stateful and activation attributes
- strings, Unicode runes, integers, floats, generics, and operators
- Option/Result constructors, collections, and Reload intrinsics

Nexa IDL support follows the current IDL Parser:

- interface, opaque, struct, and enum declarations
- sync and request Host functions
- request policies and fuel clauses
- typed request, token, snapshot, Option, Result, array, and buffer types
- exports and `void`

Neither language has comment syntax. `/` is always highlighted and parsed as
the division operator.

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
