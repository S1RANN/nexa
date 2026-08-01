# Executable Module v1

Version: **1.0.0**

The formal load path is:

```text
Verified Bytecode -> ExecutableModuleBuilder -> ExecutableModule -> Interpreter
```

Portable bytecode remains the cache and safety boundary; runtime pointers and
dense slots are never serialized to disk.

## Predecoded instructions

Each executable instruction carries a compact opcode, physical operands,
resolved layout, static fuel, a safepoint flag, a root map index, and a cold
metadata index. Static fuel is read directly for most instructions; only
data-dependent operations (string, collection, map, buffer copy) compute a
dynamic surcharge. Safepoint flags are fixed at build time, not recomputed
per instruction.

## Dense resolution

Module load resolves stable identities to dense slots or offsets: functions,
struct fields, class fields, enum variants, state fields, host functions,
and script exports. Formal profiles must show no BTreeMap lookups, linear
stable-ID searches, string comparisons, or dynamic allocation on ordinary
arithmetic, field access, call, or host-call instructions.

## Hot/cold separation

Hot data: opcode, operands, layout, fuel, safepoint. Cold data: source maps,
display names, diagnostic descriptions, related locations, full debug
metadata. Cold data is reached through the cold metadata index only on trap,
diagnostic, or inspection paths.

## Self-validation

Builder output validates dense slot completeness, physical range legality,
cold metadata mapping, host/export plan consistency, and root map
consistency before the module becomes executable.

## Call frames and execution paths

Call and return do not allocate, do not clear unrelated slots, initialize
only declared ranges, copy parameters directly into the target range, and
write results into the caller range. Synchronous and Task execution share
one semantic core with distinct fast paths (immediate/ordinary fast poll,
Task poll, migration/cleanup restricted paths); synchronous execution does
not check request or migration state that cannot apply to it.

## Differential gate

The portable reference interpreter and the ExecutableModule interpreter run
the full corpus plus randomized bytecode; results, traps, fuel (same
cost-table version), task lifecycles, and identities must match item by
item.
