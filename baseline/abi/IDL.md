# Nexa Contract

Version: **3.0.0**

Status: **ACTIVE**

The internal gameplay language uses one exact-build Rust Host Contract. The
normative syntax and semantic rules are in
[`CONTRACT_LANGUAGE_V3.md`](CONTRACT_LANGUAGE_V3.md); binary identity remains
[`CONTRACT_DESCRIPTOR_V2.md`](CONTRACT_DESCRIPTOR_V2.md); generated Rust rules
are in [`BINDING_CODEGEN_V2.md`](BINDING_CODEGEN_V2.md).

The source begins with one flat Header:

```nexa
contract App;

handle Entity;

host {
    fn spawn() -> Token<Entity>;
}

nexa {
    fn on_event(event: Event) -> Array<Command>;
}
```

The `host` surface is implemented by Rust and callable from Nexa. The `nexa`
surface lists legal typed entrypoints implemented by Nexa and callable from
Rust. Required versus Optional belongs to the Host usage site, not to the
Contract declaration.

Validated declarations lower through `ContractAst → ValidatedContract →
ContractDescriptor → BindingModel`. Stable IDs provide dispatch identity;
Descriptor v2 fingerprints provide structured compatibility identity. Source
formatting and the flat Header container never become descriptor payload.

An async Host function evaluates in Nexa to its declared result after postfix
`.await`. ABI lowering may use completion tickets and internal Host Request
state, but no Request type is nameable in Contract or Nexa source.

The active product provides no compatibility parser, old file suffix, public
old-name API, CLI alias, editor association, formatted canonical-string hash,
or function-index binding surface.
