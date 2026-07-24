# Module Machine Specification 1.0

```text
Prepared → Quiescing → Staged → Activating → Active
                                      └────→ ActivationFaulted
Active → Faulted
```

Commit occurs between `Staged` and `Activating`. Failures before commit restore the old task and
completion-delivery state. Failures during post-commit activation do not restore old tasks; the
new module enters `ActivationFaulted` and accepts only reload, reset, diagnostics, or destruction.

Retired epoch cleanup is tracked independently:

```text
Pending → Draining → Completed
                   └→ Failed
```

A retired cleanup failure does not fault the newly active module.
