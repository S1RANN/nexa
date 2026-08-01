# Nexa Local Static Libraries

Status: M4 COMPLETE; M4R1 COMPLETE

M4 resolves only explicit local path dependencies in the same `PackageSource`
root and `SourceId`. Network, Git, registry, remote download, version ranges,
and dependency solving are intentionally unsupported.

An Application declares dependencies by alias:

```toml
[dependencies]
snake_common = { path = "../snake-common" }
```

The target must have `kind = "library"`. An alias must be a valid Nexa
identifier. A resolved closure may contain only one version and one path for a
Package ID, and dependency cycles are rejected.

Library Manifests contain only common identity fields, `source_root`, and
dependencies. They cannot declare an Entry, Activation, Migration, entrypoint,
Capability, Entitlement, or Runtime lifecycle configuration. Library code is
statically analyzed and linked against the consuming Application's Host
Contract and effective Capability set; it never creates a Realm.

## Lockfile

A root Package with dependencies must have an adjacent `nexa.lock`. Only these
commands write it:

```bash
nexa lock <package-directory>
nexa lock --project nexa.dev.toml
```

The lock records schema, root Package ID, Package ID/version/source-root
relative path tuples, and sorted dependency edges. It never records absolute
paths, source hashes, or API hashes. `check`, `build`, `dev`, `test`, and
`NexaEngine` reject a missing or stale lock without changing the active
Runtime or Last Known Good Artifact.
