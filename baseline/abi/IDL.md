# Nexa IDL MVR 1.0

The MVR uses an IDL-first, exact-build Rust host interface.

Supported forms:

- scalars and strings
- opaque host handles
- fixed-layout structs
- `Result`
- copy buffers and immutable snapshots
- asynchronous host requests
- registered host-resource tokens
- immediate-host whitelist metadata

The normalized schema produces an exact interface hash. A hash mismatch rejects module loading and
requires binding regeneration and rebuild. Compatible adapters and independent release windows are
not part of the MVR.

Asynchronous functions use:

```text
request(return_error|cancel_task, return_error|trap) fn name(...)
    -> request<Result<Success, Error>>;
```

Both terminal policies and both Result payload types participate in the exact hash. Generated Rust
bindings include IDL enums and a typed completion-ticket wrapper.
