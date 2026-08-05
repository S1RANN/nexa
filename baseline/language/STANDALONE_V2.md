# Nexa Standalone Profile v1

Version: **1.0.0**

Status: **COMPLETE**

This document defines the Language v2 Standalone execution profile used by
`nexa run`. It covers Package entrypoints and the single-file script lowering.
`nexa exec` is the separate bytecode-debugging interface.

## Built-in Host contract

Every Standalone run binds this synchronous Host surface:

```nidl
contract Console {
    host {
        fn write(value: string);
        fn write_line(value: string);
        fn write_error(value: string);
        fn write_error_line(value: string);
    }
}
```

Nexa imports it as:

```nexa
use host::console;
```

`write` and `write_line` target standard output. `write_error` and
`write_error_line` target standard error. The `*_line` functions append one
line-feed after the supplied string; the other functions append nothing. They
return `unit`.

The default Standalone Host provides no filesystem, network, subprocess,
environment mutation, wall-clock, or system-random capability. A program that
requires another Host contract is rejected unless a future profile explicitly
defines and binds it; Standalone v1 never silently grants it.

## Package `main` ABI

A Package run has exactly one entry module selected by the canonical Package
build input. That module must contain exactly one declaration with one of these
two signatures:

```nexa
fn main(args: Array<string>) -> i32

async fn main(args: Array<string>) -> i32
```

The parameter name, parameter type, arity, return type, and function name are
part of the ABI. Default parameters, overloads, additional parameters, a
missing return type, a different integer width, and any other signature are
rejected. `main` is an entrypoint, not an indexed export, and normal run
commands never ask the user for its bytecode function index.

The synchronous form executes as an ordinary verified function. The
asynchronous form executes as a bounded task to completion, including Host
waits, yields, cancellation, fuel, trap, cleanup, and reload rules from
`ASYNC_V2.md`.

A missing `main`, multiple `main` declarations in the entry module, or a
second Package module attempting to define the Package entrypoint is a compile
error.

## Package source restrictions

Executable top-level statements are forbidden in every normal Package module.
Ignoring comments and attributes attached to declarations, the only file-level
forms are:

```text
use
const
struct
enum
class
fn
```

`let`, assignment, calls, control flow, `return`, `yield`, `defer`, and other
executable statements must occur inside a function. This restriction applies
to all Package modules, including the entry module.

## Single-file scripts

The file form:

```bash
nexa hello.nexa Alice
```

uses the Script compilation profile. A script may contain the same declarations
as a Package module and may additionally contain executable top-level
statements:

```nexa
use host::console;

let name = args
    .get(0)
    .unwrap_or("world");

console::write_line("hello, ${name}");
```

`args` is an implicit immutable binding of type `Array<string>`. It contains
only the values after the source path; it does not contain `nexa`, `run`, or
the source path. A top-level declaration of another `args`
binding is an error because it would hide the script ABI.

When at least one executable top-level statement exists, the compiler lowers
those statements in lexical order into a synthetic entrypoint:

```nexa
fn main(args: Array<string>) -> i32 {
    // original top-level statements
    return 0;
}
```

If any top-level statement contains `.await`, including inside its expression
tree, the synthetic entrypoint is instead:

```nexa
async fn main(args: Array<string>) -> i32 {
    // original top-level statements
    return 0;
}
```

An explicit top-level `return` supplies the synthetic `main` result. Falling
off the end returns `0`. Synthetic lowering must preserve the original file
URI, byte spans, line/column locations, evaluation order, and diagnostics;
synthetic wrapper text is not reported as user source.

A single file may use either:

- an explicit valid `main` and no executable top-level statement; or
- one or more executable top-level statements and a synthetic `main`.

Combining an explicit `main` with any executable top-level statement is a
compile error. A declaration-only file with neither an explicit `main` nor an
executable top-level statement has no entrypoint and is also a compile error.
Declarations may coexist with either entrypoint form.

## Command-line interface

The user-facing run forms are:

```bash
nexa file.nexa args
nexa run file.nexa args
nexa run package-directory args
nexa run --project nexa.dev.toml --package package.id args
```

Runtime options must precede the source or package path. Every token after the
path is passed verbatim, in order, as a UTF-8 `string` value in `args`. The
project form resolves the selected Package and its locked static dependency
closure through the same canonical build pipeline used by check, build,
Engine, and editor tooling.

Running verified bytecode by function index is deliberately low-level:

```bash
nexa exec module.nxb --function 0
```

The removed form:

```bash
nexa run module.nxb --function 0
```

is a usage error. `nexa exec` does not apply the Standalone `main` ABI and must
not be presented as the ordinary Package execution path.

## Exit status

The CLI uses this complete exit-status contract:

| Outcome | Process exit status |
|---|---:|
| `main` completes | the returned `i32`, passed to the platform process-exit API without tool remapping |
| Runtime trap while running `main` | `4` |
| Source, analysis, link, bytecode, or verifier error | `1` |
| CLI usage, missing path, invalid option, or unavailable environment input | `2` |
| Tool I/O invariant failure or other internal error | `3` |

Operating systems may expose only a platform-sized portion of an `i32` process
status; Nexa does not reinterpret a successfully returned value to avoid the
reserved tool statuses. Diagnostics distinguish a returned value from a tool
failure even when their numeric status happens to match.

An asynchronous `main` that is cancelled by the launcher or exhausts a
non-renewable runtime limit is a runtime failure. A proper Runtime trap maps to
`4`; a failure of the tool itself maps to `3`.

## Resource and failure rules

Standalone execution uses finite, resolved limits for fuel, cumulative budget,
heap objects and bytes, frame/call depth, tasks, Host resources, cleanup, and
output. Package requests may lower host policy ceilings but cannot raise them.
Single-file execution uses the CLI's deterministic Standalone defaults.

Every candidate passes the formal Syntax, Analysis, Typed IR, Bytecode, and
Verifier pipeline before `main` begins. Compilation or verification failure
causes no user code or Console Host call. Runtime capacity is checked before
an allocation or output operation that can fail. A trap is reported with its
source-level stack and does not fall back to invoking another function.

## Required diagnostics

The implementation must distinguish and point at:

- a Package top-level executable statement;
- explicit `main` combined with script statements;
- missing, duplicate, misplaced, or incorrectly typed `main`;
- a top-level script redeclaration of implicit `args`;
- `.await` in an explicitly synchronous `main`;
- a missing Standalone Host capability;
- an attempt to use `nexa run` as indexed bytecode execution.

Source diagnostics retain the original file or Package URI and exact source
span. CLI usage errors do not masquerade as compiler diagnostics.

## Conformance

Standalone conformance covers synchronous and asynchronous `main`, ordered
arguments, returned exit status, runtime trap status, Package top-level
rejection, top-level script lowering, top-level `.await`, explicit-main
conflict, missing and invalid `main`, Console stdout/stderr routing, and the
separation of `run` from indexed `exec`.
