# Snake Pilot

Status: M2 COMPLETE

The Snake pilot is a playable Macroquad application with a pure Rust,
headless-capable authoritative core. Rust owns fixed ticks, input, snake body,
collision, food instances, final score, total plays, random selection,
rendering, persistence, and validation of every script command.

Nexa packages receive immutable events and return `SnakeCommand` arrays.
Formatting and logging Host functions are read-only or diagnostic. Gameplay
mutation is never exposed as a direct Host call. Commands are first validated
as one package-owned batch against capabilities, event context, value bounds,
coordinates, and ownership. The whole batch applies or none of it does. A
rejected batch faults and cleans only its owner.

UI, skin, food, and spawn registries namespace every local item ID by owner.
Disable or fault removes every item from that owner. Selected items fall back
deterministically when their owner disappears.

The domain layer maps three local directories to generic source policy:
first-party required/default content, first-party entitlement-controlled
content, and reviewed user-controlled local extensions. Those category names
must not enter the public `nexa-embed` API.

Safe Mode is implemented entirely in Rust and remains playable with no package,
with failed classic rule/HUD content, without a valid skin, or without a spawn
proposal.

The M2 baseline did not cover JIT/AOT, the later Language v2 surface, user
generics, dynamic types, traits, LSP/DAP, C++/C# binding, remote packages, a
public Mod market, network or arbitrary filesystem capabilities, hostile
bytecode sandboxing, cross-module state references, old-Task migration,
completion replay, parallel business versions, or cross-process script-state
persistence.
