# Host Binding

Version: **2.0.0**

Status: **COMPLETE**

The selected `.contract.nexa` file is the only authoritative Host Contract
definition. The normative Contract, Descriptor, and generator rules are:

- [`../abi/CONTRACT_LANGUAGE_V3.md`](../abi/CONTRACT_LANGUAGE_V3.md)
- [`../abi/CONTRACT_DESCRIPTOR_V2.md`](../abi/CONTRACT_DESCRIPTOR_V2.md)
- [`../abi/BINDING_CODEGEN_V2.md`](../abi/BINDING_CODEGEN_V2.md)

`nexa-contract` consumes the shared lossless `ContractSyntaxTree`, produces a
source-preserving `ContractAst`, validates it once into `ValidatedContract`, and
derives both ABI Descriptor v2 and a semantic `BindingModel`. No generator
backend performs new name, type, or policy decisions while emitting code.

The Rust backend deterministically generates into `OUT_DIR`:

- the Rust Host Trait and test Stub;
- the Host Dispatcher and generated Registry;
- borrowed argument decoding and return encoding;
- typed error and completion conversion for async Host functions;
- stable function identifiers and per-declaration fingerprints;
- the full ABI Descriptor v2 and full Contract fingerprint;
- Nexa entrypoint marker, argument, and output types.

The backend constructs `proc_macro2::TokenStream` values with `quote`, parses
the complete output as `syn::File`, formats it with `prettyplease`, parses the
formatted source again, and only then writes it. Host crates include that
generated file and implement only the generated Trait. They do not maintain
function-name matching, argument slots, return slots, stable-ID tables, ABI
fingerprints, or generated entrypoint metadata by hand.

A resolved Package build derives its effective Descriptor and fingerprint from
the referenced subset authorized by that generated full Contract. Loading
compares the Package's embedded effective fingerprint, Host imports, and
entrypoint signatures with the generated Host Registry before interpreter
execution. A required entrypoint must exist with the exact signature. An
optional entrypoint may be absent, but an implementation with a mismatched
signature rejects the Package.

The binding gate mutates legal and illegal Contracts, validates source spans
and diagnostics, generates each valid model repeatedly for byte-identical
output, reparses and compiles the output in an independent Host crate, and
rejects mismatched Bytecode before Runtime admission. Compatibility is checked
against handwritten business code, never a generated Stub. Each incompatible
case applies an explicit minimal business-code patch before executing the
changed binding.

Contract Syntax v3 changes the source container and source identity, not Host
ABI schema or Descriptor v2 framing. Equivalent migration preserves Stable
IDs, normalized Descriptor bytes, and generated Host/Nexa binding shapes; the
required syntax-version metadata rename is provenance. The Build Fingerprint
records `CONTRACT_SYNTAX_VERSION = 3`.

The active product contains no legacy function index, old export alias,
formatted canonical-string hash, second private Contract parser, superseded
public API alias, or string-based Rust expression generator.
