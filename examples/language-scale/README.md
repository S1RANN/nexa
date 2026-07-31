# Language Scale

This compact M4 project shows how one schema 2 Application can be split into
Source Modules and statically link a schema 2 local Library. The Application
uses module-private, `pub(package)`, and `pub` declarations, an explicit Host
alias, stable identities, constants, comments, string interpolation, loop
control, and pure Package Tests.

```text
packages/
  app/
    package.toml
    nexa.lock
    src/language_scale/
      app.nexa
      flow.nexa
      rules.nexa
      text.nexa
    tests/basic/scoring.nexa
  snake-common/
    package.toml
    src/math.nexa
```

From the repository root:

```bash
cargo run -p nexa-cli -- lock --project examples/language-scale/nexa.dev.toml
cargo run -p nexa-cli -- check --project examples/language-scale/nexa.dev.toml
cargo run -p nexa-cli -- test --project examples/language-scale/nexa.dev.toml
```

`lock` is the only command above that writes `nexa.lock`; `check` and `test`
require the checked-in lockfile to be current.
