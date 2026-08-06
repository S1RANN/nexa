# Nexa editor support

This directory contains local language support for `.nexa` source and
Host Contract files:

- `tree-sitter-nexa/` is the Nexa source Tree-sitter grammar.
- `tree-sitter-nexa-contract/` is the intentionally separate Host Contract
  Tree-sitter grammar. Keeping it separate lets contract declarations and
  `host`/`nexa` blocks retain their own structure and outline.
- `vscode/` is the VS Code syntax and diagnostic-client extension.
- `zed/` is a two-language Zed extension template.
- `language-syntax.json` is the shared lexical vocabulary.
- `scripts/` generates, validates, and packages the extensions.

The Nexa v2 grammar covers `use` declarations and `::` namespace paths,
`pub` and `pub(package)`, typed constants, `let mut`, mutable class fields,
attributes, `async fn`, postfix `.await`/`?`, structural updates, loop control,
comments, documentation comments, and string interpolation. The Contract (v3)
grammar covers the flat `contract <Name>;` header (with `@stable(...)`
attributes and doc comments attached to the header), `handle`, `host`/`nexa`
blocks, synchronous and asynchronous functions, policy attributes, PascalCase
generic types, and all three comment forms including `///` documentation.
Legacy `*.nidl` files receive a migration diagnostic pointing at
`*.contract.nexa`; they are not parsed by the Contract grammar.

`tree-sitter-nexa/src/parser.c` is a deterministic build artifact and is not
versioned because the generated C parser exceeds the repository file-size
budget. `pnpm generate` materializes it locally, `pnpm check` regenerates and
validates it in an isolated directory, and the Zed packaging step commits it
into the self-contained grammar repository shipped with the extension.
`src/grammar.json` and `src/node-types.json` remain versioned and are checked
byte-for-byte.

Build everything from the repository root:

```sh
rustup target add wasm32-wasip2
pnpm --dir editors install --frozen-lockfile
pnpm --dir editors package
```

The package command fails unless `vsce` produces a real VSIX and Cargo
successfully builds the Zed extension for `wasm32-wasip2`. Artifacts and the
typed package evidence report are written below `target/nexa-editor-support/`.
See
[`docs/EDITOR_SUPPORT.md`](../docs/EDITOR_SUPPORT.md) for installation,
maintenance, supported syntax, and limitations.
