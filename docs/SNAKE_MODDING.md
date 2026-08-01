# Snake Modding

Snake accepts reviewed local Packages placed under
`examples/snake-game/packages/mods/<package>/`. Each Package contains a schema
2 `package.toml`, a `src/` Source Module tree, and uses the shared NIDL v2
Snake Contract. The Contract declares all Host calls in `host {}` and all
legal Nexa entrypoints in `nexa {}`:

```nidl
contract Snake {
    enum SnakeEvent {
        GameStarted(GameSnapshot),
        GameEnded(GameSnapshot),
    }

    enum SnakeCommand {
        AddScore(i32),
        ShowToast(string),
    }

    host {
        fn format_score(score: i32) -> string;
    }

    nexa {
        fn on_event(event: SnakeEvent) -> Array<SnakeCommand>;
        fn choose_food_spawn(
            context: FoodSpawnContext,
        ) -> Option<Cell>;
        fn calculate_food_effect(
            context: FoodEatenContext,
        ) -> FoodEffect;
    }
}
```

The corresponding entry Source Module keeps every Host symbol behind the
Contract namespace and publishes only the entrypoints it implements:

```nexa
use host::snake;

pub fn on_event(
    event: snake::SnakeEvent,
) -> Array<snake::SnakeCommand> {
    let output: Array<snake::SnakeCommand> = Array::new();
    return output;
}
```

Module identity comes only from the path under `src/`; source files do not
contain a module declaration. Functions are `snake_case`, types and Variants
are `PascalCase`, and namespace or associated-item paths use `::`.

This is a reviewed developer extension mechanism, not a hostile-code sandbox
or public marketplace.

## Events, commands, and entrypoints

Packages receive immutable snapshots for package enable, game start/tick,
score change, food spawn request/eat, game end, and settings change. Host calls
only format text or log diagnostics. Gameplay changes must be returned as
commands.

The Snake Host uses entrypoints according to their domain role:

- `on_event` is an Optional broadcast to every enabled Package that implements
  it;
- `choose_food_spawn` is called only on the selected spawn provider;
- `calculate_food_effect` is called only on the Package that owns the food.

Absence of an Optional entrypoint does not prevent Package enable. If a Package
does implement one, its stable ID and exact signature must match the effective
Contract or loading fails. Required entrypoints are selected by the Engine
through typed marker APIs, not encoded in NIDL.

Every returned command batch is checked before any command applies:

- the Package owns the required capability;
- the command is legal for the current event;
- item IDs and numeric bounds are valid;
- UI updates address an item owned by the same Package.

One invalid command rejects the whole Package batch. Other Package batches
still run. The offending Package faults and every UI, skin, food, and spawn
registration owned by it is removed.

Registration IDs are local in script and become
`publisher.package:local-id` in Rust. Never hard-code another Package's local
ID as your own.

## Manifest and paths

```toml
schema = 2
kind = "application"
id = "community.example"
name = "Example"
version = "1.0.0"
source_root = "src"
entry = "community.example"
priority = 100
activation = "user-controlled"
handler_fuel = 20000
capabilities = ["ui.register", "ui.update"]
```

The entry above maps to `src/community/example.nexa`. Host access is explicit:
`use host::snake;`.

Local Mods cannot request entitlements and cannot exceed the public Mod
capability ceiling.

## Reload and state

Settings Reload recompiles the current source and uses Realm Restart Reload.
Compile or migration failure leaves the old Package enabled. Old Tasks are
cancelled. Compatible state objects are retained by stable state metadata.

`community.score-overlay` is the reference. Its state is declared as an
ordinary Class carrying state metadata:

```nexa
@state(version = 1)
class OverlayState {
    @stable("foods")
    mut foods: i32,
}
```

The Package keeps the current-session food count in its Realm state domain and
renders the projected value after Restart Reload. Script heap and state are not
persisted across application processes.

## Safe Mode

If required rules, UI, skin, or spawn providers are unavailable, Rust Safe
Mode remains playable and uses classic scoring, a fallback skin, and safe food
placement.
