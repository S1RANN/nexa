# Nexa Snake Pilot

This example is a complete Macroquad Snake game with a pure Rust authoritative
core and nine real Nexa packages. The same generated `snake_api.nidl` contract
is used by required/default content, entitlement-controlled official content,
and reviewed local Mods.

Run the windowed game:

```sh
cargo run -p snake-game
```

Controls:

- Arrow keys or WASD: move; Space: restart; Escape: exit.
- PageUp/PageDown: select a package.
- E/X/R: enable, disable, or Restart Reload the selected package.
- L: grant/revoke the Food Chaos entitlement.
- K/P: cycle registered skin or spawn policy.

The settings panel shows package name/ID, source category, version, lifecycle
status, capabilities, recent error, selected skin, and selected spawn policy.
All mutations are queued and applied at the game Tick safe point.

Headless validation:

```sh
cargo xtask test-snake
cargo xtask snake-headless-smoke
cargo xtask snake-stress
cargo xtask snake-bench
```

`snake-stress` runs 36,000 game ticks, 100 enable/disable cycles, 100 Restart
Reload cycles, and 100 entitlement lock/unlock cycles, then verifies transient
Runtime ledgers and owned registries. `snake-bench` dispatches 1,000 events
through eight enabled packages and enforces p95 ≤ 4ms and p99 ≤ 8ms.

Rust owns collision, score, length bounds, food placement validation, final
state, persistence, and rendering. Packages return commands; command batches
are atomic per package. With all packages unavailable, Safe Mode remains
playable.
