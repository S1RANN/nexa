# Nexa Package Tests

Status: M4R1 COMPLETE

Package tests live under `tests/**/*.nexa` and receive path-derived identities
in the reserved `test::` namespace. Production Modules cannot use Test
Modules. Test Modules may use the Production Package's `pub(package)` and
`pub` surface:

```nexa
use package::food::effects;
```

The only accepted test signature is a zero-argument, synchronous function
returning `bool`:

```nexa
@test
fn poison_food_shrinks_snake() -> bool {
    return true;
}
```

Its complete call graph must remain pure Nexa code. It cannot reach Host calls,
async functions, `.await`, `yield`, lifecycle-attributed functions, migration,
or persistent State APIs. Pure standard-library functions and pure Production
Package helpers are allowed.

Each test runs in an independent Realm, Heap, and State with a deterministic
rejecting Host:

- `true`: PASS
- `false`: FAIL
- Trap or Fuel Exhaustion: ERROR

Results include Package, Module, test name, source location, Nexa call stack,
instruction count, and Fuel. Test sources do not contribute to product Source
or Build Fingerprints, Public API, State Schema, or Runtime Artifacts.

```bash
nexa test <package-directory> --contract app_api.nidl
nexa test --project nexa.dev.toml
```
