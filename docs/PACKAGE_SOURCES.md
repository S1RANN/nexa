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
    main.nexa
```

Discovery is sorted. Every package must contain `package.toml`. Entry paths are
relative and may not contain parent traversal. Both package directory and
entry are canonicalized and must remain inside the configured root/package;
symbolic-link escape is rejected. A source package-count ceiling is enforced.

The M2 directory source is local. It does not download, unpack, update, or
execute remote packages and grants no network or arbitrary filesystem access.

Source IDs must be unique in one builder. Package IDs must be unique across all
sources. Duplicate candidates are all marked `Incompatible`; source order does
not select a winner.

Development mode can call `reload_changed` at a fixed tick interval. Change
detection compares stable hashes of the manifest text and entry source. There
is no watcher thread and no platform-specific filesystem API.
