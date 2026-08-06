# Package Source

Version: **1.0.0**

Status: M4 COMPLETE; M4R1 COMPLETE

A `PackageSource` has one stable `SourceId`, one host-defined `PackagePolicy`,
and a deterministic `discover` operation returning `PackageCandidate` values.
The generic layer does not assign product categories to sources and does not
assume candidates came from a filesystem.

`MemorySource` stores manifest/source pairs and is the canonical fixture and
programmatic source. `DirectorySource` examines only direct child directories.
Each child must contain a strict schema 2 `package.toml` and exactly one
`source_root = "src"`. The Application Entry Module maps uniquely to a source
path, and every other Source Module identity is likewise derived from its
normalized package-relative path. Canonical source roots and files must remain
inside the package directory and cannot escape through parent traversal or
symbolic links. Discovery order is stable.

Local path dependencies resolve only inside the same Source root and
`SourceId`. The target must be a Library, and a Package with dependencies must
provide a current `nexa.lock`; discovery never rewrites it.

Source IDs must be unique in one embed instance. Package IDs must be globally
unique across all sources. If multiple candidates claim one package ID, every
claim is `Incompatible`; scanning order never chooses a winner.

The project may select one current `.contract.nexa` Host Contract. That file
is resolved separately under `SourceProfile::Contract`; it never enters a
Package Source Module Graph and never receives a path-derived module identity.
Filesystem resolution requires the exact suffix, canonical containment within
the allowed project root, and rejection of parent or symbolic-link escape.
The normalized Contract source identity and `CONTRACT_SYNTAX_VERSION = 3`
participate in build freshness, while the absolute path remains outside ABI
identity.

M2 sources are local and trusted by host policy. Remote download, network
access, arbitrary filesystem access, a public package market, and an
adversarial-code sandbox are outside this specification.
