# Nexa editor support

This directory contains local language support for `.nexa` and `.nidl` files:

- `tree-sitter-nexa/` is the Nexa source Tree-sitter grammar.
- `tree-sitter-nexa-idl/` is the intentionally separate NIDL Tree-sitter
  grammar. Keeping it separate lets contract declarations and `host`/`nexa`
  blocks retain their own structure and outline.
- `vscode/` is the VS Code syntax and diagnostic-client extension.
- `zed/` is a two-language Zed extension template.
- `language-syntax.json` is the shared lexical vocabulary.
- `scripts/` generates, validates, and packages the extensions.

The Nexa v2 grammar covers `use` declarations and `::` namespace paths,
`pub` and `pub(package)`, typed constants, `let mut`, mutable class fields,
attributes, `async fn`, postfix `.await`/`?`, structural updates, loop control,
comments, documentation comments, and string interpolation. NIDL v2 covers
`contract`, `handle`, `host`/`nexa` blocks, synchronous and asynchronous
functions, policy attributes, PascalCase generic types, and all three comment
forms including `///` documentation.

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
