# Gate 1 v2.7 Terminal Record

Gate 1 v2.7 is `INVALID_ENVIRONMENT_EXECUTION / INCOMPLETE` and is not decision-usable.
Formal Run 1 was invoked without the unrestricted subprocess access required by the qualified
environment. H1 therefore could not create its temporary Git worktree and the supervisor correctly
returned `INVALID`. Formal Run 2 and Replay were not started. The failed run is not overwritten,
retried, packaged, or used for a product decision.

The v2.7 Raw-to-Gate pointer repair remains preserved in I2.7, but v2.7 itself cannot close Gate 1.
It is superseded by v2.8.
