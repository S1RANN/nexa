# Package Policies

Policy belongs to the Host, not `package.toml`.

`PackagePolicy` fixes:

- `FirstParty` or `Trusted` trust;
- capability ceiling;
- allowed activation policies;
- maximum discovered packages;
- handler fuel, cumulative budget, heap, Host resource, Task, and release
  ceilings;
- whether entitlement declarations are legal.

`Trusted` means local developer content or content that has been manually
reviewed. It does not claim an adversarial-code sandbox.

Manifest capabilities must be a subset of the source ceiling. Runtime limits
may be lower than policy but never higher. A violation rejects the manifest;
the embed layer does not silently remove capabilities or clamp limits.

Activation meanings:

- `required`: enabled by default and rejects ordinary disable;
- `default-enabled`: initially enabled unless the user persisted disable;
- `user-controlled`: executes only after explicit/persisted enable;
- `programmatic`: reserved for host-managed activation.

If a source allows entitlements, a manifest may name one stable entitlement.
Missing ownership yields `Locked`, not `Faulted`. Entitlement refresh disables
and cleans a package before locking it. Unlock produces `Disabled`; the Host
must explicitly enable it.

Snake maps its three directories in the domain layer:

| Source | Trust | Activation | Entitlement |
|---|---|---|---|
| Built-in | FirstParty | Required / DefaultEnabled | No |
| Official DLC | FirstParty | UserControlled | Yes |
| Trusted local Mod | Trusted | UserControlled | No |

These categories do not appear in `nexa-embed` public API.
