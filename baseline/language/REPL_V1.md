# Nexa REPL v1

Version: **1.0.0**

Status: **COMPLETE**

This document defines the first interactive Nexa session model. The REPL is a
client of the formal Language v2 build and Runtime pipeline. It is not a second
expression interpreter, an Engine debugger, or a process-attachment facility.

## Entry and profile

The command is:

```bash
nexa
nexa repl
```

With no command or script, the naked command selects this same profile.
Redirected standard input automatically disables the interactive prompt.

The default session binds the Standalone Console Host and creates one hidden
synthetic Package:

```text
Package ID  = nexa.repl
Module      = repl.session
Cell N      = repl::cell_N
```

`N` is a monotonically increasing submission ordinal within the session.
`:reset` starts a new empty session and resets the ordinal. Commands other than
`:load` are control commands and do not consume a Cell ordinal.

Every submitted Cell uses the `ReplCell` compilation profile and passes through
the production pipeline:

```text
lossless Syntax
→ shared Analysis
→ Typed IR
→ Bytecode v7
→ Verifier
→ Runtime
```

The REPL must not directly evaluate an AST or introduce a separate expression
type checker, bytecode evaluator, or Host-call path.

## Cell forms

A Cell may contain declarations, top-level bindings, executable statements, or
a final expression. A final expression without a semicolon is evaluated as the
Cell result; `unit` produces no result line. Rendering a value is a tool
operation based on the verified runtime type and does not add reflection,
`dynamic`, a user `Display` trait, or an overloadable language operation.

The REPL buffers incomplete syntax until it forms a complete Cell. Parse
recovery and completeness detection come from `nexa-syntax`, not prompt-side
token counting.

Top-level `let` and `let mut` are lowered into a hidden state type:

```nexa
@state(version = 1)
class __ReplEnvironment {
    // compiler-generated stable slots
}
```

This code block is descriptive pseudo-source, not text passed back through the
user identifier grammar. The compiler creates the type and slots in a reserved
internal symbol namespace after parsing. Compiler-generated symbols are exempt
from source spelling classes precisely because no source token can declare,
refer to, shadow, or collide with them; user-declared leading or repeated
underscores remain invalid under Language v2.

The hidden type and its fields cannot be named by user code. Each successful
binding declaration receives a stable slot identity derived from its committed
Cell and declaration position. The slot survives later Cell reloads.

Binding semantics are:

```text
let      → initialize its slot exactly once; assignment through that binding is rejected
let mut  → initialize its slot and permit assignment from later Cells
```

Resolution in a new Cell sees the newest successful declaration. Variables and
functions may shadow earlier variables or functions. Shadowing creates a new
definition and, for a binding, a new stable slot; it does not mutate or rename
the shadowed definition. Previously compiled functions continue to refer to
the definition they originally resolved.

A type name (`struct`, `enum`, or `class`) cannot be redefined in one session.
The diagnostic must identify the original declaration and suggest `:reset`.
This prohibition avoids pretending that two incompatible layouts are one
reload-compatible type.

## Transactional Cell reload

For each Cell, the REPL:

1. appends the Cell to a staged copy of the synthetic Package;
2. analyzes and compiles the complete staged Package;
3. verifies the staged bytecode;
4. opens the Runtime's staged reload transaction and, only when the hidden
   environment gains slots, supplies a `TransactionalStateExtension`;
5. executes only `repl::cell_N`;
6. commits the Package generation and staged state only after successful Cell
   completion.

Old Cell entrypoints are never executed again during reload. Their declarations
remain available for name resolution until `:reset` or the Cell limit is
reached. Restart Reload preserves `__ReplEnvironment` slots using their stable
identities.

Syntax, analysis, link, verifier, trap, fuel, cancellation, and other runtime
failures abort the staged generation. The previously committed Package,
bindings, functions, types, and state remain usable, and a later Cell can
continue normally. A failed Cell never becomes a name-resolution input.

### `TransactionalStateExtension` is not migration

REPL state growth uses a narrow Runtime authority, not the Language
`@migration` mechanism. The old and Candidate state schemas must each contain
exactly the same single hidden environment state Class with the same Stable ID
and version. Every committed field must remain an identical prefix, and the
Candidate may only append fields for bindings introduced by the staged Cell.
The Candidate must contain no migration-effect function, migration entrypoint,
or activation entrypoint.

Appended fields begin as pending and have no value. The current Cell must
initialize every new slot before it can become Ready-to-Commit. The Runtime
then validates the complete Candidate schema, state handles, exact GC roots,
and absence of pending fields. A failure discards the Candidate and staged
extension; success publishes code and state atomically. This path never invokes
user migration code, never synthesizes Preserve/Replace/Delete decisions, and
never reports a Cell schema append as a migration.

### Cumulative effective Contract

The session retains the effective Contract selection committed by earlier
Cells. A staged Cell unions that selection with the current staged Package's
referenced shared types, Host functions, Required Nexa entrypoints, and
actually implemented Optional entrypoints, then recomputes the canonical
effective descriptor and its 32-byte fingerprint. The selection is monotonic
until `:reset`, because previously compiled functions remain callable and
their Host/type authority cannot disappear.

Only a successful Cell commits the expanded selection and fingerprint. A
failed Cell cannot permanently add Host authority or invalidate the committed
identity. `:reset` discards the synthetic Package, state, cumulative selection,
and fingerprint together. Unrelated Optional Contract declarations remain
outside the selection until required or actually implemented.

Console bytes already emitted by a Cell are external observations and cannot
be rolled back, but failure still rolls back all REPL-owned Package and state
changes. Other external Host capabilities are not part of REPL v1.

## Asynchronous Cells and interruption

A Cell containing `.await` in executable top-level code is lowered to an
asynchronous Cell task under `ASYNC_V2.md`. A Cell without `.await` is lowered
to a synchronous Cell entrypoint. Declarations of asynchronous functions do
not by themselves force execution of an asynchronous Cell.

While a Cell task is running, Ctrl+C requests ordinary cancellation of that
Cell, including bounded cleanup. Cancellation aborts the staged Cell but does
not destroy the committed session. A late Host result for the cancelled Cell
is discarded by normal Runtime ownership rules. Ctrl+C while no Cell is
running clears the current input buffer and leaves the session intact.

## Commands

REPL v1 supports exactly these required control commands:

```text
:type <expr>
:ast <source>
:bytecode [name]
:gc
:memory
:load <file>
:reset
:help
:quit
```

Their semantics are:

| Command | Semantics |
|---|---|
| `:type <expr>` | Analyze the expression against the committed session and print its resolved type; do not execute or commit a Cell |
| `:ast <source>` | Parse the supplied source with the lossless production parser and print its syntax/AST view; do not analyze, execute, or commit |
| `:bytecode` | Print verified bytecode for the latest committed Cell |
| `:bytecode <name>` | Resolve `name` in the committed session and print its verified function bytecode; ambiguity or absence is an error |
| `:gc` | Run a collection at a Runtime safe point and report reclaimed/current heap usage |
| `:memory` | Report current usage and resolved limits for heap, tasks, Host resources, state, diagnostics, Cells, and output |
| `:load <file>` | Read one UTF-8 file and submit its contents as the next Cell, preserving that file's URI and source spans |
| `:reset` | Cancel no running work, discard the synthetic Package, state, declarations, diagnostics, and Cell history, then create a fresh session |
| `:help` | Print the command list and short usage without changing session state |
| `:quit` | End the session cleanly with process status `0` |

`:load` uses the REPL Cell profile, not the Standalone `main` ABI. A `main`
declaration loaded into a session is an ordinary declaration and is not
automatically invoked. Relative load paths are resolved against the REPL
process working directory.

End-of-file at an idle prompt has the same effect as `:quit`. Unknown commands,
missing operands, and extra operands produce a command diagnostic without
changing the session.

## Resource limits

The default `nexa repl` session has these finite ceilings:

| Resource | Default ceiling | Accounting |
|---|---:|---|
| Live heap objects | `4,096` | Across the committed session and staged Cell |
| Cell fuel | `20,000` | Slice and cumulative budget for one submitted Cell |
| Committed Cells | `1,024` | Successful Cells since session creation/reset |
| Diagnostic history | `256` entries | Stored interactive diagnostics |
| Console output | `1,048,576` UTF-8 bytes per Cell | stdout and stderr combined |

The embedding/tool configuration may lower these ceilings but must not make any
of them unbounded or raise them during an active session. The resolved values
are visible through `:memory`.

Heap and output capacity are charged before the allocation or Host write that
would exceed the ceiling. A heap, fuel, or output-limit failure aborts the
staged Cell under the normal failure rule. Output already emitted before a
rejected write remains visible. At the Cell ceiling, submission is rejected
before parsing and the diagnostic suggests `:reset`.

Diagnostic history is a bounded oldest-first queue. Evicting an old diagnostic
does not evict its successful Cell, binding, or state. `:gc` may reclaim only
objects unreachable from exact Runtime roots, including
`__ReplEnvironment`; it cannot be used to bypass state or Cell accounting.

Runtime structural ceilings for frames, call depth, tasks, Host resources,
cleanup, and release records remain finite even though they are not separate
REPL history counters. Failure never silently raises a ceiling.

## Error recovery and process status

Cell and control-command diagnostics are non-terminal: they are printed and the
next prompt remains available. Every diagnostic identifies its logical Cell or
loaded-file URI and exact source span. A failed Cell's diagnostics may enter
the bounded history, but none of its definitions or state do.

`:quit` and idle end-of-file return `0`. Startup usage failure returns `2`; an
internal tool failure that makes the committed session unsafe returns `3`.
Compilation and runtime failures of submitted Cells do not terminate the REPL
process and therefore do not use the Standalone one-shot exit statuses.

## Isolation boundary

REPL v1 owns only its synthetic `nexa.repl` Package, Realm, and state. It has
no command or API that attaches to, inspects, reloads, or mutates another
running Engine, game Realm, task, or operating-system process. Implementing
such attachment under the name `:attach`, `:connect`, or an implicit discovery
mechanism is outside M4R1.

## Conformance

Conformance tests cover expression results, `let`, `let mut`, assignment,
variable and function shadowing, rejected type redefinition, functions,
asynchronous Cells, Ctrl+C, state preservation across reload, non-reexecution
of old side effects, compile/runtime error recovery, transactional rollback,
`:reset`, `:load`, `:gc`, every inspection command, and each resource ceiling.
All executed Cells must be observable through the production verifier and
Runtime path.
