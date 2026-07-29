# Package Source

Status: M2 COMPLETE

A `PackageSource` has one stable `SourceId`, one host-defined `PackagePolicy`,
and a deterministic `discover` operation returning `PackageCandidate` values.
The generic layer does not assign product categories to sources and does not
assume candidates came from a filesystem.

`MemorySource` stores manifest/source pairs and is the canonical fixture and
programmatic source. `DirectorySource` examines only direct child directories.
Each child must contain `package.toml`; its declared entry is relative, cannot
contain a parent traversal, and after canonicalization must remain inside the
package directory. Package directories and entries may not escape through
symbolic links. Discovery order is stable.

Source IDs must be unique in one embed instance. Package IDs must be globally
unique across all sources. If multiple candidates claim one package ID, every
claim is `Incompatible`; scanning order never chooses a winner.

M2 sources are local and trusted by host policy. Remote download, network
access, arbitrary filesystem access, a public package market, and an
adversarial-code sandbox are outside this specification.
