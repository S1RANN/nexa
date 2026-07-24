# Runtime Handle Specification 1.0

All externally retained runtime identities use:

```text
{ realm_id: u32, index: u32, generation: u32 }
```

Resolution order is fixed:

1. realm matches;
2. index is in range;
3. generation matches;
4. slot state permits the operation.

Reusing a slot increments its generation. A slot is retired before generation wraparound. Stale,
cross-realm, out-of-range, and terminal-state handles return structured errors and never expose a
payload reference.
