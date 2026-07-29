# Engine Diagnostics

Status: M3R1 COMPLETE

An Engine diagnostic code is valid evidence only when all of the following
exist:

```text
registered error code
→ real public or product-internal call path
→ test executes that path
→ emitted EngineDiagnostic is captured
→ the same object renders as Human, JSON, and NDJSON
```

Diagnostic evidence must not construct its target code directly with
`Diagnostic::without_source` or an equivalent helper. Such construction proves
only formatting, not that the product emits the diagnostic.

The Engine diagnostic harness records the public entry point, captured
diagnostic, three renderings, and whether the evidence traversed a real Engine
path. Registered Engine codes must equal codes observed through real paths;
direct target-code construction must remain zero.

If an Engine code has no fallible product path, the code, fixture, registry
entry, and documentation must be removed until that path exists. Compiler,
Verifier, and Runtime leaf codes remain authoritative and are not rewritten
into generic Engine codes.
