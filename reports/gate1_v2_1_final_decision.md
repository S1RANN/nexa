# Gate 1 v2.1 Final Decision

Gate 1 v2.1: **INVALID**

Milestone 5.0R1: **INCOMPLETE**

The only authorized Formal Run 1 passed Preflight, then process provenance initialization returned
`PermissionDenied` / os code 1 / `Operation not permitted` before the top-level Worker started.
Zero retries were used. Formal Run 2 and Replay were not started, no Receipt was created, and
`origin/main` was not changed.

The complete original failure evidence remains on local branch `codex/gate1-v2.1` at:

- I2.1: `3b795b315b5556ca965e75a4f5dbddc31341c0a6`
- E2.1: `8e2296da6f9ca85a51fd164eb9f3f0c89849a499`

Superseded by: **gate1-v2.2**
