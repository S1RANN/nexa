# Pilot Error and Diagnostic Manual

| Code/family | Meaning | Operator action |
|---|---|---|
| NX4001 | Exact host interface hash mismatch | Regenerate bindings and rebuild host/module |
| NX4002 | Host ABI type/shape mismatch | Fix generated host implementation or module |
| NX4003 | Host argument mismatch | Inspect call site and exact IDL |
| NX5001 | Host panic or result mismatch | Quarantine call, collect host/runtime artifacts |
| NX5002 | Async ticket was abandoned | Apply declared abandon policy |
| NX5003 | Unknown typed host error | Fix host error enum/code mapping |
| NX5004 | Runtime capacity exhausted | Stop admission; adjust measured capacity safely |
| NX6001 | Migration limit exceeded | Roll back pre-commit; inspect dry-run consumption |
| NX6002 | Invalid migration graph | Fix preserve/replace/delete/forwarding graph |
| NX6003 | Activation failed after commit | Candidate remains published/faulted; do not roll back |

Diagnostic JSON follows `pilot/diagnostic.schema.json`. Preserve the full structured error,
source span, task/module/request identity, runtime snapshot, and exact build hashes.
