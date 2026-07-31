# Nexa Package Tests

Status: M4 COMPLETE

Package tests live under `tests/**/*.nexa` and map to reserved `test.*`
Modules. Production Modules cannot import Test Modules. Test Modules may use
the Production Package's `pub(package)` and `pub` surface.

The only accepted signature is:

```nexa
@test
fn poison_food_shrinks_snake() -> bool {
    true
}
```

A test is zero-argument, Immediate, and returns `bool`. Its complete call graph
must remain pure Nexa code: it cannot reach Host calls, Tasks, `await`,
`yield`, Activation, Migration, or persistent State APIs.

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
