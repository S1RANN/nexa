# Nexa Bytecode MVR 1.0

The MVR bytecode is a fixed-endian, sectioned, versioned typed-register format. Runtime execution
accepts only a `VerifiedModule`.

Required sections:

```text
Header
Strings
Types
Constants
HostImports
Functions
Code
RootBitmaps
Safepoints
LoopBounds
SourceMap
ReloadMetadata
```

The core verifier independently validates section bounds, instruction boundaries, CFG edges,
register types, call signatures, field slots, root bitmaps, safepoints, frame quotas, host
signatures, and immediate-function WCET. Immediate loops require an explicit static upper bound;
recursive immediate call graphs are rejected.
