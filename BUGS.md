# Known gaps

An honest list of what javars does **not** do yet. Slice 1 is a single-class,
single-`main` subset; unsupported constructs are reported as parse errors, never
silently mis-run.

## Not implemented (parse errors today)

- **User-defined methods.** Only `main` is compiled. Calling a helper method, or
  any static/instance method, is rejected. (Next wave: fusevm's native
  `Op::Call` frame ABI.)
- **Classes, fields, objects, `new`.** No instance model, no fields, no
  constructors, no `this`. `new` is lexed but has no semantics.
- **Arrays.** `int[] a = …`, indexing, `.length`. The `args` parameter of `main`
  is parsed and ignored.
- **The standard library.** No `Math`, `String` methods, `Integer.parseInt`,
  `java.util.*`. Only `System.out.print`/`println` exist.
- **Method/field access on values.** `x.foo()`, `s.length()`, `System.err`.
- **`return <value>`.** `main` is `void`; only a bare `return;` is accepted.
- **`switch`, `do/while`, labeled break, ternary `?:`, enhanced `for`.**
- **`try`/`catch`/`finally`, exceptions, `throw`.**
- **Generics, lambdas, streams, records, `var` type inference beyond storage.**

## Modeled with a documented simplification

- **Types are not checked.** Declared types (`int`, `String`, …) are retained for
  diagnostics but do not gate execution — the runtime is dynamically typed on the
  fusevm value model. Definite-assignment and type errors that `javac` would
  reject may run.
- **`==` on non-numbers compares by value**, not Java reference identity. In
  slice 1 this only affects strings/booleans and matches the far more common
  intent; true reference `==` arrives with the object model.
- **`char` literals are one-character strings**, not an integer `char` type.
- **Integer arithmetic uses fusevm's 64-bit semantics.** Java `int` 32-bit
  overflow wrapping is not yet modeled (values behave like `long`).
- **Uninitialized locals are unbound** rather than rejected; reading one before
  assignment yields `null` instead of a compile error.
