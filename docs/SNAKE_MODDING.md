# Snake Modding

Snake M2 accepts trusted local packages placed under
`examples/snake-game/packages/mods/<package>/`. Each package contains
`package.toml` and a Nexa entry source, uses the shared `snake_api.nidl`, and
exports:

```nidl
export OnEvent(event: SnakeEvent) -> Array<SnakeCommand>;
```

This is a reviewed developer extension mechanism, not a hostile-code sandbox
or public marketplace.

## Events and commands

Packages receive immutable snapshots for package enable, game start/tick,
score change, food spawn request/eat, game end, and settings change. Host calls
only format text or log diagnostics. Gameplay changes must be returned as
commands.

Every returned batch is checked before any command applies:

- the package owns the required capability;
- the command is legal for the current event;
- item IDs and numeric bounds are valid;
- UI updates address an item owned by the same package.

One invalid command rejects the whole package batch. Other package batches
still run. The offending package faults and every UI, skin, food, and spawn
registration owned by it is removed.

Registration IDs are local in script and become
`publisher.package:local-id` in Rust. Never hard-code another package's local
ID as your own.

## Manifest

```toml
schema = 1
id = "community.example"
name = "Example"
version = "1.0.0"
entry = "main.nexa"
priority = 100
activation = "user-controlled"
state_schema = "v1"
handler_fuel = 20000
capabilities = ["ui.register", "ui.update"]
```

Local Mods cannot request entitlements and cannot exceed the public Mod
capability ceiling.

## Reload and state

Settings Reload recompiles the current source and uses Realm Restart Reload.
Compile or migration failure leaves the old module enabled. Old Tasks are
cancelled. Compatible `@stateful` objects are retained.

`community.score-overlay` is the reference: it declares
`@stateful(1) class OverlayState`, keeps the current-session food count in its
Realm state domain, and renders the projected value after Restart Reload.
Script heap and state are not persisted across application processes.

## Safe Mode

If required rules, UI, skin, or spawn providers are unavailable, Rust Safe
Mode remains playable and uses classic scoring, a fallback skin, and safe food
placement.
