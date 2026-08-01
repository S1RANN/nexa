# Nexa NIDL

Version: **2.0.0**

Status: **COMPLETE**

The internal gameplay language uses an NIDL-first, exact-build Rust Host
Contract. The normative syntax and semantic rules are in
[`NIDL_V2.md`](NIDL_V2.md); the binary identity rules are in
[`CONTRACT_DESCRIPTOR_V2.md`](CONTRACT_DESCRIPTOR_V2.md); generated Rust rules
are in [`BINDING_CODEGEN_V2.md`](BINDING_CODEGEN_V2.md).

The only top-level form is:

```nidl
contract App {
    handle Entity;

    host {
        fn log(message: string);

        @fuel(8)
        @cancel(return_error)
        @abandon(trap)
        async fn load(id: string) -> Result<Profile, LoadError>;
    }

    nexa {
        fn on_event(event: Event) -> Array<Command>;
    }
}
```

The `host` surface is implemented by Rust and callable from Nexa. The `nexa`
surface lists legal typed entrypoints implemented by Nexa and callable from
Rust. Whether an entrypoint is Required or Optional belongs to the Host usage
site, not to NIDL.

Validated declarations lower to ABI Descriptor v2 through a typed semantic
model. Type layout, Host function, and Nexa entrypoint fingerprints are
independent. Comments, formatting, top-level declaration order, and
`host`/`nexa` block order do not affect identity; ordered fields, Variants,
parameters, and semantic attributes do.

An async Host function evaluates in Nexa to its declared result after postfix
`.await`. ABI lowering may use completion tickets and internal Host Request
state, but no Request type is nameable in NIDL or Nexa source.

NIDL v1 spellings, canonical formatted-string hashes, and compatibility
adapters are not part of the active Contract.
