# Module Machine Specification 1.0

Version: **1.0.0**

```text
Prepared → Quiescing → Staged → Activating → Active
                                      └────→ ActivationFaulted
Active → Faulted
```

Commit occurs between `Staged` and `Activating`. Failures before commit keep the old root and state,
but old tasks have already been cancelled by Restart Reload and are never restored. Failures during
post-commit activation do not restore old tasks; the new module enters `ActivationFaulted` and
accepts only reload, reset, diagnostics, or destruction.

Old-root cleanup is an internal, bounded drain within the same Restart Reload operation:

```text
Pending → Draining → Completed
                   └→ Failed
```

It is not a public epoch-routing API and cannot run old business tasks.
