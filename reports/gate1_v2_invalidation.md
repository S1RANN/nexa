# Gate 1 v2 Invalidation

Gate 1 v2 is **INVALID_APPARATUS** and **NOT_AUTHORIZED_FOR_DECISION**.

Its newly normative acceptance and authorization files lacked the Version declaration required by
`baseline check`. Correcting those inputs changes the frozen hashes after the v2 execution budget
was exhausted. Therefore its two formal runs, replay, evidence, decision, and Receipt cannot be
reused or pushed as current Gate 1 evidence.

The invalid history is preserved locally at:

- Implementation: `fe8046e5e45135d87900cfb7cd18c468b7604401`
- Evidence: `b6d49c0b4f7dd283dc0a04e6f1c1950e3c40bb4d`
- Receipt: `b63e542c0bd5564704e0c2fda0c551376f60623f`

Superseded by: **gate1-v2.1**
