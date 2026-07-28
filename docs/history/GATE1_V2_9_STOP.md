# Gate 1 v2.9 STOP

Gate 1 v2.9 is the final decision for Nexa's former general-product MVR.
The verified product decision is **STOP**: H1, H2, and H3 all produced stable
`FAIL` outcomes. This closes that product route; it does not close the Nexa
Internal Language Pivot.

The full experiment apparatus, raw evidence, receipts, and finalization files
remain recoverable from the immutable annotated tag `gate1-v2.9-stop`. They are
deliberately absent from the active product branch.

## Frozen chain

| Stage | Commit | Tree |
|---|---|---|
| I2.9 | `d3d66810ea6f6a7ef36f104f2e74a7588736e077` | `e22824f3e812abbd983ec345094589decabb7150` |
| E2.9 | `c1b4a42e5b1b0ff602e5677d7fa8debcb2049d6f` | `16ba70436b1213aa99ec20d69ac7fe3d1ddc7d97` |
| D2.9 | `a05b897999a5c8d408d5d9d1f018295652fec5dd` | `660d5cacbf2bc0d0566d5fd57ddd8554b61049a7` |
| R2.9 | `c251d62ad09ee47b59baf2c0bc73829fc2c76671` | `18bbd828d0f763a363aa1d5fa367c17c65b522c0` |
| F2.9 | `8552064ec01b3191467633717de7b77c97cb24f1` | `7babb34ab785c156a752171335e36ce7ff19b86d` |

The terminal result recorded 50/50 satisfied contracts, no known structural
gaps, a verified receipt, and stable failures for H1/H2/H3. Formal and replay
comparisons were `INCONCLUSIVE`; the per-run product outcomes were nevertheless
stable failures.

## Known harness issue

The post-finalization `verify-final` command in the old v2.9 tool assumes that
the finalization files still contain the v2.8 status row before applying its
replacement. At F2.9 those files already contain the v2.9 row, so invoking the
command from the finalized tree can fail even though the eight finalization
file hashes match the receipt. The immutable commit and tree chain above is the
authoritative historical record.

## Checkout

```sh
git fetch --tags origin
git show gate1-v2.9-stop
git switch --detach gate1-v2.9-stop
```

Return to current product work with:

```sh
git switch codex/internal-pivot-m1
```
