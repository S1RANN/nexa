# Migrating to Contract v3

Contract v3 is a one-time, source-breaking rename and container migration. It
does not provide a compatibility parser, crate alias, CLI alias, editor file
association, or public API alias. Migrate the complete Host boundary and then
regenerate bindings from one clean source tree.

## 1. Rename the source file

Rename every old Contract file:

```text
app_api.nidl
→ app_api.contract.nexa
```

Update the project manifest:

```toml
contract = "app_api.contract.nexa"
```

The selected path must exist, have the exact suffix, and remain inside the
allowed project root after canonicalization. A project selects exactly one
current Host Contract.

## 2. Flatten the Contract container

Move Header documentation and attributes immediately before the new Header,
replace the opening block with a semicolon, and remove the final Contract
brace:

```nexa
/// Application Host ABI.
@stable("app-api")
contract AppApi;

struct Event {
    kind: i32,
}

host {
    fn log(message: string);
}

nexa {
    fn on_event(event: Event);
}
```

`contract AppApi;` must be the first non-comment declaration and appear once.
All following `struct`, `enum`, `handle`, `host`, and `nexa` declarations
belong to that Contract automatically.

Do not retain both files. An old extension receives a targeted migration
diagnostic; it is never accepted as an alternate input.

## 3. Rename Rust dependencies and APIs

Change the workspace/package dependency and Rust module:

```text
nexa-idl      → nexa-contract
nexa_idl      → nexa_contract
```

Replace public model and frontend names:

| Previous name | Contract v3 name |
| --- | --- |
| `NidlAst` | `ContractAst` |
| `NidlSyntaxTree` | `ContractSyntaxTree` |
| `NidlDiagnostic` | `ContractDiagnostic` |
| `NidlType` | `ContractType` |
| `NidlFunction` | `ContractFunction` |
| `NidlStruct` | `ContractStruct` |
| `NidlEnum` | `ContractEnum` |
| `NidlHandle` | `ContractHandle` |
| `parse_nidl` | `parse_contract` |
| `validate_nidl` | `validate_contract` |

There are no deprecated aliases. Update call sites atomically with the crate
rename.

Build scripts generate from the renamed path:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    nexa_contract::build::generate("app_api.contract.nexa")?;
    Ok(())
}
```

## 4. Update commands and automation

Replace the old command group:

```bash
nexa contract check app_api.contract.nexa
nexa contract generate app_api.contract.nexa
```

Update `check`, `build`, `test`, `dev`, and `lock` invocations that pass a
Contract path. Machine-readable consumers must read:

```text
contractPath
contractSyntaxVersion
contractDiagnostic
```

Delete consumers of the previous field names; no dual-write interval exists.

## 5. Update editor configuration

Associate `*.contract.nexa` with **Nexa Contract** in VS Code and Zed. Remove
the old extension association. Tree-sitter and TextMate consumers must parse
the flat Header and highlight `contract`, `host`, `nexa`, and `handle`.

The LSP selects `SourceProfile::Contract` from the URI and project manifest.
If editor diagnostics differ from `nexa contract check`, treat that as a
profile-resolution defect rather than configuring two parsers.

## 6. Regenerate and review identity

Regenerate every Rust binding and inspect the diff. For a semantic-equivalent
container migration, these remain unchanged:

- Contract and declaration Stable IDs;
- normalized Descriptor semantic payload and Descriptor v2 framing;
- generated Host trait, ABI type, wrapper, registry, and Nexa entrypoint API;
- Host/Nexa direction, async policy, capabilities, token identity, and snapshot
identity.

The required syntax-version metadata rename is an audited provenance change;
it is not permission to change the generated Host/Nexa binding shapes.

The generated source provenance comment changes to the new file name and the
Build Fingerprint records `CONTRACT_SYNTAX_VERSION = 3`. If an existing outer
serialization envelope includes the syntax version, its bytes/fingerprint may
change once; that difference must be Golden-locked and reviewed. No other
Descriptor or binding difference is implied by flattening the container.

## 7. Remove the old surface

Before merging, verify all active products, examples, fixtures, docs, build
scripts, editor manifests, and automation use Contract v3. The release gate
requires:

```text
zero active *.nidl files
zero public NIDL API
zero NIDL CLI commands
zero NIDL editor associations
all Contract Descriptor Goldens passing
all generated binding differences absent or explicitly reviewed
```

Historical immutable tags may retain the former surface. The active source
tree must not expose it as a supported input.
