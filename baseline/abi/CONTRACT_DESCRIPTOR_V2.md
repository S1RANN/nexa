# Nexa Contract Descriptor v2

Version: **2.0.0**

Status: **COMPLETE**

The structured descriptor version encoded in every full and effective
descriptor is:

```text
ABI_DESCRIPTOR_VERSION: u16 = 2
```

It is independent from `NIDL_SYNTAX_VERSION` and
`HOST_CONTRACT_SCHEMA_VERSION`; all three currently equal `2` but identify
different protocol boundaries. The Rust constant remains `u16`; canonical
wire framing encodes `u32::from(ABI_DESCRIPTOR_VERSION)` as four
little-endian bytes.

Contract Descriptor v2 is the canonical, structured ABI identity derived from
a valid NIDL v2 contract. It replaces formatted canonical-source strings and
formatted canonical-hash strings. Source rendering is diagnostic material
only and is never hash input.

## Terms

- **Stable ID** is the declaration identity used by generated bindings,
  bytecode, the verifier, and Runtime lookup.
- **Declaration fingerprint** is a 32-byte BLAKE3 digest of one validated type,
  Host function, or Nexa entrypoint descriptor.
- **Full contract fingerprint** is the digest of the complete validated
  contract descriptor.
- **Effective contract fingerprint** is the Package-specific digest of only
  the ABI surface that can affect that Package.

Stable IDs and fingerprints are different values. Truncating a fingerprint to
obtain a Stable ID is not a collision policy.

## Canonical writer

Every fingerprint is produced by a typed descriptor writer. The writer has no
operation that accepts a formatted declaration or debug string.

The primitive encoding is:

| Value | Encoding |
| --- | --- |
| enum or union tag | fixed schema-defined `u8` tag |
| boolean | fixed tag `0x00` or `0x01` |
| `u32` | four bytes, unsigned little-endian |
| Stable ID | eight bytes, unsigned little-endian |
| byte string | `u32` byte length followed by exact bytes |
| UTF-8 string | byte-string encoding after UTF-8 validation |
| optional value | `0x00`, or `0x01` followed by the value |
| ordered sequence | `u32` element count followed by encoded elements |
| set or map | canonical sort followed by ordered-sequence encoding |

Counts and lengths that do not fit `u32` are validation errors. An unknown enum
tag or trailing byte is a decoding error. There is no platform-sized integer,
native-endian value, locale-sensitive comparison, or implicit separator.

Every declaration, full-contract, and effective-contract canonical input
begins with:

```text
bytes("nexa.contract-descriptor")
u32(u32::from(ABI_DESCRIPTOR_VERSION))
bytes(domain)
encoded payload
```

Both `bytes(...)` values use the `u32` byte-length prefix from the primitive
table. The domain is the exact UTF-8 string listed below, without an implicit
namespace, terminator, or alternate prefix.

The exact domain distinguishes:

```text
type-layout
host-function
nexa-entrypoint
full-contract
effective-contract
```

The complete framed bytes are the canonical descriptor input. For a full or
effective descriptor they are also the exact bytes returned by
`AbiDescriptor::as_bytes()` or `EffectiveContractDescriptor::as_bytes()`.
The fingerprint is:

```text
BLAKE3(canonical bytes) -> [u8; 32]
```

There is no second fingerprint envelope. The prefix, converted version,
domain, lengths, fixed tags, and complete payload are all inside the single
BLAKE3 digest.

## Fixed tags

Descriptor v2 reserves the following type tags:

```text
0x01 i32       0x02 i64       0x03 f32
0x04 f64       0x05 bool      0x06 rune
0x07 string    0x10 Array     0x11 Buffer
0x12 Option    0x13 Result    0x14 Token
0x15 Snapshot  0x20 Named
```

Declaration tags are:

```text
0x30 Struct    0x31 Enum      0x32 Handle
0x40 HostFn    0x41 HostAsyncFn
0x50 NexaEntrypoint
```

Immediately after `0x50`, a Nexa entrypoint encodes one effect byte:

```text
0x00 Ordinary
0x01 Task
```

These byte values are context-specific and may also appear as enum-payload
tags. A Host function uses `0x40` or `0x41` and therefore has no additional
effect byte.

Enum-payload tags are:

```text
0x00 no payload
0x01 tuple payload
0x02 reserved; NIDL v2 does not emit or accept it
```

Attribute tags are:

```text
0x60 Fuel      0x61 Cancel    0x62 Abandon
0x63 Capability
```

Policy values also use fixed tags:

```text
Cancel:  0x01 return_error, 0x02 cancel_task
Abandon: 0x01 return_error, 0x02 trap
```

Tags are schema data, not enum discriminants inferred from Rust layout. They
must not be renumbered without a descriptor-version change.

## ABI type encoding

Primitive types encode only their fixed type tag. A unary constructor encodes
its tag followed by its element type. `Result<T, E>` encodes its tag, then `T`,
then `E`. A named type encodes:

```text
u8(0x20)
StableId(type)
bytes(source type name)
```

The source name makes a rename explicit ABI data; the Stable ID makes lookup
identity explicit and independently collision-checkable.

`Token<H>` encodes the Token tag followed by the complete named-type encoding
of declared Handle `H`. Runtime lowering derives the token value type through
the resource-token identity domain and `H`'s Stable ID; the derived identity is
not a declaration ordinal and is not interchangeable with `Token<K>`.
`Snapshot<S>` follows the same rule with a declared Struct `S` and the snapshot
identity domain.

## Declaration descriptors

All declaration descriptors start with their fixed declaration tag, Stable
ID, and source name.

A struct then encodes its ordered fields. Each field encodes field Stable ID,
field source name, and ABI type.

An enum then encodes its ordered variants. Each variant encodes variant Stable
ID, source name, payload tag, and either no payload or one ABI type.

A handle has no layout payload after its declaration identity.

A Host function then encodes:

```text
sync or async declaration tag
function Stable ID
function source name
normalized ordered attributes
ordered parameters
optional result
```

Each parameter encodes its resolved Stable ID, source name, and ABI type.
Parameters do not have independent dispatch identity, but their stable
identity and names are ABI data because they are part of generated Rust
bindings.

A Nexa entrypoint encodes:

```text
NexaEntrypoint tag
Ordinary or Task effect byte
entrypoint Stable ID
entrypoint source name
ordered parameters
optional result
```

Required versus optional is deliberately absent. Requirement belongs to a
Host use of the contract, not to the contract declaration.

Normalized Host attributes encode in fixed tag order. Fuel encodes its `u32`
value, cancellation and abandonment encode one fixed policy tag, and
capabilities encode as a lexicographically sorted set of UTF-8 byte strings.
Attribute source order therefore has no semantic effect.

`@cancel(return_error)` and `@abandon(return_error)` do not add a synthetic
error or Variant tag to the function descriptor. Their policy tags and the
encoded `Result<S, E>` error type are sufficient. Validation has already
proved that `E = i32` or contains the required unit `Cancelled` or `Abandoned`
Variant. The `i32` values `-2` for cancellation and `-1` for abandonment are
schema semantics, not separately encoded per function.

`@stable("...")` is normalized before descriptor construction. A declaration
encodes its resolved Stable ID and source name, but never the raw attribute
spelling or string a second time. This applies to the Contract, type, field,
Variant, parameter, and Host or Nexa function identities.

## Declaration fingerprints

`ValidatedContract` calculates three declaration-level fingerprint classes:

- **Type Layout Fingerprint**: the complete struct, enum, or handle descriptor.
- **Host Function Fingerprint**: the complete normalized Host function
  descriptor, including mode, policies, Fuel, capabilities, parameter order,
  and result.
- **Nexa Entrypoint Fingerprint**: the complete Nexa entrypoint descriptor,
  including Ordinary versus Task effect, parameter order, and result.

Each declaration uses its own complete prefix/version/domain/payload framing.
A fingerprint is stored alongside the corresponding validated declaration and
is reused by the full and effective descriptor builders. Implementations do
not serialize a hex representation or nest a declaration descriptor in the
full descriptor. A sequence encodes its `u32` element count followed by each
raw 32-byte declaration fingerprint.

## Canonical ordering

Order has exactly these semantics:

| Construct | Order rule |
| --- | --- |
| Struct fields | Source order is semantic |
| Enum variants | Source order is semantic |
| Function parameters | Source order is semantic |
| Top-level type declarations | Source order is not semantic |
| Host function declarations | Source order is not semantic |
| Nexa entrypoint declarations | Source order is not semantic |
| `host` versus `nexa` block | Source order is not semantic |
| Capabilities | Set order is not semantic |
| Attributes | Source order is not semantic after normalization |
| Comments, documentation, formatting | Never semantic |

Every order-insensitive declaration collection is sorted by:

```text
(declaration kind tag, source-name UTF-8 bytes, Stable ID)
```

The Stable ID is a deterministic tie-breaker, not permission for duplicate
names. Capabilities are sorted by their UTF-8 bytes. No sort uses the current
locale, Rust hash-map iteration, source position, or generated Rust name.

Changing a semantic ordered list changes its declaration fingerprint.
Reordering top-level declarations, moving the two direction blocks, changing
comments, or reformatting produces identical canonical bytes.

## Full contract descriptor

The full descriptor payload is:

```text
ContractDescriptorV2
  contract Stable ID
  contract source name
  sorted Type Layout Fingerprints
  sorted Host Function Fingerprints
  sorted Nexa Entrypoint Fingerprints
```

The full contract fingerprint identifies the entire legal surface. It is used
for whole-contract generation provenance and audits, but is not automatically
the Package invalidation key. `AbiDescriptor.bytes` is the complete
`full-contract` framed input, not only this payload, and generated
`CONTRACT_DESCRIPTOR` contains those exact bytes.

## Effective contract closure

The effective descriptor is built for one resolved Application build. Its
inputs are validated against the same `ValidatedContract` and accumulated
over the complete linked production source closure: the root Package plus
every resolved local Library dependency:

```text
referenced shared types
called Host functions
required Nexa entrypoints
Nexa entrypoints actually implemented by the Package
```

The builder computes the least fixed point of referenced shared types:

1. seed with every type directly present in the selected Host function,
   required entrypoint, and implemented entrypoint signatures;
2. add types directly referenced by every selected struct field, enum payload,
   `Array`, `Buffer`, `Option`, `Result`, `Token`, or `Snapshot`;
3. repeat until no type is added; and
4. encode the resulting Type Layout Fingerprints in canonical top-level order.

Handles are included but have no outgoing layout edge. Cycles were already
rejected or bounded by identity-bearing leaves during contract validation.

Host function and shared-type references from any linked Library are part of
the Application's selection. Only the root Application Entry Module may
implement Nexa entrypoints; a Library function with the same name does not
become an implementation. Collection follows resolved namespace bindings and
typed references, not text matches, comments, or unused Contract declarations.

The effective payload is:

```text
EffectiveContractDescriptorV2
  contract Stable ID and source name
  sorted effective Type Layout Fingerprints
  sorted actual Host Function Fingerprints
  sorted required Nexa Entrypoint Fingerprints
  sorted implemented Nexa Entrypoint Fingerprints
```

An entrypoint present in the contract but neither required nor implemented by
that Package is excluded. Therefore adding an unrelated optional entrypoint
does not invalidate every Package. If the Package implements that entrypoint,
if the Host requires it, or if it changes a type in the effective closure, the
effective fingerprint changes.

Required and implemented sets are encoded as distinct sequences. A selected
entrypoint is valid only when the Package implementation signature exactly
matches its declaration fingerprint. Missing required entrypoints and
signature mismatches are build errors, not alternative descriptor values.
`EffectiveContractDescriptor.bytes` is the complete `effective-contract`
framed input, not only this payload.

The resulting 32-byte effective Contract fingerprint, not the full-Contract
fingerprint, is embedded into that Package build's cumulative
`BuildFingerprint`. Thus a newly referenced Library Host function, a change in
the transitive type closure, or a required/implemented entrypoint change
invalidates the build. Adding or changing a Contract declaration outside the
effective closure does not. Reordering the resolved source traversal cannot
change the set, descriptor bytes, or fingerprint.

## Runtime Host-call identity

Descriptor ordering never becomes an executable function ordinal. The
`HostCall.import` operand in Bytecode v6 is a module-local index into that
module's `HostImport` metadata only. The verifier first bounds-checks the index
and validates the selected import's Stable ID, signature, sync/async mode,
Fuel, and asynchronous-result policy.

At execution, the Runtime reads the selected `HostImport` and dispatches the
generated `HostRegistry` by that metadata record's Host function Stable ID.
It must not pass the module-local index as a full-contract ordinal, effective
descriptor ordinal, or registry slot. A Package-specific effective subset may
renumber its local import table without changing Host dispatch identity, so an
effective subset cannot accidentally call a different full-contract function.

## Determinism and prohibited inputs

For identical validated semantics, all supported hosts produce byte-identical
descriptors and fingerprints. The following are forbidden as fingerprint
inputs:

```text
formatted NIDL source
canonical source strings
Rust Debug or Display output
generated Rust source
source URI or absolute path
source span
comments or documentation
hash-map iteration order
legacy function index
```

Descriptor construction is total only for `ValidatedContract`. Syntax trees
and `NidlAst` cannot bypass validation, and binding codegen cannot manufacture
or reinterpret descriptor identity.
