# Active Test Matrix

| Test | Category | Protected behavior | Failure means | Product API | Model / fault injection |
|---|---|---|---|---|---|
| `nexa-runtime/tests/task_lifecycle.rs` | Task Lifecycle, Resource Lifecycle, Gameplay Integration | 16 public lifecycle outcomes, request-handle safety, exact release counts, terminal ledger | Public Task or resource contract regressed | Yes | Failure probes in capacity/cleanup cases |
| `nexa-runtime/tests/restart_reload.rs` | Restart Reload, Resource Lifecycle | 16 restart, rollback, late-completion, release, and activation outcomes | Restart Reload semantics regressed | Yes | Activation probe only |
| `nexa-runtime/tests/runtime_baseline.rs` and `runtime-baseline/fast_complete.rs` | Task Lifecycle | Fast completion and terminal record | Interpreter-driven completion changed | `poll_task` | No |
| `runtime-baseline/fuel_yield.rs` | Task Lifecycle | Fuel suspension preserves continuation | Fuel resume contract changed | Baseline oracle | No |
| `runtime-baseline/explicit_yield.rs` | Task Lifecycle | Explicit yield resumes once | Yield semantics changed | Baseline oracle | No |
| `runtime-baseline/gc_suspended_root.rs` | Resource Lifecycle | Suspended roots survive GC and reclaim after cancellation | Root map or cleanup regressed | Baseline oracle | No |
| `runtime-baseline/nested_call.rs` | Bytecode / Verifier | Nested call frame and return | Call lowering or frames regressed | Baseline oracle | No |
| `runtime-baseline/resource_token.rs` | Host Binding, Resource Lifecycle | Token ownership and release | Host resource ledger regressed | Baseline oracle | No |
| `runtime-baseline/scope_cancel.rs` | Task Lifecycle | Scope cancellation reaches Task terminal | Structured cancellation regressed | Baseline oracle | No |
| `runtime-baseline/trap.rs` | Bytecode / Verifier, Task Lifecycle | Trap context and terminal record | Trap propagation regressed | Baseline oracle | No |
| `nexa-model/tests/task_differential.rs` | Model Differential | Task machine matches executable kernel | Runtime/model transition mismatch | No | Model |
| `nexa-model/tests/scope_differential.rs` | Model Differential | Scope machine matches executable kernel | Scope ownership mismatch | No | Model |
| `nexa-model/tests/task_scope_system_differential.rs` | Model Differential | Coupled Task/Scope invariants | Cross-machine invariant mismatch | No | Model |
| `nexa-model/tests/realm_differential.rs` | Model Differential | Current restart/task/request/resource oracle | Runtime/model mismatch | No | Model |
| `nexa-model/tests/realm_failure_differential.rs` | Model Differential | Rejected operations preserve state | Failure classification or atomicity mismatch | No | Model |
| `fuzz/bytecode*`, `fuzz/verifier`, `fuzz/root-map`, `fuzz/wcet` | Fuzz, Bytecode / Verifier | Decode, verify, root map, WCET safety | Panic, acceptance bug, or bound violation | No | Fuzz |
| `fuzz/host-import`, `fuzz/idl` | Fuzz, Host Binding | Host ABI decoding and IDL canonicalization | Panic or inconsistent binding | No | Fuzz |
| `fuzz/state-schema`, `fuzz/realm-events` | Fuzz, Restart Reload | State schema, migration arena, Realm sequences | Panic or invariant violation | No | Fuzz/model |
| `fuzz/source` | Fuzz, Parser / Type Checker | Arbitrary source is diagnosed safely | Compiler panic or invalid acceptance | No | Fuzz |
| `tools/allocation-observer` | Performance / Allocation | Zero-allocation promotion/resume/completion paths | Absolute allocation guarantee regressed | Yes | Targeted failure probes |
| `tools/benchmark-v6` | Performance / Allocation, Gameplay Integration | Absolute latency budgets on real runtime paths | Product path exceeds budget | Yes | No |

`support.rs` is shared construction/assertion code, not an independent test.
Unit tests embedded in `crates/*/src` cover Parser / Type Checker, Bytecode /
Verifier, Host Binding generation, Runtime Inspection, Migration Core, and
state-machine internals.
