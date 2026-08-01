# Nexa Async v2

Version: **2.0.0**

Status: **COMPLETE**

This document is the normative source-language contract for asynchronous Nexa
code in Language v2. It defines the target M4R1 surface. Runtime requests, task
frames, resume points, schedulers, and verifier metadata remain implementation
concepts and are not source-language types.

There is no source compatibility with the Language v1 spellings `task fn` or
prefix `await`.

## Source surface

An asynchronous function uses `async fn`:

```nexa
async fn load_profile(
    id: string,
) -> Result<Profile, LoadError> {
    return host::profile::load(id).await;
}
```

`pub` and function attributes, when otherwise legal, may be applied to an
asynchronous function:

```nexa
pub async fn refresh() -> Result<Profile, LoadError> {
    return load_profile("active").await;
}
```

The declared return type is the value produced after the function completes.
It is not a `Future`, `Task`, or `Request` type. Calling an `async fn` or an
asynchronous Host function produces an unnameable compiler temporary that must
be consumed by postfix `.await`.

The only source spelling of await is:

```nexa
expression.await
```

`.await` has no parentheses, cannot take arguments, and cannot be overloaded.

## Postfix parsing

Call, member access, index, `.await`, and `?` form one left-to-right postfix
chain. Each postfix operation consumes the complete expression to its left.
Consequently, the required parses are:

```text
load().await
    = await(call(load))

load().await?
    = try(await(call(load)))

load().await?.field
    = field(try(await(call(load))), "field")

client.connect().await?.fetch().await?
    = try(await(call(fetch(field(
          try(await(call(connect(field(client, "connect"))))),
          "fetch"
      )))))

items().await?[0]
    = index(try(await(call(items))), 0)
```

Parentheses may group an expression but do not create a storable asynchronous
value. Prefix `await expression`, `await(expression)`, and `.await()` are syntax
errors.

## Static semantics

`.await` is legal only when all of the following hold:

1. The enclosing function is declared `async fn`, or the compiler is lowering
   a script or REPL Cell to a synthetic asynchronous entrypoint.
2. Its receiver is the immediate result of a compiler-known asynchronous Nexa
   call or asynchronous Host call, allowing only transparent parentheses
   around that call.
3. The asynchronous result has not crossed a statement, binding, argument,
   return, field, collection, branch join, or other storage boundary.

Valid consumption includes:

```nexa
let profile = load_profile(id).await?;
return host::profile::load(id).await;
let first = items().await?[0];
```

The following are invalid:

```nexa
fn sync_load() -> Profile {
    return load_profile("active").await;
}

async fn missing_await() -> Result<Profile, LoadError> {
    let profile = load_profile("active");
    return profile;
}

async fn split_await() -> Result<Profile, LoadError> {
    let pending = load_profile("active");
    return pending.await;
}
```

There is no implicit await. Every asynchronous call result must be consumed in
the same expression by `.await`; merely assigning, returning, passing, or
discarding that result is an error.

Language v2 does not define or predeclare any of these source types:

```text
Future<T>
Awaitable<T>
Task<T>
Request<T>
Pin<T>
Poll<T>
Waker
```

A package cannot name the compiler's pending-result temporary in a signature,
field, local annotation, generic argument, or public API. This also prevents an
asynchronous operation from being detached or stored for later polling.

The type of `call.await` is the declared return type of the asynchronous
callee. In particular, an asynchronous Host declaration returning
`Result<S, E>` produces `Result<S, E>` after `.await`; it never degrades to
`unit` and never exposes a Host Request type.

## `yield`

`yield;` is an explicit cooperative suspension statement. It is legal only in
an `async fn` or a synthetic asynchronous script/REPL entrypoint. It does not
produce a value and is not a generator operation.

Using `yield` in an ordinary function is an analysis error. A yielded task
resumes at the following statement under the same task, scope, frame, fuel,
resource, cancellation, and reload rules as any other asynchronous suspension.

## `defer`

A `defer` body is synchronous cleanup. The following are forbidden anywhere in
the deferred expression or block, including through a directly reachable
callee:

```text
.await
yield
an asynchronous Host call
```

The analyzer rejects the deferred operation before bytecode generation.
Ordinary task cancellation runs accepted `defer` actions in last-in-first-out
order under bounded cleanup fuel and cleanup-operation limits. Restart Reload
cancellation releases VM and registered Host resources but does not execute
user `defer`. A cleanup trap terminates the task as trapped; successful
ordinary cancellation terminates it as cancelled.

## Lowering and runtime contract

`async fn` lowers to the existing verified task machinery:

```text
async source function
→ Task effect
→ Task frame and exact root maps
→ resume points at `.await` and `yield`
→ verified bytecode
→ bounded Runtime task
```

The surface reset must not change the observable semantics of:

- waiting for and resuming from a Host result;
- explicit yield and fuel yield;
- normal completion and returned values;
- ordinary cancellation and bounded cleanup;
- traps, including cleanup traps;
- late Host-result rejection after cancellation or reload;
- Restart Reload cancellation and old-epoch rejection;
- exactly-once Request, Resource Token, snapshot, continuation, and scheduler
  release.

An asynchronous call is not implicitly spawned in the background. Evaluation
is structured by the enclosing task, and a source program cannot obtain a
request handle or manually poll a pending operation.

All task growth is subject to the finite limits resolved for the build and
Realm: fuel slice, cumulative fuel, frame and call depth, heap objects, Host
resources, child tasks, cleanup operations, and cleanup fuel. Capacity that can
fail must be checked before an externally visible effect or allocation.
Exhausting a renewable fuel slice yields; exhausting a cumulative or structural
limit produces the corresponding deterministic runtime failure. Limits are
never silently raised by `.await`.

## Required diagnostics

The implementation must produce one primary diagnostic at the most specific
source span for each of these classes:

| Condition | Required diagnostic behavior |
|---|---|
| `.await` outside an asynchronous context | Point at `.await`; state that the enclosing function must be `async` |
| asynchronous call without `.await` | Point at the call; require postfix `.await` |
| `.await` on a synchronous value | Point at `.await`; identify the receiver as non-asynchronous |
| pending result stored or split across expressions | Point at the escape site; require same-expression consumption |
| `yield` outside an asynchronous context | Point at `yield`; require `async fn` |
| asynchronous effect in `defer` | Point at the first forbidden operation and identify the enclosing `defer` |
| legacy `task fn` or prefix `await` | Report unsupported Language v1 syntax; do not reinterpret it |
| attempted public pending/Future type | Report an unknown or forbidden type; do not synthesize a compatibility type |

Diagnostics must preserve the original URI and UTF-8 source span. Recovery may
continue to find later errors, but it must not manufacture Typed IR or bytecode
that treats an invalid asynchronous operation as synchronous.

## Conformance

Conformance requires positive and negative syntax/type tests for every rule
above and differential task tests covering wait, resume, cancellation, fuel,
trap, cleanup, and reload. The differential oracle compares runtime-observable
task behavior, not acceptance of the removed syntax. `task fn` and prefix
`await` must remain rejected.
