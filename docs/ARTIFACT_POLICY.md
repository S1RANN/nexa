# Artifact Policy

Generated runtime, benchmark, audit, fuzz, and diagnostic outputs belong under
`target/nexa-artifacts/`. Raw JSON, NDJSON, benchmark samples, logs, build
products, and temporary worktrees are not tracked by Git.

Tracked fixtures must protect a current product behavior, be reviewed, be
deterministic, and remain at or below 512 KiB. Historical experiment evidence
is preserved by immutable Git tags rather than copied into the active tree.

The only historical Gate 1 entry point on the active branch is
[`history/GATE1_V2_9_STOP.md`](history/GATE1_V2_9_STOP.md).

Finalization audit output is generated locally below milestone-specific
directories such as `target/nexa-artifacts/m1-finalize/`,
`target/nexa-artifacts/m4-finalize/`, and
`target/nexa-artifacts/m4r1-finalize/`. Each finalizer writes
`final-report.json`; supporting inventories and gate reports live under the
same artifact root. These reports are reproducible evidence and must not be
committed.

`cargo xtask finalize-m1` regenerates the final report only after running the
complete local check, verifying the annotated historical tag and target,
reading the 20-case Business Host mutation report, recording the current HEAD,
and confirming a clean working tree. A generated report never changes the Git
tree.

`cargo xtask finalize-m4-r1` follows the same rule after the Language v2,
Object Model v2, Async v2, NIDL v2, structured Codegen, Standalone, REPL,
multiple-entrypoint, scale, and repository gates. Its completion report is
`target/nexa-artifacts/m4r1-finalize/final-report.json`.
