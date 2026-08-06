# Package Sources

`PackageSource` separates package discovery from package execution. A source
has a stable `SourceId`, a host-owned `PackagePolicy`, and returns deterministic
`PackageCandidate` values. `nexa-embed` does not know whether a source
represents application content, licensed content, or a reviewed extension.

## MemorySource

`MemorySource` accepts manifest/source pairs. It is suitable for embedded
fixtures, generated content, tests, and applications that obtain source
through a host-controlled mechanism. It proves the core has no filesystem
dependency.

## DirectorySource

`DirectorySource` scans direct child directories only:

```text
root/
  publisher.package/
    package.toml
    src/
      publisher/
        package.nexa
    nexa.lock
```

Discovery is sorted. Every package must contain a strict schema 2
`package.toml` and exactly one `source_root = "src"`. Application Entry Module
paths map uniquely to source paths. Every Source Module identity is derived
only from its normalized package-relative path. Every source path is UTF-8,
normalized and package-relative. Source roots and files are canonicalized and
must remain inside the configured package; symlinks and root escape are
rejected. Package, Module, file, use-edge, dependency-edge, and source-byte
ceilings are enforced.

Path dependencies are resolved only within the same Source root and Source ID.
A dependency target must be a Library. Packages with dependencies require an
explicit, current `nexa.lock`; discovery and compilation never rewrite it.

The M2 directory source is local. It does not download, unpack, update, or
execute remote packages and grants no network or arbitrary filesystem access.

Source IDs must be unique in one builder. Package IDs must be unique across all
sources. Duplicate candidates are all marked `Incompatible`; source order does
not select a winner.

Development mode scans the complete Manifest, Lockfile, Source Module set,
dependency closure, and selected `.contract.nexa` Host Contract. The Contract
uses `SourceProfile::Contract`, remains outside the Source Module Graph, and
does not derive module identity from its path. Contract resolution validates
the suffix, root containment, symbolic-link/`..` safety, and one-current-
Contract rule. It uses the shared resolved build input
and commit-time Build Fingerprint freshness guard, so additions, deletion,
renames, dependency changes, and ABA writes cannot commit stale candidates.
