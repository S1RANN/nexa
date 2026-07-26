# Nexa Milestone 4.0R Final MVR Closure

Status: **INCOMPLETE**

Milestone 4.0R 的 `COMPLETE` 已被 Milestone 4.0R2 事实复核取代。旧审计结果仅作为
历史证据保留，不再表示当前有效完成状态。4.0R2 开始时确认以下四个未关闭契约：

- `M4R2-HOST-RETURN`：非空复杂 Host 返回仍可能由 Thunk 分配。
- `M4R2-SOURCE-SPAN`：Compiler SourceSpan 仍存在伪造回退位置。
- `M4R2-DIAGNOSTIC-CORPUS`：Diagnostic Corpus 没有真实执行发射路径。
- `M4R2-RELEASE-AUDIT`：Release Audit 的 PASS、计数和缺口状态存在硬编码。

- Implementation SHA: `a13102e1b682269b3af1e4e574c5d7edc20c2d51`
- Implementation Tree SHA: `57dd882d1d8dc6e6587e98f73af2e62fff998c12`
- Evidence SHA: `SELF`

本报告与机器证据由 `cargo run -p nexa-release-audit -- milestone4r` 从干净的implementation commit 自动生成。Evidence SHA 使用 `SELF` 表示包含本报告的evidence commit；Git 提交不能无循环地包含自身哈希。

## 26 个工作包

| WP | Commit | Result |
|---:|---|---|
| 1 | `33cfb54` | PASS |
| 2 | `938c305` | PASS |
| 3 | `d796c72` | PASS |
| 4 | `d7dfe9b` | PASS |
| 5 | `7ff9c73` | PASS |
| 6 | `ca123be` | PASS |
| 7 | `67ba463` | PASS |
| 8 | `aded355` | PASS |
| 9 | `4de5ce0` | PASS |
| 10 | `10b9ac6` | PASS |
| 11 | `6577a3f` | PASS |
| 12 | `05bddb7` | PASS |
| 13 | `fca870b` | PASS |
| 14 | `41391e2` | PASS |
| 15 | `41391e2` | PASS |
| 16 | `3af247b` | PASS |
| 17 | `3af247b` | PASS |
| 18 | `9c1fe5c` | PASS |
| 19 | `8f62703` | PASS |
| 20 | `8f62703` | PASS |
| 21 | `8f62703` | PASS |
| 22 | `8f62703` | PASS |
| 23 | `d08e09e` | PASS |
| 24 | `370fd8b` | PASS |
| 25 | `370fd8b` | PASS |
| 26 | `a13102e1b682269b3af1e4e574c5d7edc20c2d51` | PASS |

## Realm v5 与故障注入

- Worlds: 592
- Real RealmRuntime paths: 592
- Rejected event paths: 15745
- Shadow state fields: 0
- Production failure points: 15

## Host ABI

- Complex thunk cases: 25
- Allocation counts: 25 cases × 0
- All zero allocations: true

## Diagnostics

- Registered codes: 34
- Emitted codes: 34
- Independent fixtures: 34
- Source-backed 0..0 spans: 0

## Typed Snapshot

- Storage: `Arc<[u8]>`
- Codec shapes: 5
- Combat payload: `EnemyView`

## Local gates

| Gate | Result | stdout/stderr lines |
|---|---|---:|
| `complex-host-views` | PASS | 17 / 5 |
| `diagnostic-corpus` | PASS | 2 / 3 |
| `diagnostic-emission` | PASS | 7 / 3 |
| `diagnostic-spans` | PASS | 7 / 3 |
| `generated-runtime-thunks` | PASS | 8 / 3 |
| `milestone4-local-gates` | PASS | 968 / 262 |
| `real-realm-v5` | PASS | 19 / 5 |
| `typed-snapshot-codec` | PASS | 7 / 3 |
| `typed-snapshot-storage` | PASS | 17 / 5 |

## 已知范围内缺口

- `M4R2-HOST-RETURN`
- `M4R2-SOURCE-SPAN`
- `M4R2-DIAGNOSTIC-CORPUS`
- `M4R2-RELEASE-AUDIT`

Milestone 4.0R2 = **INCOMPLETE**。
