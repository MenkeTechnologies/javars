# Known gaps

An honest list of what javars does **not** do yet. Slice 1 is a single-class,
single-`main` subset; unsupported constructs are reported as parse errors, never
silently mis-run.

## Implemented

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
- **The standard library.** No `Math`, `Integer.parseInt`, `java.util.*`, and no
  non-`String` receiver methods. `System.out.print`/`println` and the `String`
  methods above are the only library surface.
- **Field access on values.** `arr.length`, `System.err`, and any `x.field`
  (only `recv.method(...)` calls dispatch; a bare `.field` is a parse error).
- **`return <value>` from `main`.** `main` is `void`; only a bare `return;`
  (which ends the program) is accepted there. Value returns work in methods.
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
