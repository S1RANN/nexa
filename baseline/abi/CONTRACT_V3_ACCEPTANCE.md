# Contract Syntax v3 Acceptance Matrix

Version: **1.0.0**

Milestone status: **IN PROGRESS**

This matrix is the release contract for the `.contract.nexa` migration. It
does not mark implementation complete. The milestone becomes **COMPLETE** only
when every required row has repository evidence on one candidate commit, the
independent audit accepts that commit, and the Baseline Index is updated with
the final evidence reference.

## Frozen boundaries

| Boundary | Required value | Change policy |
| --- | --- | --- |
| Executable language | `NEXA_LANGUAGE_VERSION = 2` | unchanged |
| Contract syntax | `CONTRACT_SYNTAX_VERSION = 3` | replaces the previous syntax constant and enters Build Fingerprint |
| Host Contract schema | `HOST_CONTRACT_SCHEMA_VERSION = 2` | unchanged |
| Contract Descriptor | `ABI_DESCRIPTOR_VERSION = 2` | unchanged; no framing or schema change |
| Bytecode | `BYTECODE_VERSION = 7` | unchanged |
| Active Contract file | exactly one `*.contract.nexa` per project | required |
| Compatibility | none | only a targeted migration diagnostic for an old file |

For a semantic-equivalent container migration, the Contract and declaration
Stable IDs, normalized Descriptor semantic payload, Descriptor v2 framing, and
generated Host trait, ABI type, wrapper, registry, and Nexa entrypoint API must
remain unchanged. The required syntax-version metadata rename is an audited
provenance change. A syntax-version field
inside an existing outer Build Fingerprint envelope may change once; that exact
difference must be Golden-locked and does not authorize any ABI schema change.

## Delivery matrix

Status values are `PENDING`, `PASS`, or `FAIL`. A task owner may attach evidence
but may not self-waive a required row.

| ID | Surface | Acceptance | Evidence / gate | Owner | Status |
| --- | --- | --- | --- | --- | --- |
| C01 | Source profile | Shared `SourceProfile::{Executable, Contract}` selects the same profile in compiler, CLI, LSP, and editor paths | `cargo xtask test-contract-syntax`; profile isolation fixtures | task #4, task #6, task #7 | PENDING |
| C02 | File identity | Only `*.contract.nexa` is active; Contract source stays outside the Source Module Graph and has no path-derived module identity | discovery fixtures; `contract-migration-check` | task #4, task #6 | PENDING |
| C03 | Header grammar | Exactly one first non-comment `contract Name;`; Header docs, `@stable`, and exact span retained | `cargo xtask test-contract-syntax` | task #4 | PENDING |
| C04 | Parser recovery | Missing semicolon/Header, duplicate/late Header, invalid top-level item, and executable-profile Header have precise diagnostics; later declarations survive recovery | syntax fixture snapshots | task #4 | PENDING |
| C05 | Semantic ownership | Flat declarations resolve under the Header Contract Stable ID; type/direction/attribute validation remains centralized | `cargo xtask test-contract-semantics` | task #4, task #5 | PENDING |
| C06 | Public names | Public crate is `nexa-contract`; public syntax/model/error/functions use `Contract*`, `parse_contract`, and `validate_contract` only | public API scan; workspace build | task #5 | PENDING |
| C07 | Stable IDs | Equivalent migration preserves Contract, type, function, field, parameter, and Variant Stable IDs | Stable-ID Golden corpus | task #5 | PENDING |
| C08 | Descriptor | Descriptor v2 framing/schema stays unchanged; equivalent migration preserves normalized payload and canonical bytes | `cargo xtask test-contract-descriptor`; byte Goldens | task #5 | PENDING |
| C09 | Build fingerprint | Build Fingerprint records `CONTRACT_SYNTAX_VERSION = 3`; any one-time outer-envelope difference is exact and reviewed | fingerprint Golden and review note | task #5, task #9 | PENDING |
| C10 | Codegen model | Pipeline is `ValidatedContract → ContractDescriptor → BindingModel → generated Rust`; no backend semantic redecision | `cargo xtask test-contract-codegen` | task #5 | PENDING |
| C11 | Binding behavior | Host/Nexa direction, `handle`, `Token<T>`, `Snapshot<T>`, async policies, Fuel, and capabilities remain in Descriptor/fingerprint and generated behavior | structured-codegen and real Host consumer gates | task #5, task #9 | PENDING |
| C12 | Binding determinism | Equivalent migration preserves Host/Nexa binding shapes apart from required provenance renames; generated file names the new source; repeated generation is byte-identical | generated binding Goldens; reviewed diff | task #5 | PENDING |
| C13 | CLI surface | `nexa contract check/generate` are the only direct commands; generic project commands accept the selected Contract; JSON/NDJSON exposes only Contract-named fields | `cargo xtask test-contract-cli` | task #6 | PENDING |
| C14 | Discovery safety | Resolver checks existence, suffix, project root, symlink/`..` escape, and one current Host Contract | CLI/project discovery fixtures | task #6 | PENDING |
| C15 | Migration diagnostic | An old file receives one actionable suffix/Header diagnostic without invoking a compatibility parser | CLI and parser diagnostic snapshots | task #6 | PENDING |
| C16 | Product migration | Snake, Combat, Language Scale, Hello Runtime, Standalone, REPL, CLI/editor fixtures, diagnostic corpus, and codegen fixtures use Contract v3 | `contract-migration-check`; product corpus | task #6 | PENDING |
| C17 | Editor registration | VS Code and Zed register `*.contract.nexa` as **Nexa Contract** and remove the old association | extension manifest checks | task #7 | PENDING |
| C18 | Editor parsing | Tree-sitter/TextMate support flat Header and Contract keywords | grammar/highlighting fixtures | task #7 | PENDING |
| C19 | LSP semantics | URI/config selects Contract profile; diagnostics include syntax, naming, type, attribute/direction, Stable-ID, and generated-Rust-name collisions | `cargo xtask test-contract-lsp` | task #7 | PENDING |
| C20 | Outline | Outline contains Contract, Struct, Enum, Handle, Host Function, and Nexa Function symbols | LSP snapshot fixtures | task #7 | PENDING |
| C21 | Documentation | Contract Language, Descriptor, Codegen, Host Binding, CLI/development loop, editor support, migration guide, Roadmap, and Baseline Index agree on v3 boundaries | link check; terminology scan | task #8 | PENDING |
| C22 | Split gates | All seven Contract commands exist, fail closed, and write reproducible evidence | task #9 receipts | task #9 | PENDING |
| C23 | Zero old surface | No active old-extension file, public old-name API, old CLI command, or old editor association remains | `cargo xtask contract-migration-check`; repository scan | task #9 | PENDING |
| C24 | Workspace regression | Complete workspace regression and all product examples pass on the candidate commit | clean full gate receipt | task #9 | PENDING |
| C25 | Independent audit | Architecture, traversal security, Stable IDs, Descriptor determinism, public surface, and release evidence accepted independently | task #10 audit report | task #10 | PENDING |

## Required gate set

The candidate commit runs these commands as independent named gates:

```text
cargo xtask test-contract-syntax
cargo xtask test-contract-semantics
cargo xtask test-contract-descriptor
cargo xtask test-contract-codegen
cargo xtask test-contract-cli
cargo xtask test-contract-lsp
cargo xtask contract-migration-check
```

The gate implementation may compose existing lower-level tests, but each
command must report its own success/failure and must fail when its required
surface is absent. The final workspace regression runs after all seven.

The migration check may contain the superseded spelling only in the migration
guide, the targeted diagnostic implementation, and negative fixtures that
prove rejection. Those occurrences must be explicitly allowlisted. Product
sources, public docs, exported symbols, commands, editor manifests, generated
output, and positive fixtures are not allowlisted.

## Required Golden set

The committed Golden corpus locks at least:

- Contract Stable ID;
- each declaration Stable ID;
- canonical Descriptor v2 bytes;
- full and effective Contract fingerprints;
- Build Fingerprint syntax-version contribution;
- generated Rust public API and complete source;
- precise Contract Header and declaration source spans;
- deterministic parser recovery after an early malformed item.

Goldens are regenerated only through an explicit review. A blanket fixture
rewrite is not evidence that semantics were preserved.

## Final release decision

The release candidate is rejected if any required row is `PENDING` or `FAIL`,
if evidence comes from different commits, if an unreviewed generated-binding
diff remains, or if a compatibility alias survives. Once task #10 accepts the
single candidate commit, the coordinator records the commit and evidence path
here and changes both this matrix and the Baseline Index milestone status to
**COMPLETE**.

```text
Candidate commit: PENDING
Gate evidence: PENDING
Independent audit: PENDING
Final decision: PENDING
```
