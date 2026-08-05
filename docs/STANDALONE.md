# Standalone Nexa programs

Status: M4R1 Standalone Profile v1 COMPLETE

The Standalone profile runs a Nexa program as a command-line application. It
uses the same syntax, analysis, Typed IR, Bytecode v7, verifier, and runtime as
an embedded package. `nexa run` selects a source program and calls its typed
`main`; it never asks users for a bytecode function index.

## Run a program

Runtime options precede the source path. Every token after the path is an
application argument, with no separator:

```sh
nexa hello.nexa Alice
nexa run hello.nexa Alice
nexa run path/to/package Alice
nexa run --project nexa.dev.toml --package example.hello Alice
```

The first two forms run one source file. The third resolves an Application
package from its directory. The fourth resolves a package through a schema-2
project and package ID. Arguments after the selected path become the
`Array<string>` passed to `main`, or the implicit `args` binding in a top-level
script. They remain in order as UTF-8 strings and do not include the
executable, command, or source path.

## Package entrypoint

An Application package must provide exactly one of these signatures:

```nexa
fn main(args: Array<string>) -> i32 {
    return 0;
}
```

```nexa
async fn main(args: Array<string>) -> i32 {
    return 0;
}
```

No other parameter list, return type, overload, or public Future type is a
Standalone entrypoint. The name `main`, parameter name `args`, parameter type,
arity, and return type are all part of the ABI. A missing, duplicate, or
misplaced `main` is a compilation error.

The synchronous form runs as one verified function. The asynchronous form runs
as one bounded task to completion under the normal wait, yield, cancellation,
fuel, trap, and cleanup rules.

Package modules may contain only `use`, module-level `const`, `struct`, `enum`,
`class`, and `fn` declarations. Top-level executable statements are rejected
in package mode, including in the entry module.

Module identity comes from the source path. A package entry such as
`entry = "app.main"` resolves `src/app/main.nexa`; no in-source module
declaration is present.

## Console Host

Standalone programs receive only the deterministic `host::console` contract:

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

```nexa
use host::console;

fn main(args: Array<string>) -> i32 {
    console::write("hello");
    console::write_line(", world");
    console::write_error("warning");
    console::write_error_line(": example");
    return 0;
}
```

`write` and `write_line` write to standard output.
`write_error` and `write_error_line` write to standard error. The `_line`
forms append one line feed; the other forms append nothing. All four return
Unit.

The Standalone Host does not provide file, network, process, environment,
wall-clock, or system-random APIs. A package cannot obtain those facilities by
naming an undeclared capability.

## Single-file scripts

A single `.nexa` file may use a normal explicit `main`, or it may contain
top-level executable statements:

```nexa
use host::console;

let name = args
    .get(0)
    .unwrap_or("world");

console::write_line("hello, ${name}");
```

The compiler lowers those statements, in source order, into a synthetic
`main(args: Array<string>) -> i32`. The implicit `args` binding contains the
arguments after the script path; a top-level declaration that would shadow this binding
is an error. Reaching the end of a top-level script returns 0. An explicit
top-level `return` supplies the synthetic `main` result.

If any top-level statement consumes an asynchronous result with `.await`, the
compiler synthesizes an `async fn main`:

```nexa
use host::console;

async fn greeting() -> string {
    return "hello";
}

console::write_line(greeting().await);
```

A single file cannot contain both an explicit `main` and top-level executable
statements. `use` and `const` declarations, type declarations, and helper
functions do not count as executable statements. A declaration-only file with
neither an explicit `main` nor an executable statement has no entrypoint and
is rejected. Synthetic lowering retains the original URI and exact source
spans; diagnostics do not point into an invented wrapper.

## Exit status and failures

`nexa run` uses this exit-status contract:

| Outcome | Status |
| --- | ---: |
| `main` completes | the returned `i32` |
| Runtime trap | `4` |
| Syntax, analysis, link, bytecode, or verifier error | `1` |
| Usage, invalid option, missing path, or unavailable environment input | `2` |
| Tool I/O invariant or internal failure | `3` |

The returned `i32` is passed to the platform process-exit API without
remapping reserved values; an operating system may expose only a
platform-sized portion. Diagnostics still distinguish a returned value from a
tool failure.

Every program passes Syntax, Analysis, Typed IR, Bytecode v7, and Verifier stages
before user code or a Console call can run. Standalone execution resolves
finite limits for fuel, cumulative budget, heap, frames, call depth, tasks,
Host resources, cleanup, and output. Capacity is charged before an operation
that can fail.

## Low-level bytecode execution

`nexa exec` is the diagnostic path for an already-built bytecode module:

```sh
nexa exec module.nxb --function 0
```

It exposes verifier and function-index controls for tool authors and tests,
including `--arg-i32`, `--fuel`, and `--limits-file`. It is not the Standalone
application model. `nexa run` accepts source or a package and resolves the
typed `main` ABI; combining `nexa run` with `--function` is a usage error.

For exploratory cells and persistent bindings, use the isolated
[REPL](REPL.md). For embedding packages in a Rust Host, see
[EMBEDDING.md](EMBEDDING.md).
