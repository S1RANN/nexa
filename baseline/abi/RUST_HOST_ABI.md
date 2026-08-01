# Rust Host ABI Specification

Version: **2.0.0**

Status: **COMPLETE**

Generated Rust bindings are the sole supported internal-language Host
integration. A validated NIDL v2 Contract lowers to ABI Descriptor v2 and a
semantic Binding Model before code emission.

The generated layer provides:

- Host traits;
- typed argument and result thunks;
- Host Registry metadata and Dispatch;
- typed Nexa entrypoint markers, argument structs, and output types;
- stable IDs and per-declaration fingerprints;
- the full ABI Descriptor and full Contract fingerprint from which Package
  builds derive effective Contract subsets;
- test Stubs.

Rust names are validated and collision-checked before emission. The generator
uses token syntax trees, validates the complete file with `syn`, formats with
`prettyplease`, and reparses the result. It never inserts an NIDL identifier as
unparsed Rust source.

Hosted Realms register against a process-level `RuntimeHost`.
`RuntimeHost::close()` succeeds only after every hosted Realm is dropped, every
completion reservation is released, and every transferred release is drained.
Closed Hosts reject new Realms. In debug builds, dropping the final Host handle
without an explicit close reports the live Realm, completion, and release
counts.

The Host-call bridge accepts at most eight arguments through
`RuntimeHostArgs`. Scalar and Handle arguments are decoded directly into
generated binding types without constructing an intermediate value vector.
Async Host functions use generated completion tickets internally; Request
types are not exposed in NIDL or Nexa source.

Each generated `HToken` is statically and dynamically bound to declared Handle
`H`. Its Runtime value type is the domain-separated resource-token identity
derived from `H`'s Stable ID. Argument decode, Host return encode, Registry
dispatch, and release validate that identity; a raw token for another Handle
cannot cross the generated wrapper boundary.

`HostCall.import` is a Bytecode v6 module-local index. It addresses only the
calling module's verified `HostImport` metadata and is never a Contract
declaration ordinal or a generated Registry slot. The selected metadata
contains the Host function Stable ID, exact parameter/result ABI, sync/async
mode, Fuel, and asynchronous-result policy.

The Runtime resolves the module-local index, then asks `HostRegistry` to
dispatch by the selected Host function Stable ID. Generated Registries match
that Stable ID and validate typed arguments; they do not reuse the local
index. This two-step boundary is mandatory because two Packages may carry
different effective Contract subsets and therefore different local import
indices for the same Host function. The Contract Runtime ID validates Registry
provenance but never substitutes for the per-function Stable ID.

Host panics are contained and converted to structured Host traps. Runtime
internals, GC pointers, Frames, and mutable Epoch roots are never exposed.

Normal Host-to-Nexa calls use typed marker APIs. Required and Optional
entrypoints are resolved by stable marker type and exact signature, never by a
public function index.
