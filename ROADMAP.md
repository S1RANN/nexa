# Nexa Roadmap

This is the only global roadmap for Nexa. Normative meaning remains in `baseline/`; this document
only describes stage navigation and decisions.

## Project position

Nexa is an experimental, exact-build typed scripting language and controlled runtime for embedded
gameplay logic. The intended product direction is an engine-integrated runtime with generated host
bindings, bounded resumable tasks, explicit host resources, typed snapshots, and transactional
single-module stateful reload.

```text
Gate -1
→ Gate 0
→ MVR Implementation
→ Milestone 4.0R3
→ Gate 1 v1 INVALID_APPARATUS
→ Gate 1 v2 INVALID_APPARATUS
→ Gate 1 v2.1 INVALID
→ Gate 1 v2.2 NOT TRUSTWORTHY
→ Gate 1 v2.3 SEMANTICALLY_INSUFFICIENT
→ Gate 1 v2.4 STRUCTURAL_CLOSURE_FAILED
→ Gate 1 v2.5
→ Gate 1 v2.5 Decision
→ Pilot / Gate 2 RFC / Pivot / Stop
```

Current implementation milestone: **4.0R3 complete**.

<!-- gate1-v2.5-status:start -->
Current project gate: **Gate 1 v2.5 frozen / incomplete**.

Gate 1 v2.4 is **STRUCTURAL_CLOSURE_FAILED** and not decision-usable.

Current Gate 1 v2.5 decision: **PENDING_FORMAL_EXECUTION**.

Current milestone: **5.0R5 — INCOMPLETE**.
<!-- gate1-v2.5-status:end -->

## Gates

| Gate | Entry condition | Exit condition | Status |
|---|---|---|---|
| Gate -1 | A falsifiable product thesis and hard budget exist | MVR hypotheses, scope, and stop rules are written | Complete |
| Gate 0 | MVR scope and experiment protocol are normative | H1a/H2a/H3a implementation paths are executable | Complete |
| MVR Implementation | Gate 0 baseline is frozen | Exact-build IDL, Fast Task, and single-module reload close end to end | Complete |
| Milestone 4.0R3 | Runtime is executable | Diagnostics and evidence provenance are independently verifiable | Complete |
| Gate 1 v1 | 4.0R3 complete | Historical apparatus review | Invalid apparatus |
| Gate 1 v2 | v1 apparatus invalidated | Historical baseline-integrity review | Invalid apparatus |
| Gate 1 v2.1 | Qualified provenance unavailable | Historical terminal validity review | Invalid |
| Gate 1 v2.2 | Qualified technical runs | Historical governance review | Not trustworthy |
| Gate 1 v2.3 | Qualified environment and unchanged thresholds | Historical semantic review | Semantically insufficient |
| Gate 1 v2.4 | Three formal executions completed | Structural closure and Receipt are valid | Structural closure failed |
| Gate 1 v2.5 | Stable projection and finalization apparatus is prefreeze-complete | Two formal runs and replay preserve real outcomes | Frozen |
| Gate 1 v2.5 Decision | Valid v2.5 evidence is available | One legal terminal decision and verified F2.5 are recorded | Pending |
| Pilot | Gate 1 permits Pilot and a team commits | Pilot exit review accepts, pivots, or stops | Conditional |
| Gate 2 RFC | H1/H2 pass, H3 is acceptable, Pilot commits, budget is approved | RFC decision only; no implementation is implied | Conditional |

## Decision and budget boundaries

The legal Gate 1 decisions are `PROCEED_TO_PILOT`, `PROCEED_TO_GATE2_RFC`, `PIVOT`, `HOLD`,
`STOP`, and `UNVERIFIABLE_WITHIN_MVR`. With no committed Pilot team, this repository may only
choose `HOLD`, `PIVOT`, or `STOP`.

STOP or PIVOT review is mandatory when a hypothesis fails because of a core design defect, the
experiment is unverifiable after its one permitted retest, or the hard Gate -1 budget is exhausted.
The budget remains fourteen calendar months or six person-years through Gate 1. Gate 2 requires a
separately approved budget.

Pilot admission requires valid H1/H2/H3 evidence, independent replay, a named committed team, a
supported host environment, capacity configuration, rollback artifacts, and acceptance of the MVR
limits. Gate 2 additionally requires H1a and H2a PASS, H3a PASS or single-module-only
unverifiability, no failed hypothesis, a Pilot commitment, and approved Gate 2 budget.

## Deferred capabilities and non-goals

Dynamic Value, interfaces, user-defined generics, cross-module StateHandle, reload groups,
compatible ABI adapters, strict deterministic scheduling, an untrusted-bytecode sandbox, AOT/JIT,
LSP/DAP, a package manager, online seamless hot patching, new language data types, and new gameplay
features are deferred. They are Gate 2-or-later candidates, not promised deliverables.

The MVR is not a general-purpose language, a stable cross-release ABI, a multi-module transactional
runtime, or a security boundary for hostile bytecode.

## Document priority

Conflicts are resolved by `baseline/BASELINE_INDEX.md`: MVR Scope, Gate 1 Acceptance, current
architecture baseline, this roadmap, then historical rationale. The roadmap never overrides
normative semantics.
