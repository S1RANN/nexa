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

State handles are the exception to module-slot identity. They use:

```text
{ domain: StatefulDomainId, stable_id: StableId, generation: u32 }
```

`StatefulDomainId` and “Stateful Domain” are retained names of this internal
Runtime ledger only. They are not Nexa source type kinds. Reader-facing state
is always an ordinary `@state(version = N) class`; analysis represents it as
Class plus state metadata.

The internal Stateful Domain survives reload. `STATE_PRESERVE` retains
generation, `STATE_REPLACE` increments it, and `STATE_DELETE` leaves old
handles permanently stale.
