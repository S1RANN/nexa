# Nexa Pilot — Five-minute Quick Start

Prerequisites: Rust 1.97.1, a clean checkout, and the exact repository revision selected for the
Pilot.

```sh
cargo run -p nexa-cli -- nidl check examples/combat-runtime/combat_api.nidl
cargo run -p nexa-cli -- nidl generate examples/combat-runtime/combat_api.nidl
cargo run -p nexa-cli -- build examples/combat-runtime/gameplay.nexa -o /tmp/gameplay.nxb
cargo run -p nexa-cli -- qa verify /tmp/gameplay.nxb
cargo run -p combat-runtime
```

The final command executes compiler/bytecode/verifier, hosted Realm tasks, host requests, resources,
snapshots, cancellation, migration, commit, and activation-fault handling.
