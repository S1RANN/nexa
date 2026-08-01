# Incremental Package Analysis

Status: M4 COMPLETE; M4R1 COMPLETE

Nexa uses an explicit in-memory Query Database. It does not rely on process
global caches or wall-clock timing to decide correctness.

Stable analysis input identity is:

```text
SourceKey = Package ID + normalized package-relative path
```

`FileId` is allocated only when producing a complete Artifact and is
deterministic for the sorted `(Package ID, path)` closure. Its numeric value is
not stable across Candidates.

The query boundaries are:

```text
parse(SourceKey)
module_headers(ModulePath)
resolved_uses(ModulePath)
typed_module(ModulePath)
package_public_api(PackageId)
package_state_schema(PackageId)
linked_artifact(RootPackage)
```

Invalidation follows semantic reachability:

- A private implementation change reparses and reanalyzes its Module.
- A `pub(package)` surface change invalidates the exact reverse-use closure
  in the same Package.
- A `pub` surface change also invalidates consuming dependency Packages.
- A Library implementation change with an unchanged Public API reuses
  consumer semantic analysis, but still relinks the complete Artifact.

Every build produces a full Package Artifact. Incremental analysis never
changes the Runtime boundary or enables Module-level hot replacement.

## Freshness

Initial Enable, Manual Reload, Auto Reload, CLI `dev`, and LSP analysis use the
same resolved build input. Before Commit, the Engine rereads the complete root
Source Set, Manifest, Lockfile, dependency closure, and Host Contract. A
Generation or Build Fingerprint mismatch terminates the Candidate as
Superseded.

The following fingerprints are distinct:

- `SourceSetFingerprint`: normalized paths and source contents.
- `PublicApiFingerprint`: public signatures, effects, layouts, field
  mutability, async effects, NIDL entry signatures, and public Const values.
- `StateSchemaFingerprint`: `@state` Class metadata and stable field
  identities.
- `BuildFingerprint`: root and dependency source closure, the Package-specific
  effective Contract fingerprint, language/compiler/bytecode versions,
  effective options, and resolved Lock Graph.

All four use versioned, domain-separated, length-prefixed BLAKE3 encoding.

The effective Contract selection accumulates Host functions and shared types
referenced by the root Package and every linked local Library, then closes
transitively over their ABI layouts. Required and actually implemented Nexa
entrypoints are included; unrelated Optional declarations are not. That
canonical set is order-independent and its raw 32-byte fingerprint is one
field of the cumulative Build Fingerprint. An unrelated Contract addition
therefore does not invalidate a Package, while a newly referenced Library Host
call does.
