# Migrating a Nexa Package to M4

M4 accepts only `package.toml schema = 2`.

1. Set `kind = "application"` or `kind = "library"`.
2. Set `source_root = "src"`.
3. For an Application, replace the old entry file path with a Module path,
   such as `entry = "snake.classic_rules"`.
4. Move the file to the exact mapped path, such as
   `src/snake/classic_rules.nexa`.
5. Add the exact declaration `module snake.classic_rules;`.
6. Replace the old implicit Host import with `import host as snake;` and
   qualify every Host type, variant, constructor, and function as `snake.*`.
   Required Exports in the Entry Module must be `pub`.
7. Remove `state_schema`; M4 computes State identity from Stateful types and
   fields.
8. Generate `nexa.lock` explicitly when the Package has local dependencies.

Schema 1, implicit Host imports, entry file paths, and compatibility aliases
are not retained.
