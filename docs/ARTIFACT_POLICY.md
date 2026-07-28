# Artifact Policy

Generated runtime, benchmark, audit, fuzz, and diagnostic outputs belong under
`target/nexa-artifacts/`. Raw JSON, NDJSON, benchmark samples, logs, build
products, and temporary worktrees are not tracked by Git.

Tracked fixtures must protect a current product behavior, be reviewed, be
deterministic, and remain at or below 512 KiB. Historical experiment evidence
is preserved by immutable Git tags rather than copied into the active tree.

The only historical Gate 1 entry point on the active branch is
[`history/GATE1_V2_9_STOP.md`](history/GATE1_V2_9_STOP.md).

Finalization audit output is generated locally at
`target/nexa-artifacts/m1-finalize/inventory.json` and
`target/nexa-artifacts/m1-finalize/final-report.json`. These reports are
reproducible evidence and must not be committed.

`cargo xtask finalize-m1` regenerates the final report only after running the
complete local check, verifying the annotated historical tag and target,
reading the 20-case Business Host mutation report, recording the current HEAD,
and confirming a clean working tree. A generated report never changes the Git
tree.
