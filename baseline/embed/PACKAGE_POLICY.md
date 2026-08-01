# Package Policy

Version: **1.0.0**

Status: M2 COMPLETE

Source policy is host-owned and cannot be declared by a package. It fixes
`TrustLevel`, capability ceiling, allowed activation policies, package count,
runtime ceilings, and whether an entitlement may be requested.

M2 trust is either `FirstParty` or `Trusted`. `Trusted` means local,
developer-provided, or manually reviewed content; it is not a hostile-code
security boundary. There is intentionally no `Untrusted` variant.

Activation is `Required`, `DefaultEnabled`, `UserControlled`, or
`Programmatic`. A manifest activation outside its source set is rejected.
Required packages reject ordinary disable.

Capabilities are validated IDs and iterate in stable order. A manifest request
must be a subset of the source ceiling. Runtime limits for handler fuel,
cumulative budget, heap objects, host resources, tasks, and release records
must be at or below source ceilings. Neither capabilities nor limits are
silently clipped.

An entitlement request is legal only when the source permits it. A legal but
unowned entitlement produces `Locked`, not `Faulted`. Refreshing entitlements
locks an enabled package through the normal disable path and unlocks it to
`Disabled`; it never implicitly executes newly unlocked code.
