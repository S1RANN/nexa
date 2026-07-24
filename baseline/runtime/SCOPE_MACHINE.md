# Scope Machine Specification 1.0

Scopes are structured-concurrency owners.

```text
Active → CancelRequested → Cancelling → Cancelled → Destroyed
```

A fast task increments the owner's transient-child count before executing. First-slice completion
removes it; promotion atomically turns it into a persistent child. Once cancellation is requested,
new child admission fails and existing children observe cancellation at their next safepoint.
