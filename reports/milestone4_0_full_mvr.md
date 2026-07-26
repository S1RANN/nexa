# Nexa Milestone 4.0R3 端到端诊断与可信证据最终收口

Status: **INCOMPLETE**

当前重新打开的真实缺口：

- `M4R3-RUNTIME-DIAGNOSTIC`
- `M4R3-GATE-PROVENANCE`
- `M4R3-AUDIT-DERIVATION`
- `M4R3-EVIDENCE-RECEIPT`

下方 R2 内容仅作为已被取代的历史证据保留，不再代表当前有效完成状态。

- Implementation SHA: `3bc6a81b8bcb61307698ece3b80f118fdb2c64df`
- Implementation Tree SHA: `7faecfe1cbf7b24a7da04959c3fa4fff6c83dcf4`
- Evidence SHA: `SELF`
- Contract Manifest Hash: `39d59e79b941039e1cbfb1e614169c2dc4798ce3`

本报告由 `nexa-release-audit generate milestone4r2` 从结构化 Gate JSON 确定性生成；Evidence commit 使用 `SELF`，并由 `verify-evidence milestone4r2` 校验父提交和允许路径。

## 24 个工作包 Commit

| WP | Commit |
|---:|---|
| 1 | `9f49ad391022dbc711227c87756c7de0f0107077` |
| 2 | `9f49ad391022dbc711227c87756c7de0f0107077` |
| 3 | `9e7d371de32ec65e8e22de731c723d6a78d19b42` |
| 4 | `9e7d371de32ec65e8e22de731c723d6a78d19b42` |
| 5 | `9e7d371de32ec65e8e22de731c723d6a78d19b42` |
| 6 | `9e7d371de32ec65e8e22de731c723d6a78d19b42` |
| 7 | `9e7d371de32ec65e8e22de731c723d6a78d19b42` |
| 8 | `9e7d371de32ec65e8e22de731c723d6a78d19b42` |
| 9 | `9e7d371de32ec65e8e22de731c723d6a78d19b42` |
| 10 | `9e7d371de32ec65e8e22de731c723d6a78d19b42` |
| 11 | `55298f7ce4fd89696be45c60e878d4e63893d7e6` |
| 12 | `55298f7ce4fd89696be45c60e878d4e63893d7e6` |
| 13 | `55298f7ce4fd89696be45c60e878d4e63893d7e6` |
| 14 | `55298f7ce4fd89696be45c60e878d4e63893d7e6` |
| 15 | `55298f7ce4fd89696be45c60e878d4e63893d7e6` |
| 16 | `55298f7ce4fd89696be45c60e878d4e63893d7e6` |
| 17 | `9665afb198ab4d453436313690c30e0c925486b5` |
| 18 | `9665afb198ab4d453436313690c30e0c925486b5` |
| 19 | `9665afb198ab4d453436313690c30e0c925486b5` |
| 20 | `9665afb198ab4d453436313690c30e0c925486b5` |
| 21 | `9665afb198ab4d453436313690c30e0c925486b5` |
| 22 | `788e7a1c4e94391d1693aa73ad0b95063d0d9e68` |
| 23 | `788e7a1c4e94391d1693aa73ad0b95063d0d9e68` |
| 24 | `788e7a1c4e94391d1693aa73ad0b95063d0d9e68` |

## Contract 结果

| Contract | Gate | Status |
|---|---|---|
| M4R2-STATUS-001 | workspace-gates | passed |
| M4R2-CONTRACT-001 | workspace-gates | passed |
| M4R2-HOST-001 | host-returns | passed |
| M4R2-HOST-002 | host-returns | passed |
| M4R2-HOST-003 | host-returns | passed |
| M4R2-HOST-004 | host-returns | passed |
| M4R2-HOST-005 | host-returns | passed |
| M4R2-ALLOC-001 | host-allocations | passed |
| M4R2-ALLOC-002 | host-allocations | passed |
| M4R2-ALLOC-003 | host-returns | passed |
| M4R2-SPAN-001 | diagnostic-spans | passed |
| M4R2-SPAN-002 | diagnostic-spans | passed |
| M4R2-SPAN-003 | diagnostic-spans | passed |
| M4R2-SPAN-004 | diagnostic-spans | passed |
| M4R2-SPAN-005 | diagnostic-spans | passed |
| M4R2-SPAN-006 | diagnostic-spans | passed |
| M4R2-CORPUS-001 | diagnostic-corpus | passed |
| M4R2-CORPUS-002 | diagnostic-corpus | passed |
| M4R2-CORPUS-003 | diagnostic-corpus | passed |
| M4R2-CORPUS-004 | diagnostic-corpus | passed |
| M4R2-CORPUS-005 | diagnostic-corpus | passed |
| M4R2-AUDIT-001 | workspace-gates | passed |
| M4R2-AUDIT-002 | workspace-gates | passed |
| M4R2-EVIDENCE-001 | evidence-chain | passed |

## Host 非空返回逐项分配

| Case | Total | Host | Thunk |
|---|---:|---:|---:|
| input_string | 0 | 0 | 0 |
| input_struct | 0 | 0 | 0 |
| input_enum | 0 | 0 | 0 |
| input_option | 0 | 0 | 0 |
| input_result | 0 | 0 | 0 |
| input_array_struct | 0 | 0 | 0 |
| input_buffer_struct | 0 | 0 | 0 |
| input_nested | 0 | 0 | 0 |
| input_mixed_eight | 0 | 0 | 0 |
| input_scalar_collections | 0 | 0 | 0 |
| return_string | 1 | 1 | 0 |
| return_struct | 1 | 1 | 0 |
| return_enum | 1 | 1 | 0 |
| return_option | 1 | 1 | 0 |
| return_result | 1 | 1 | 0 |
| return_array | 1 | 1 | 0 |
| return_buffer | 1 | 1 | 0 |
| return_array_struct | 3 | 3 | 0 |
| return_buffer_struct | 3 | 3 | 0 |
| return_nested_enum | 1 | 1 | 0 |
| return_option_array | 2 | 2 | 0 |
| return_result_buffer | 2 | 2 | 0 |
| return_large_array | 1 | 1 | 0 |
| return_large_buffer | 1 | 1 | 0 |
| return_nested | 5 | 5 | 0 |
| error_wrong_struct_type | 0 | 0 | 0 |
| error_wrong_enum_tag | 0 | 0 | 0 |
| error_wrong_payload_type | 0 | 0 | 0 |
| error_heap_object_capacity | 0 | 0 | 0 |
| error_collection_capacity | 0 | 0 | 0 |
| error_string_capacity | 0 | 0 | 0 |
| error_host_panic | 2 | 2 | 0 |
| injected_object_reservation | 1 | 1 | 0 |
| injected_collection_reservation | 1 | 1 | 0 |
| injected_string_reservation | 1 | 1 | 0 |
| injected_struct_write | 1 | 1 | 0 |
| injected_collection_write | 1 | 1 | 0 |
| injected_commit | 1 | 1 | 0 |

## 34 个 Diagnostic 实际发射

| Code | Pipeline | Category | Primary slice |
|---|---|---|---|
| NX1001 | compiler | diagnostic | # |
| NX1002 | compiler | diagnostic | ) |
| NX2001 | compiler | diagnostic | missing |
| NX2002 | compiler | diagnostic | Missing |
| NX2101 | compiler | diagnostic | true |
| NX2201 | compiler | diagnostic | match value { A => 1 } |
| NX2202 | compiler | diagnostic | A => 2 |
| NX2210 | compiler | diagnostic | None |
| NX2220 | compiler | diagnostic | ? |
| NX2221 | compiler | diagnostic | ? |
| NX2301 | compiler | diagnostic | await |
| NX2302 | compiler | diagnostic | work() |
| NX2401 | compiler | diagnostic | value |
| NX2501 | compiler | diagnostic | record.missing |
| NX2601 | compiler | diagnostic | old.get<i32>(legacy) |
| NX2602 | compiler | diagnostic | { return true; } |
| NX2603 | compiler | diagnostic | { old.get<i32>(legacy); finish_migration(); return true; } |
| NX2604 | compiler | diagnostic | preserve(legacy) |
| NX3001 | bytecode_decode | decode |  |
| NX3002 | verifier | verify |  |
| NX3003 | verifier | verify |  |
| NX3004 | bytecode_decode | decode |  |
| NX4001 | runtime | host |  |
| NX4002 | runtime | host |  |
| NX4003 | host | host |  |
| NX5001 | host | host |  |
| NX5002 | host | host |  |
| NX5003 | host | host |  |
| NX5004 | runtime | host |  |
| NX6001 | migration | migration |  |
| NX6002 | migration | migration |  |
| NX6003 | reload | reload |  |
| NX6004 | reload | reload |  |
| NX6005 | verifier | verify |  |

## 关键机器指标

- Realm v5 worlds: 592
- RealmRuntime shortest paths: 592
- Production failure points: 15
- Host-return failure points: 6
- Source-backed inexact spans: 0
- Typed Snapshot storage: `Arc<[u8]>`
- All contracts passed: true

## 已知范围内缺口

无。

Milestone 4.0R2 = **COMPLETE**。
