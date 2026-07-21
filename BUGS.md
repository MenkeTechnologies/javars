# Known gaps

An honest list of what javars does **not** do yet. Slice 1 is a single-class,
single-`main` subset; unsupported constructs are reported as parse errors, never
silently mis-run.

## Implemented

- **Control flow.** `if`/`else`, `while`, `do`/`while`, C-style `for`, and the
  ternary `cond ? a : b` (right-associative, result typed by branch promotion).
  `switch (x) { case L: … break; default: }` on `int` and `String`, with
  fall-through between groups and grouped labels (`case 1: case 2:`). `break`
  and `continue`, including the labeled forms (`outer: for (…) { break outer; }`)
  — a labeled `break` exits the named loop/`switch`, a labeled `continue` steps
  the named loop.
- **Standard-library essentials.** `Math.abs`/`max`/`min`/`pow`/`sqrt`/`floor`/
  `ceil`/`round` (with Java's int-vs-double overload result typing),
  `Integer.parseInt` (with radix)/`valueOf`/`toString` (with radix),
  `Long.parseLong`, `Boolean.parseBoolean`, and `String.valueOf`. Malformed
  numeric input faults like `NumberFormatException`; an unregistered static
  method is an error. `System.err.print[ln]` writes to stderr.
- **User-defined `static` methods.** `static <ret> name(<params>) { … }` lowers
  to fusevm's native `Op::Call` frame ABI — parameters and locals live in call-
  frame slots, so recursion, mutual recursion, and forward references all work.
  `return <expr>;` returns a value; `void` methods return `null` on fall-off.
  Arity is checked at compile time.
- **`String` instance methods.** `recv.method(args)` dispatches on `String`
  receivers through the host: `length`, `isEmpty`, `charAt`, `substring`,
  `indexOf`, `contains`, `equals`, `equalsIgnoreCase`, `toUpperCase`,
  `toLowerCase`, `trim`, `startsWith`, `endsWith`, `concat`, `replace`,
  `repeat`. Index/length semantics use Unicode scalar (`char`) positions — exact
  for ASCII/BMP, one unit per astral char (see the `char` simplification below).

## Not implemented (parse errors today)

- **Instance methods, method overloading.** Only `static` helpers are compiled,
  and they are keyed by name — two methods with the same name (overloads)
  collide. No `this`, no dispatch on a receiver's runtime type.
- **Classes, fields, objects, `new`.** No instance model, no fields, no
  constructors, no `this`. `new` is lexed but has no semantics.
- **Arrays.** `int[] a = …`, indexing (`a[i]`), `.length`, `new int[n]`. The
  `args` parameter of `main` is parsed and ignored. fusevm's `Value::Array` is a
  by-value `Vec`, so a faithful implementation of Java's array *reference*
  semantics (pass to a method, mutate an element, observe the change in the
  caller) needs a heap/handle indirection that is not yet built — arrays stay
  unsupported rather than silently mis-run the aliasing case.
- **Most of the standard library.** The `Math`/`Integer`/`Long`/`Boolean`/
  `String` statics listed above and the `String` instance methods are the whole
  library surface — no `java.util.*` collections, no non-`String` receiver
  instance methods, no `Math` constants (`Math.PI`).
- **Field access on values.** `arr.length` and any `x.field` (only
  `recv.method(...)` calls dispatch; a bare `.field` is a parse error).
- **`return <value>` from `main`.** `main` is `void`; only a bare `return;`
  (which ends the program) is accepted there. Value returns work in methods.
- **Arrow `switch` expressions**, `switch` on enums/patterns, and the enhanced
  `for` (`for (var x : coll)`).
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
