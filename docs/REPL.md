# Nexa REPL

Status: M4R1 REPL v1 COMPLETE

Start an isolated interactive session with:

```sh
nexa
nexa repl
```

With no command or script, `nexa` starts the same REPL. An interactive terminal
displays the `nexa> ` prompt. Redirected standard
input runs in batch mode but submits every item through the same cell
pipeline.

The REPL is a frontend to the real Nexa toolchain, not a second expression
interpreter. Each submitted cell passes through the lossless syntax tree,
analysis, Typed IR, Bytecode v7 generation, verifier, and runtime.

## Cells and persistent bindings

The session is a synthetic package with these stable identities:

```text
Package ID  nexa.repl
Module      repl.session
Cell        repl::cell_N
```

Expressions, declarations, and statements use Nexa Language v2:

```nexa
1 + 2
let greeting = "hello";
let mut count = 1;
count = count + 1;
fn double(value: i32) -> i32 { return value * 2; }
double(count)
```

A final expression without a semicolon is displayed as the cell result; Unit
produces no result line. Incomplete syntax is buffered until the production
parser identifies a complete cell.

Top-level `let` and `let mut` bindings are lowered into a hidden
`@state(version = 1) class __ReplEnvironment`. Each successful declaration
receives a stable slot derived from its cell and declaration position. `let`
slots may be written only by their defining cell; `let mut` slots may be
assigned by later cells. Variables and functions may shadow earlier
definitions. Type redefinition is rejected because existing state may use the
old layout; use `:reset` before declaring the replacement type. A previously
compiled function continues to refer to the definitions it originally
resolved.

Each successful cell extends the synthetic package, performs Restart Reload
while preserving the REPL environment, and executes only the newest cell.
Earlier cells are not replayed, so their output and Host side effects do not
run again.

## Asynchronous cells

A cell containing `.await` is compiled as an asynchronous cell task:

```nexa
async fn answer() -> i32 { return 42; }
answer().await
```

Await follows the normal language rules: it is postfix, applies directly to a
known asynchronous call, and may be followed by `?`, a field, an index, or
another call. The REPL does not expose a storable `Future` value.

Pressing Ctrl+C cancels the currently running cell. It does not destroy the
session or discard state committed by earlier cells. At an idle prompt,
Ctrl+C clears the current input buffer.

## Commands

REPL commands begin with `:`:

```text
:type <expr>       analyze without executing or committing
:ast <source>      parse and print syntax/AST without committing
:bytecode [name]   print verified bytecode for a named item or latest cell
:gc                collect at a safe point and report heap use
:memory            report current and maximum resource use
:load <file>       submit one UTF-8 file as the next cell
:reset             discard package, state, declarations, and history
:help              list commands and usage
:quit              end the session cleanly
```

`:load` uses the same cell transaction as typed input. It does not create a
second package mode or bypass verification, and it preserves the loaded file's
URI and source spans. Relative paths resolve from the REPL process working
directory. `:reset` resets cell numbering, bindings, functions, types,
diagnostics, and runtime state. Control commands other than `:load` do not
consume a cell number. A `main` loaded into the REPL is an ordinary function
and is not invoked automatically. Unknown commands and invalid operands report
a command diagnostic without changing the session.

## Failure recovery

A cell is transactional:

```text
parse/analyze/compile/verify/runtime success → commit cell and state
any failure                              → discard the cell
```

When a successful Cell adds bindings, the Runtime uses a
`TransactionalStateExtension` to append their hidden environment slots. This
is not state migration: the environment Stable ID and version stay unchanged,
all committed fields remain an exact prefix, no `@migration` or `@activation`
function runs, and the Cell must initialize every appended slot before commit.
Code and state publish atomically; failure discards the staged slots.

The REPL also accumulates its effective Contract selection across successful
Cells so older functions retain the Host/type authority they were compiled
against. Each staged fingerprint is computed from the committed selection
union the current Cell's referenced types, Host calls, Required entrypoints,
and implemented Optional entrypoints. Failed Cells do not expand that
selection; `:reset` clears it with the Package and state.

After a syntax, type, verifier, fuel, Host, or runtime error, the last
successful session remains valid and the next cell can run. A failed cell
does not reserve its declarations or partially update persistent bindings.
Console bytes emitted before a runtime failure remain visible; external output
cannot be rolled back.

The REPL enforces these default limits:

```text
Live heap objects        4,096
Fuel per cell            20,000
Committed cells          1,024
Diagnostic history       256 entries
Captured output per cell 1 MiB
```

The heap ceiling covers committed state plus the staged cell. The fuel ceiling
is both the cell slice and cumulative budget; the output ceiling combines
stdout and stderr. `:memory` reports current use. Hitting a limit fails the
current cell without corrupting prior state; `:reset` reclaims the session.

REPL v1 always owns its synthetic package and runtime. It cannot attach to or
modify another process, an embedded `NexaEngine`, a game Realm, or a package
already running elsewhere.

`:quit` and end-of-file at an idle prompt exit with status 0. Cell diagnostics
are non-terminal and return to the prompt. Startup usage failure exits with
status 2; an internal failure that makes the committed session unsafe exits
with status 3.

For a repeatable command-line application use
[Standalone](STANDALONE.md). For package and Host integration use
[Embedding Nexa](EMBEDDING.md).
