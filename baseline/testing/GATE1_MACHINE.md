# Gate 1 Experiment Machine 1.0

The source of truth is `specs/machines/gate1.machine.spec`; generated Rust guards are checked with
`nexa-machine -- check-generated`.

```text
Draft → CriteriaFrozen → InputsFrozen → Running → InitialResult
InitialResult → ValidityReview → IndependentReplay → DecisionReview
DecisionReview → Passed | Failed | Invalid | Inconclusive | UnverifiableWithinMvr | Stopped
```

At `DecisionReview`, an invalid first run may return to `InputsFrozen` only after a recorded
amendment review. An inconclusive first result may return to `Running` only for the one permitted
retest. Once recorded as terminal `Invalid` or `Inconclusive` it cannot silently leave that state;
a second inconclusive result terminates at `UnverifiableWithinMvr`.

Acceptance criteria and frozen inputs cannot be replaced after `Running`. `Failed` cannot return
to `Running`, and `Invalid` cannot be rerun without review. All terminal decisions are explicit.
