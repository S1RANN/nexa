# Nexa Object Model v2

Version: **2**

Status: **COMPLETE**

This document is the normative object-model specification for Nexa v2
Structs, Enums, Classes, mutable Places, persistent state metadata, equality,
and GC integration. `COMPLETE` means the model and its repository-wide
implementation and conformance evidence are frozen for M4R1.

The terms **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are normative.

## 1. Model overview

Nexa v2 has three user-declared nominal type kinds:

| Kind | Category | Storage and assignment |
|---|---|---|
| `struct` | value type | fixed layout, inline where possible, copied by value |
| `enum` | tagged value type | fixed tag/payload layout, copied by value |
| `class` | non-null GC reference type | object on GC heap, references copied |

`@state` is metadata on a Class. It does not introduce another type kind.

This split is observable through construction, mutation, equality, aliasing,
const eligibility, layout recursion, and garbage collection. The compiler MUST
NOT erase the distinction by lowering a nominal Class to Unit or treating a
Struct as an object reference.

## 2. Values, objects, bindings, and Places

A **value** is a scalar, tuple, Struct value, Enum value, built-in value, or
Class reference. An **object** is the GC allocation denoted by a Class
reference. A **binding** stores a value. A **Place** is a writable or
non-writable storage path such as a local or field selection.

Binding mutability and object-field mutability are independent:

```nexa
let enemy = new Enemy {
    name: "slime",
    health: 100,
};

enemy.health = 20; // allowed if health is declared mut
enemy = other;     // rejected: binding is not rebindable

let mut other_enemy = enemy;
other_enemy = other; // allowed
```

`let` and `let mut` never change the declared layout or mutability of a Class
field.

### 2.1 Writable Place rules

A local Place is writable exactly when its binding was declared `let mut`.

A Struct-field Place is writable when its containing Struct Place is writable.
Struct fields have no declaration-level `mut`.

A Class-field Place is writable exactly when that field is declared `mut`.
The mutability of the binding containing the Class reference is irrelevant to
that field write.

For a nested path:

- selecting a Struct field propagates the writability of its containing Place;
- selecting a Class field checks that field's own `mut` flag before replacing
  or mutating value storage inside that field;
- following a Class reference value reaches a new object; writes to that
  object's fields are governed by the reached Class declarations, not by the
  Place from which the reference was read.

Thus an immutable Class field can hold a reference whose target has other
mutable fields, but the immutable field itself cannot be replaced.

All assignments are type checked after Place writability is established. A
compiler MUST diagnose rebinding, Struct Place mutation, and Class-field
mutation as distinct failure categories.

## 3. Structs

### 3.1 Declaration and construction

```nexa
struct Cell {
    x: i32,
    y: i32,
}

let cell = Cell {
    x: 1,
    y: 2,
};
```

Struct declarations use `PascalCase`; their fields use `snake_case`. Fields
are ordered and comma-separated, with a supported trailing comma. `mut` is
forbidden on Struct fields.

Construction uses the type name without `new`. Every field appears exactly
once unless an update base supplies it. Unknown and duplicate fields are
errors. Field evaluation follows source order.

`new Cell { ... }` is rejected because `Cell` is a Struct.

### 3.2 Value semantics

A Struct has deterministic fixed layout. Assignment, argument passing, return,
field initialization, and container insertion have value semantics. Copying a
Struct copies every field value:

- nested Structs and Enums are copied recursively;
- a nested Class field copies its reference, not the referenced object;
- resource-like fields retain the copy/ownership semantics defined by their
  own type and do not gain an implicit duplicate-resource operation.

An implementation MAY use registers or an equivalent optimized
representation, but observable behavior is as if the complete Struct value
were copied.

### 3.3 Mutability

```nexa
let cell = Cell { x: 1, y: 2 };
cell.x = 3; // rejected

let mut movable = Cell { x: 1, y: 2 };
movable.x = 3; // allowed
```

The field declaration does not decide this result. The containing Place does.
No operation may acquire a source-level reference that bypasses this rule.

### 3.4 Structural equality

`left == right` on the same Struct type is available exactly when every field
type supports equality. It compares corresponding fields in declaration
order. Nested Class fields use Class identity equality.

A Struct containing any field whose type does not define equality is not
implicitly comparable. Resource-like types are non-comparable unless their
normative type contract explicitly defines equality. The compiler reports the
first non-comparable field path. It MUST NOT compare object bytes, padding, GC
addresses exposed as integers, or resource internals.

## 4. Enums

### 4.1 Declaration and construction

```nexa
enum FoodEffect {
    None,
    Grow(i32),
    Teleport {
        cell: Cell,
    },
}
```

An Enum is a tagged value. Variants use `PascalCase`, remain in declaration
order, and may have:

- no payload;
- an ordered tuple payload;
- an ordered record payload with `snake_case` field names.

Construction always uses the type-associated path:

```nexa
FoodEffect::None
FoodEffect::Grow(2)
FoodEffect::Teleport { cell }
```

A bare `Grow(2)` is not inferred as an Enum variant. Variant tag order and
payload layout participate in the public API fingerprint.

### 4.2 Value and equality semantics

Enum assignment, parameter passing, and return use value semantics. Equality
is available when every payload reachable from every variant supports
equality. Values compare equal only when their variant tags match and their
active payloads compare equal. Inactive storage and padding are never
compared.

### 4.3 Recursive layout

The graph of by-value Struct fields and Enum payloads MUST be acyclic. Direct
or indirect infinite layouts are rejected:

```nexa
enum Expr {
    Add(Expr, Expr),
}
```

The diagnostic MUST identify the recursive edge, show the declaration cycle as
related locations, and recommend a Class node when recursive identity is
intended. The compiler MUST NOT silently insert a box, pointer, or GC
indirection.

A value type may contain a Class reference, and Classes may form cycles. For
example, `enum Link { End, Next(Node) }` is finite when `Node` is a Class.

## 5. Classes

### 5.1 Declaration

```nexa
class Enemy {
    name: string,
    mut health: i32,
}
```

A Class:

```text
has fixed declared field layout
has reference semantics and object identity
is allocated on the managed GC heap
is non-null by default
is sealed by default
does not participate in inheritance or dynamic dispatch
has no user-defined finalizer
```

Fields are ordered and comma-separated. A field's `mut` flag is part of the
type's public layout and fingerprint.

### 5.2 Construction

Class allocation requires `new`:

```nexa
let enemy = new Enemy {
    name: "slime",
    health: 100,
};
```

`Enemy { ... }` is rejected because it omits `new`. A successful construction
allocates one distinct object after all validation and required capacity/fuel
checks. Each field is initialized exactly once. Unknown, duplicate, or missing
fields are errors.

The language exposes no placement allocation, allocator choice, stack-only
Class, raw object address, or explicit deallocation.

### 5.3 Reference assignment and aliasing

Assigning, passing, or returning a Class value copies the reference:

```nexa
let first = new Enemy {
    name: "slime",
    health: 100,
};
let second = first;
second.health = 20;
// first.health is now 20
```

Both bindings denote the same object. Copying a reference never deep-copies
the object.

### 5.4 Field mutation

After construction, a Class field can be assigned only when declared `mut`.
Construction itself initializes both mutable and immutable fields.

```nexa
enemy.name = "boss"; // rejected
enemy.health = 50;   // allowed
```

`let mut enemy` permits replacing the reference stored in `enemy`; it does not
make `name` mutable. Conversely, `let enemy` does not prevent writing
`enemy.health`.

Every Class field write that can install a GC reference MUST pass through the
runtime's write barrier. Compiler optimization cannot bypass the barrier unless
it proves the equivalent collector invariant.

### 5.5 Identity equality

`left == right` for the same Class type means that both references denote the
same GC object. It does not compare fields and does not perform a deep copy or
user-defined comparison.

Object identity remains stable across GC movement. Source code cannot observe
an address. Two separately allocated objects are unequal even when every field
value is equal.

### 5.6 Absence

A Class value itself is non-null. Optional absence is written explicitly:

```nexa
Option<Enemy>
```

There is no `null` literal, nullable-Class marker, implicit null default, null
pointer, or null identity comparison. Definite initialization applies before a
non-optional Class value can be read.

## 6. Explicit update construction

### 6.1 Struct update

```nexa
let moved = Cell {
    x: 10,
    ..cell
};
```

Operand expressions are evaluated left-to-right in lexical order: explicit
field expressions first, then the base expression. The base is evaluated
exactly once. Explicit fields override the same fields copied from the base.
The result is a new Struct value. Updating does not mutate `cell`.

### 6.2 Class update

```nexa
let copied = new Enemy {
    health: 50,
    ..enemy
};
```

Operand expressions are evaluated left-to-right in lexical order: explicit
field expressions first, then the base expression. The base is evaluated
exactly once and MUST have the exact Class type being constructed. The
operation allocates a distinct object whose stored field values come from the
base except where replaced by the already evaluated explicit overrides.
Copying a Class reference field copies that reference; it is not a recursive
clone. Updating does not mutate the base object.

For both kinds:

- at most one `..base` is allowed;
- it follows all explicit fields;
- each explicit field appears at most once;
- the base supplies all omitted fields;
- visibility, type, and construction rules still apply.

`with` is not an update operator. Enum values do not support update syntax.

## 7. Persistent state Classes

Persistent state is declared:

```nexa
@state(version = 1)
class GameState {
    @stable("score")
    mut score: i32,
}
```

`@state`:

- is allowed only on a Class;
- appears at most once;
- has exactly one named argument `version`;
- requires a positive integer version in `1..=4294967295`;
- leaves the type kind as `Class`;
- attaches `StateMetadata` used by schema, migration, reload, and inspection.

The semantic representation is:

```text
Type Kind      = Class
State Metadata = Some(version, ordered fields, stable field identities)
```

State Classes retain ordinary Class reference, mutation, equality, GC, and
construction semantics unless a state lifecycle profile imposes an additional
operation restriction. There is no `stateful class` syntax and no `Stateful`
type kind.

State field identity defaults to the containing stable type identity plus the
field declaration identity. `@stable("id")` MAY pin a field identity across an
intentional rename or source move. Its value matches:

```text
[A-Za-z][A-Za-z0-9._-]{0,127}
```

and MUST be unique within the Package's explicit stable-ID domain. A collision
diagnostic includes both declarations.

Changing the state version, field order, field type, field `mut` flag, stable
identity, or Class/state kind changes the state-schema fingerprint and
invalidates dependent migration/link results.

## 8. Special function attributes

Lifecycle roles use attributes on ordinary function declarations:

```nexa
@migration
fn migrate_state(...) {
}

@activation
fn activate() {
}

@cleanup
fn cleanup() {
}

@immediate
fn calculate_score(...) -> i32 {
}
```

`migration fn`, `activation fn`, `cleanup fn`, and `immediate fn` are removed
syntax. The AST and Typed IR MUST preserve one ordinary function declaration
plus validated role metadata.

The compatibility rules relevant to the object/state model are:

| Rule | Required result |
|---|---|
| `@migration` with `@activation` | reject |
| any two of `@migration`, `@activation`, `@cleanup`, `@immediate` | reject |
| `@cleanup` on `async fn` | reject |
| `@immediate` on `async fn` | reject |
| `@test` with `@migration`, `@activation`, or `@cleanup` | reject |
| `@test` with `@immediate` | permit at this matrix; Package Test signature/profile rules still apply |
| `@state` on a function, Struct, Enum, field, or local | reject |

The analysis layer diagnoses an illegal combination before lowering it to a
lifecycle slot. It MUST NOT select the first attribute, discard later ones, or
invent an ordering.

## 9. GC integration

### 9.1 Exact roots

Every live Class reference is an exact GC root whether it appears:

```text
directly in a local or frame slot
inside a Struct
inside the active payload of an Enum
inside a Tuple, Option, Result, Array, or another supported aggregate
inside an async/task frame
inside persistent state
```

Root maps describe the actual live locations at each safepoint. Treating all
registers as roots, omitting nested references, or marking inactive Enum
payload storage is non-conforming.

### 9.2 Object graph and barriers

Class objects may reference themselves or form cycles. Reachability, not
reference counting or lexical scope, determines liveness.

Every write that creates or changes an object-to-object reference uses the
collector's write barrier. This includes a Class field containing a Struct or
Enum that itself contains a Class reference. Bulk/update construction must
establish the same collector invariant as individual field writes.

Allocation failure, heap limits, and fuel limits are reported through the
defined runtime error/trap model. User code cannot catch an allocation through
exception unwinding and cannot run a finalizer.

## 10. Const and public-layout interaction

Structs and Enums are const-safe when all contained values are const-safe.
Classes are never const-safe because construction requires GC allocation and
creates object identity.

The Public API Fingerprint includes:

```text
nominal kind (Struct, Enum, or Class)
nominal visibility and stable identity
ordered Struct/Class field names and types
Class field mutability
ordered Enum variants, tags, and payloads
state metadata and state-field identities where public
equality-relevant and async/public signature facts defined by Language v2
```

Struct field order, Enum variant order, and Class field order are semantic.
Formatting, comments, and top-level declaration order are not.

## 11. Diagnostics

Object-model violations emit registered stable diagnostic codes, original
source identities, deterministic related locations, and the smallest useful
primary span.

| Failure | Primary span and required information |
|---|---|
| `mut` on Struct/Enum payload field | `mut`; explain that Struct writability comes from the Place |
| Struct field assignment through immutable Place | assigned field Place |
| Immutable Class field assignment | field selection; relate the field declaration |
| Rebinding immutable Class binding | binding Place; distinguish from field mutability |
| Struct constructed with `new` | `new`; identify Struct kind |
| Class constructed without `new` | Class constructor path; require `new` |
| Unknown/duplicate/missing construction field | field name or constructor; relate duplicate/original declaration |
| Invalid update base/order/kind | `..base` |
| Non-comparable aggregate | `==`; include first non-comparable field path |
| Recursive value layout | recursive field/payload edge; include complete declaration cycle and suggest Class |
| `null` or nullable Class syntax | invalid null/nullable token |
| Invalid `@state` target/argument | attribute or argument; identify allowed Class form |
| Stable state-field collision | later `@stable`; relate the first declaration |
| Illegal lifecycle-attribute combination | conflicting attribute; relate the other attribute |
| Invalid exact-root/write-barrier bytecode metadata | verifier location plus originating source span when available |

Diagnostics MUST NOT claim that adding `let mut` fixes an immutable Class field,
suggest `Box<T>` for recursive values, or silently deep-copy a Class.

## 12. Explicit exclusions

The Nexa v2 object model does not expose:

```text
user-defined generics
traits or interfaces
inheritance
dynamic dispatch
operator overloading
Box<T>
Gc<T> or Ref<T>
raw pointers
&, &mut, address-of, or explicit dereference
a source-level borrow checker
nullable implicit references or null pointers
user-defined allocation
user finalizers
reflection or runtime field enumeration
structural Class equality
automatic deep clone
shared-memory threads
```

Compiler/runtime-internal GC pointers, handles, barriers, root maps, and task
frames are implementation machinery and are not source-level types or syntax.

## 13. Conformance boundary

An implementation conforms to this COMPLETE object model only when tests
demonstrate:

```text
Struct value copying
Struct Place mutability
Enum tags and payloads
Class reference copying and alias visibility
Class field-level mutability
Class identity equality
Option<Class> absence
cyclic Class objects
exact roots through nested Struct/Enum values
write barriers for direct and nested Class references
rejection of recursive value layouts
rejection of Box, pointer, and null syntax
state metadata remaining Class metadata
```

These tests MUST execute through the production syntax, analysis, Typed IR,
bytecode, verifier, and runtime paths. A parser-only fixture or a fabricated
report is not conformance evidence.
