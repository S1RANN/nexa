# Migrating an M4 Package to Language v2

M4R1 is a source-breaking reset. It retains the M4 schema 2 Package and static
Library model, but it does not retain aliases for the M4 language or former
Contract surface. After completing this historical migration, also follow
[Migrating to Contract v3](MIGRATING_TO_CONTRACT_V3.md).

For each Package:

1. Keep `schema = 2`, `source_root = "src"`, and `kind = "application"` or
   `kind = "library"`.
2. Keep each file at the path that defines its Module identity. For example,
   `src/snake/classic_rules.nexa` is `package::snake::classic_rules`.
3. Delete every source-level `module` declaration.
4. Replace namespace imports with `use`. On the current Language v3 surface,
   Host symbols are referenced directly as `host::member`; same-Package paths start with
   `package::`, `self::`, or `super::`; dependencies start with their Manifest
   alias.
5. Change mutable `var value` bindings to `let value`. Change bindings that
   must not be reassigned to block-local `const value`.
6. Change `task fn` to `async fn` and prefix awaits to postfix `.await`.
   Pending async values cannot be stored for a later await.
7. Replace a state-specific type declaration with
   `@state(version = N) class`. Fields are mutable by default.
8. Replace special function kinds with attributes: `@migration`,
   `@activation`, `@cleanup`, and `@immediate`.
9. Initialize both Struct and Class values with `Type { ... }`. Remove the old
   Class constructor keyword from `new Type { ... }`; update expressions use
   `Type { fields, ..base }`. The migration namespace `new.create<T>(...)`
   remains a separate persistent-state operation.
10. Rename functions, parameters, fields, and locals to `snake_case`; rename
    types, Contracts, and Variants to `PascalCase`; rename constants to
    `SCREAMING_SNAKE_CASE`.
11. Spell generic built-ins as `Array<T>`, `Buffer<T>`, `Option<T>`,
    `Result<T, E>`, `Token<T>`, and `Snapshot<T>`. Use `::` for namespaces,
    associated functions, and Enum Variants.
12. Convert the former IDL source to `contract` with `host {}` and `nexa {}` blocks, `handle`,
    `fn`, and `async fn`. Move async policies to attributes and remove
    source-level Request types.
13. Regenerate Rust bindings and Bytecode v7. Descriptor v2, generated marker
    types, and stable IDs replace canonical string hashes and function indexes.
14. Regenerate `nexa.lock` when the local dependency closure changes.

Application Package modules contain declarations only. A Standalone single
file may instead contain ordered top-level statements; the compiler lowers
them to a synthetic `main`, and the file cannot also declare an explicit main.

Schema 1 Packages, old source syntax, the former Contract surface, Bytecode v5, implicit Host
access, entry file paths, and compatibility aliases are not retained.
