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

- Main menu: Up/Down or W/S to select; Enter/Space to confirm.
- In game: Arrow keys or WASD to move; Space to restart; Escape to pause.
- Pause menu: Continue, Settings, or Back to Main Menu.
- Settings: Up/Down selects; Left/Right changes skin or spawn policy.
- Package settings: Enter enables/disables; R reloads; L grants/revokes DLC access.

The settings screen shows package name/ID, source category, version, lifecycle
status, capabilities, recent error, selected skin, and selected spawn policy.
Gameplay is paused while a menu is open. Package mutations remain queued and
are applied at the game Tick safe point.

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
