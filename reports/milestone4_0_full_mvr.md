# Nexa Milestone 4.0 Full MVR Closure

Status: **COMPLETE**

Milestone 4.0 的 56 个工作包均已完成。最终实现 SHA（不含本报告提交）为
`1fda503394912cedeccaaa05d4b7ddd090d9695b`。

本报告是 WP56 的唯一产物。Git 提交不能包含自身 SHA，因此下表将 WP56 标为
`SELF`；它表示包含本文件的提交，而不是缺失或待办。

## 工作包与提交

| WP | 提交 | 完成内容 |
|---:|---|---|
| 1 | `16abf36` | 修正 Milestone 3.3B 事实与证据 |
| 2 | `f7ed915` | 统一公开 Nexa 错误分类 |
| 3 | `81232ea` | RuntimeHost Open/Closing/Closed 生命周期 |
| 4 | `23e9139` | 统一 Host admission gate |
| 5 | `6dec163` | Completion exactly-once |
| 6 | `de72dc9` | 统一 RuntimeResourceLedger |
| 7 | `4133de8` | Async Host admission 零分配 |
| 8 | `7718568` | Typed Result 写回零分配 |
| 9 | `a4b45c1` | Task resume 与 cleanup 零分配 |
| 10 | `7a3d695` | Release 与 Realm drop 零分配 |
| 11 | `11db020` | 移除 Runtime 热路径 String |
| 12 | `ce3e76c` | 稳定错误代码表 |
| 13 | `1fde203` | Verified ReloadMetadata |
| 14 | `a5be753` | Reload 仅由 verified metadata 驱动 |
| 15 | `6abfa39` | Migration arena 硬容量与 ownership |
| 16 | `cd4224b` | Migration 峰值容量报告 |
| 17 | `628fba1` | 确定性 Migration hash |
| 18 | `7e97fe7` | Typed StateHandle 语义 |
| 19 | `7d81980` | 多代 retired epoch 精确身份管理 |
| 20 | `167ea61` | 穷举 Realm model v5 |
| 21 | `d87b643` | Realm v5 全路径 runtime differential |
| 22 | `92fea2a` | 统一确定性 runtime fault injection |
| 23 | `d9aa18f` | 故障边界 differential |
| 24 | `d95e6b7` | 完整 JSON model failure artifact |
| 25 | `dd712e2` | MVR fuzz target suite |
| 26 | `51d1888` | 稳定 state fixture 格式 |
| 27 | `c5bd6c0` | 真实离线 `migrate-check` |
| 28 | `8a77c81` | 确定性 migration state/diff/hash 输出 |
| 29 | `cac0b0e` | Compiler IR 全程保留 source span |
| 30 | `ae177ee` | 统一结构化 Diagnostic 渲染 |
| 31 | `ac80467` | Bytecode SourceMap 生成和校验 |
| 32 | `b15c892` | Runtime trap 到脚本 source stack |
| 33 | `0aa0783` | Bytecode v4 独立 sections |
| 34 | `e92786e` | Section directory 完整校验 |
| 35 | `d78d804` | 全量 bytecode decode limits |
| 36 | `573e958` | 确定性 bytecode dump/verify |
| 37 | `6e849f2` | 完整 scalar type pipeline |
| 38 | `6732554` | Immutable UTF-8 String |
| 39 | `af89376` | 用户 payload enum |
| 40 | `b78fd90` | Immutable nominal struct |
| 41 | `fcdf9ab` | Mutable nominal class |
| 42 | `3fbf50b` | Persistent stateful class |
| 43 | `8afae15` | Generic Array |
| 44 | `d546366` | Generic Map |
| 45 | `cec964b` | VM-owned copy Buffer |
| 46 | `748d389` | Typed host Snapshot |
| 47 | `b19b9b6` | 完整 IDL boundary type system |
| 48 | `35958da` | Exact interface ABI hash 全覆盖 |
| 49 | `17df67b` | 生成 typed Rust host bindings |
| 50 | `878f7d3` | 生成 direct zero-allocation host thunks |
| 51 | `4525d7e` | 完整 Nexa CLI 命令集 |
| 52 | `a021d7c` | Combat Runtime 完整 MVR 试点 |
| 53 | `e42bd6d` | Benchmark v6 |
| 54 | `51390a8` | H1/H2/H3 本地实验 |
| 55 | `1fda503` | Milestone 4.0 最终本地门禁 |
| 56 | `SELF` | 本最终完成报告 |

## 语言功能矩阵

| 功能 | Parser/AST/HIR | 类型检查 | Bytecode/Verifier | Runtime | 真实接入 |
|---|---|---|---|---|---|
| `i32`, `i64`, `f32`, `f64`, `bool`, `rune` | 完成 | 完成 | 完成 | 完成 | Combat/CLI |
| Immutable UTF-8 `String` | 完成 | 完成 | 完成 | 完成 | Combat/Benchmark |
| Payload `enum` 与穷尽 `match` | 完成 | 完成 | 完成 | 完成 | Combat/Benchmark |
| Immutable nominal `struct` | 完成 | 完成 | 完成 | 完成 | Combat/Benchmark |
| Mutable nominal `class` | 完成 | 完成 | 完成 | 完成 | Combat/Benchmark |
| Persistent `stateful class` | 完成 | 完成 | 完成 | 完成 | Combat/H3 |
| `Array<T>` | 完成 | 完成 | 完成 | 完成 | Combat/Benchmark |
| `Map<K,V>` | 完成 | 完成 | 完成 | 完成 | Combat/Benchmark |
| `Buffer<T>` | 完成 | 完成 | 完成 | 完成 | Combat/Benchmark |
| `Option<T>` / `Result<T,E>` | 完成 | 完成 | 完成 | 完成 | Combat/Benchmark |
| `?` 与 Result error propagation | 完成 | 完成 | 完成 | 完成 | Combat |
| Async request/await/completion | 完成 | 完成 | 完成 | 完成 | Combat/Benchmark |
| Typed `Snapshot<T>` | 完成 | 完成 | 完成 | 完成 | Combat/Benchmark |
| Typed `StateHandle<T>` | 完成 | 完成 | 完成 | 完成 | Combat/H3 |
| Migration/activation/reload | 完成 | 完成 | 完成 | 完成 | Combat/H3/migrate-check |

## Bytecode v4 Section 矩阵

全部 16 个 section 均具有独立 kind、directory entry、编码/解码、limit 和 verifier
覆盖。Directory 还验证 magic/version、offset、alignment、重叠、重复、缺失、未知
section、长度与尾部数据。

| Section | 生成 | 解码限制 | Verifier |
|---|---|---|---|
| strings | 完成 | 完成 | 完成 |
| types | 完成 | 完成 | 完成 |
| constants | 完成 | 完成 | 完成 |
| enums | 完成 | 完成 | 完成 |
| structs | 完成 | 完成 | 完成 |
| classes | 完成 | 完成 | 完成 |
| host-imports | 完成 | 完成 | 完成 |
| state-schemas | 完成 | 完成 | 完成 |
| exports | 完成 | 完成 | 完成 |
| functions | 完成 | 完成 | 完成 |
| code | 完成 | 完成 | 完成 |
| root-maps | 完成 | 完成 | 完成 |
| safepoints | 完成 | 完成 | 完成 |
| loop-bounds | 完成 | 完成 | 完成 |
| source-map | 完成 | 完成 | 完成 |
| reload-metadata | 完成 | 完成 | 完成 |

`nexa build`, `nexa verify` 和 `nexa dump` 使用同一物理格式；最终 bytecode corpus
门禁已执行真实 compile → encode → decode → verify。

## Diagnostic Code 覆盖

稳定 registry 共 34 个代码，均有机器可读分类和独立渲染，不依赖 `Debug` 文本。

| 范围 | 数量 | 覆盖 |
|---|---:|---|
| NX1001–NX1002 | 2 | lexer/parser unexpected character/token |
| NX2001–NX2002 | 2 | unknown name/type |
| NX2101 | 1 | type mismatch |
| NX2201–NX2202 | 2 | match exhaustiveness/duplicate variant |
| NX2210 | 1 | constructor type inference |
| NX2220–NX2221 | 2 | `?` result requirement/error mismatch |
| NX2301–NX2302 | 2 | task/await effect |
| NX2401 | 1 | numeric conversion |
| NX2501 | 1 | field access |
| NX2601–NX2604 | 4 | migration compile-time contract |
| NX3001–NX3004 | 4 | bytecode/register/root-map/SourceMap |
| NX4001–NX4003 | 3 | host ABI/capability/argument |
| NX5001–NX5004 | 4 | host result/abandon/error/resource capacity |
| NX6001–NX6005 | 5 | migration/reload/activation/metadata |
| **合计** | **34** | **完整** |

最终门禁还逐字比较了 10 个确定性 runtime diagnostic snapshots。

## SourceMap 覆盖

| 边界 | 覆盖结果 |
|---|---|
| Lexer/Parser → AST | 每个语法节点保留 `SourceSpan` |
| AST → HIR/typed IR | 调用、match、enum、migration 和 safepoint span 保留 |
| Compiler → bytecode | 指令范围映射写入独立 `source-map` section |
| Encode/decode | v4 round-trip，具有独立 `max_source_map_entries` 限制 |
| Verifier | 校验范围、顺序、函数/指令边界和文件引用 |
| Runtime | 每个 Nexa frame 与 trap 均可解析为脚本 source stack |
| CLI | `dump` 可确定性输出 SourceMap |

## IDL 类型矩阵

| IDL 类型 | 解析/校验 | Exact hash | Rust binding | Host thunk |
|---|---|---|---|---|
| `i32`, `i64`, `f32`, `f64`, `bool`, `rune`, `string` | 完成 | 完成 | 完成 | 完成 |
| `HostRequest<T>` | 完成 | 完成 | 完成 | 完成 |
| `ResourceToken<T>` | 完成 | 完成 | 完成 | 完成 |
| `Snapshot<T>` | 完成 | 完成 | 完成 | 完成 |
| `Array<T>` | 完成 | 完成 | 完成 | 完成 |
| `Buffer<T>` | 完成 | 完成 | 完成 | 完成 |
| `Option<T>` | 完成 | 完成 | 完成 | 完成 |
| `Result<T,E>` | 完成 | 完成 | 完成 | 完成 |
| Named struct | 完成 | 完成 | 完成 | 完成 |
| Named payload enum | 完成 | 完成 | 完成 | 完成 |
| Named opaque handle | 完成 | 完成 | 完成 | 完成 |

Exact hash 覆盖类型图、字段、variant payload、函数种类、参数、返回值、effect、
fuel/policy 与 capability；H1 的真实接口变更同时改变 hash 和生成代码，旧 hash 在
Realm load 边界被拒绝。

## Allocation 结果

全局 allocator observer 在 Rust 1.97.1 上独立重复 3 次。每次的 promotion、resume、
trace-off 和 Realm-drop-transfer 四条严格 runtime contract 路径均为 **0 次系统分配**，
`all_hot_paths_zero=true`。WP7–WP10 另行覆盖 async admission success/capacity/cancel、
typed Ok/Err/cancel/abandon/heap-full writeback、fuel/explicit/host resume、全部 terminal
cleanup、release、drain 与 drop。

Benchmark v6 的 allocator 数是完整计时操作的总分配量，包含语言值和 benchmark
场景有意创建/保留的对象；它不替代也不削弱上述严格热路径 observer 的零分配结论。

## Model、Differential 与故障注入

所有模型均完成穷举，未降低深度、未截断，且无 invariant failure。

| 模型 | Worlds | Differential paths | 结果 |
|---|---:|---:|---|
| Realm v3 | 929 | 929 | PASS |
| Realm v4 task/runtime | 16 | 16 | PASS |
| Realm v4 dual-module routing | 18 | 18 | PASS |
| Realm v5 | 27,715 | 27,715 | PASS |
| **合计** | **28,678** | **28,678** | **PASS** |

Fault injection 共 **15** 个稳定注入点：task、scope、scheduler、frame、heap、request、
completion、release、snapshot、migration object、migration field、migration forwarding、
reload completion、activation trap 和 cleanup trap。每个 v5 world 的 runtime replay
与合法/拒绝事件及故障边界保持 differential 一致。

Failure artifact 使用 `serde` 输出合法 JSON，并包含模型版本、seed、world、事件路径、
期望/实际状态、注入点和错误分类。

## Fuzz 与 Migration Fixture

Fuzz target 共 **13** 个：

```text
bytecode_decode
verifier
register_planner
enum_match_lowering
try_operator_lowering
completion_routing
completion_ticket_terminal_race
release_intrusive_list
stateful_registry
migration_arena
migration_fixture_parser
source_map_decoder
realm_event_sequence
```

最终本地门禁直接构建 13 个 libFuzzer executable，并对各自 checked-in seed 执行
`-runs=1` smoke。

端到端 Migration Fixture 共 **1** 个：checked-in v1/v2 module 与
`fixtures/migration/state.json`。真实 `nexa migrate-check` 结果为 1 个 final object、
fuel 22，并输出确定性旧/新 schema hash、migration hash、capacity usage、state diff
与最终 fixture。

## Combat Runtime 功能覆盖

Combat 试点从 Nexa 源码真实经过 compiler、IDL hash、bytecode verifier、generated host
binding/thunk 和 Realm runtime。它覆盖 i64/f32/rune/string、struct、payload enum、
class、stateful class、array/map/buffer、typed snapshot、Option/Result/`?`/match、async
request/token、StateHandle、显式 migration、ReloadMetadata、activation，以及两代同时
存在的 retired epoch。IDL 中的 `EnemyView` 是真实 typed struct，不是示例特判。

## Benchmark v6

Release 模式完成 16 个真实 case；常规 case 1,000 samples，migration/reload/drop 各
200 samples。

| Case | ops/s | P50 ns | P99 ns |
|---|---:|---:|---:|
| immediate call | 1,183,902 | 833 | 917 |
| Result Ok/Err | 965,608 | 834 | 1,958 |
| fuel resume | 1,984,083 | 500 | 625 |
| explicit resume | 1,640,072 | 500 | 1,125 |
| string concat | 1,193,600 | 625 | 1,459 |
| struct construction | 1,330,872 | 584 | 1,250 |
| class allocation | 1,404,107 | 625 | 1,167 |
| enum construction/match | 1,388,083 | 584 | 1,125 |
| array operations | 893,329 | 917 | 2,000 |
| map operations | 639,474 | 1,208 | 2,916 |
| buffer copy | 1,021,625 | 833 | 1,667 |
| snapshot access | 1,323,348 | 667 | 1,084 |
| async admission/completion | 178,439 | 5,292 | 10,458 |
| migration | 73,833 | 12,417 | 16,709 |
| reload commit | 218,499 | 4,250 | 6,708 |
| Realm drop | 568,204 | 1,708 | 2,334 |

完整 mean/P50/P95/P99、allocator、heap、fuel、instruction 和 resource ledger 数据位于
`reports/raw/benchmark_v6.json`。

## H1/H2/H3 结论

### H1：IDL 生成价值

20 个真实 API 中，手写 glue 为 78 个维护行、20 个重复 dispatch site、接口变化需
3 个修改点；生成路径仅维护 22 行 IDL、0 个开发者维护 dispatch site、1 个修改点。
`apply_damage` 从 i32 改为 i64 后，Exact hash 和 Rust 生成代码均变化，真实 Realm
边界拒绝旧 hash。

### H2：Fast Task

32 个组合覆盖 500/1000 calls/frame、99/1 与 95/5 first-slice/promotion、trace
on/off、HostCall on/off、complex types on/off。24,000 个 task 全部完成，单机实测
吞吐范围为 603,211–1,633,768 calls/s。该结果用于本地因子比较，不声明跨机器目标。

### H3：Stateful Reload

真实 v1 → v2 → v3 schema 完成 preserve、replace、delete、waiting request、quiesce
期间 completion、rollback/replay、两次 commit、activation fault、两代 retired epoch
和 migration limit 拒绝。所有要求的结果布尔值均为 true。

## 全部本地门禁结果

| 门禁 | 结果 | 关键计数 |
|---|---|---|
| `cargo fmt --all -- --check` | PASS | workspace |
| `cargo check --workspace --all-targets` | PASS | workspace |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS | 0 warnings |
| `cargo test --workspace --all-targets` | PASS | all targets |
| `cargo test --doc --workspace` | PASS | doc tests |
| baseline check | PASS | 17 normative files、16 decisions |
| machine check | PASS | 8 machines、90 transitions、235 stable IDs |
| generated-code check | PASS | generated output clean |
| bytecode corpus check | PASS | compile/decode/verify/dump |
| model v3 | PASS | 929 worlds |
| model v4 | PASS | 16 + 18 worlds |
| model v5 | PASS | 27,715 worlds |
| v4 differential | PASS | 34 paths |
| v5 differential/failure differential | PASS | 27,715 paths、15 injection points |
| Combat Runtime | PASS | complete pilot |
| Benchmark v6 smoke | PASS | 16 cases |
| allocator observer | PASS | 3 repetitions、all measured paths zero |
| migrate-check fixtures | PASS | 1 fixture、1 final object、fuel 22 |
| diagnostic snapshots | PASS | 10 snapshots |
| fuzz smoke | PASS | 13 targets、1 run/seed |

可重复执行入口为 `scripts/milestone4-local-gates.sh`，机器可读结果位于
`reports/raw/milestone4_gate_results.json`。

## 已知 MVR 内缺口

无。

Milestone 4.0 = **COMPLETE**。
