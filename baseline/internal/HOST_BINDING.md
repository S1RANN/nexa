# Host Binding

Version: **1.0.0**

The `.nidl` file is the only authoritative Host API definition.

`nexa-idl` deterministically generates into `OUT_DIR`:

- the Rust Host Trait and test Stub;
- the Host Dispatcher;
- borrowed argument decoding and return encoding;
- typed error/completion conversion;
- stable function identifiers;
- the Exact Interface Hash;
- the Nexa module declaration and typed export markers.

Host crates include the generated file and implement only the generated Trait.
They must not hand-maintain function-name matching, argument slots, return
slots, stable ID tables, or interface hash tables.

Loading a module compares its embedded Exact Interface Hash with the generated
Host Registry before interpreter execution. Any incompatible `.nidl` change
therefore rejects old bytecode before gameplay code runs.
