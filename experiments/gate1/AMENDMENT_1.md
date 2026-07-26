# Gate 1 Amendment 1 — Full timed sample count

Review outcome: **approved apparatus correction before Evidence commit**.

The first formal invocation on Implementation `c6ac195` was INVALID because the inherited
benchmark tool capped the migration, reload-commit, and realm-drop cases at 200 timed samples even
when Gate 1 requested 1,000. No acceptance threshold, hypothesis input, language/runtime behavior,
or observed result was changed.

The one permitted amendment removes those three legacy caps, requires every benchmark case to
report exactly 1,000 timed samples, and aligns the runner with the frozen rule that stable hard
semantics plus excessive timing noise is `INCONCLUSIVE`, never `FAIL`. It also adds the
already-normative reconciliation branch for two independent replays that are
semantically/allocation-identical but timing-inconclusive. That branch must terminate at
`UNVERIFIABLE_WITHIN_MVR`; it cannot trigger a third replay. Runner/tool hashes are refrozen and
both formal runs and independent replay restart in fresh processes/directories. All attempts
produced by the contradictory runner are invalid apparatus evidence and are not eligible for the
decision.
