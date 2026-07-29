# Known gaps

An honest list of what javars does **not** do yet. Unsupported constructs are
reported as parse errors, never silently mis-run.

## Implemented

- **Control flow.** `if`/`else`, `while`, `do`/`while`, C-style `for`, and the
  ternary `cond ? a : b` (right-associative, result typed by branch promotion).
  `switch (x) { case L: … break; default: }` on `int` and `String`, with
  fall-through between groups and grouped labels (`case 1: case 2:`). `break`
  and `continue`, including the labeled forms (`outer: for (…) { break outer; }`)
  — a labeled `break` exits the named loop/`switch`, a labeled `continue` steps
  the named loop.
- **The enhanced `for`** (`for (String s : arr)`, `for (var v : arr)`) over an
  array — including a `T[][]` row (`for (int[] row : grid)`), an empty array, and
  labeled `break`/`continue`. The array expression is evaluated exactly once, and
  the element type drives `/` typing the same way a declared local does. There
  are no collections yet, so an array is the only thing it can iterate.
- **Standard-library essentials.** `Math.abs`/`max`/`min`/`pow`/`sqrt`/`floor`/
  `ceil`/`round` (with Java's int-vs-double overload result typing),
  `Integer.parseInt` (with radix)/`valueOf`/`toString` (with radix),
  `Long.parseLong`, `Boolean.parseBoolean`, `String.valueOf`, `String.format`
  (a `Formatter` subset: `%d %s %S %f %b %B %x %X %o %c %%`, `%n`, the `-`/`0`/`+`
  flags, width, and `.precision`), and `Arrays.toString` (shallow). Malformed
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
- **Reference arrays.** `new T[n]` (default-valued), `{…}` literals (and
  `new T[]{…}`), get/set indexing (`a[i]`, `a[i] = v`, compound `a[i] += v`,
  `a[i]++`), and `a.length`. Arrays are heap objects (`Value::Obj` handles into a
  host slab in `src/host.rs`), so Java's *reference* semantics hold: pass an array
  to a method, mutate an element, and the caller observes the change. Out-of-range
  indices fault like `ArrayIndexOutOfBoundsException`.
- **Classes and objects.** `class C { fields; C(params){…}; methods }` — instance
  fields (with initializers that run before the constructor), constructors,
  instance methods, `this`, `new C(…)`, field access (`obj.f`, `obj.f = v`), and
  implicit-`this` field/method access. Multiple classes per file, including nested
  `static` classes. Instances are heap objects with reference/aliasing semantics.
- **Instance-method dispatch.** `recv.method(args)` on a user class dispatches to
  the mangled `Class#method#argc` subroutine over fusevm's `Op::Call` frame ABI
  (`this` in slot 0). When a method is overridden in a subclass, dispatch is
  **virtual** — keyed on the receiver's runtime class via a compile-time chain.
- **Inheritance.** `extends`, `super(…)` constructor chaining, inherited fields
  and methods, method overriding, and `instanceof` (respecting the subclass
  chain). `toString()` overrides are honoured by `System.out.println(obj)`, by
  string concatenation (`"x = " + obj`, `s += obj`), and by an explicit
  `obj.toString()`. `String.valueOf(obj)` and `Arrays.toString(objArray)` still
  render the default `Class@hash` form.
- **Interfaces.** `interface I { void m(); }` with abstract and `default`
  methods, `class C implements I, J`, `interface B extends A`, dispatch through
  an interface-typed variable or parameter (virtual on the runtime class),
  `instanceof I` over the full supertype graph, and interface-typed arrays. A
  `default` method may call an abstract method (resolved to the implementor);
  interfaces are not instantiable.
- **Method overloading by parameter type.** Same-name/same-arity overloads
  differing by parameter type resolve at the call site from the static argument
  types, choosing the most-specific applicable overload (identity < numeric
  widening < reference upcast; ambiguity is an error). Applies to `static`
  methods, instance methods (with virtual dispatch keyed on the statically-chosen
  signature), and constructors.
- **Generics (type-erased).** `class Box<T>`, `class Pair<K, V>`, `<T> T id(T x)`,
  bounded `<T extends Number>` / `<T extends Comparable<T>>`, the diamond
  `new Box<>()`, and library type arguments (`List<String>`, `Map<K, V>`) all
  parse and run with type arguments erased at runtime, exactly like `javac`.
- **Exceptions.** `throw <expr>;`, `try`/`catch`/`finally`, multiple `catch`
  arms (first matching type wins), and a `throws` clause (parsed and discarded —
  javars has no checked-exception analysis). The thrown object unwinds real
  fusevm call frames: a `throw` several methods deep lands in the caller's
  handler, and a `finally` runs on both the normal and the exceptional path. The
  `java.lang` throwable hierarchy from `Throwable` down through `Exception`/
  `RuntimeException` to `IllegalArgumentException`, `NumberFormatException`,
  `IllegalStateException`, `ArithmeticException`, `NullPointerException`,
  `ClassCastException`, `UnsupportedOperationException`, and the
  `IndexOutOfBounds` pair is supplied as an implicit prelude (`src/prelude.rs`),
  so `catch (Exception e)` matches a thrown `NumberFormatException`,
  `e.getMessage()` works, and `System.out.println(e)` prints
  `java.lang.Foo: message`. A user class may `extend` any of them. An exception
  no handler claims reports Java's `Exception in thread "main" …` line on stderr
  and exits non-zero.
- **Multi-dimensional arrays.** `new int[m][n]` (rectangular), `new int[m][]`
  (jagged, inner rows `null`), `a[i][j]` read/write, nested array literals
  (`{{1, 2}, {3, 4}}`), and `.length` at each level. Rows are reference arrays,
  so aliasing holds.

## Not implemented (parse errors today)

- **Abstract classes, enums, records.** (Interface abstract methods and
  `abstract` method *signatures* parse; a standalone `abstract class` body is not
  specially modeled.)
- **Most of the standard library.** The `Math`/`Integer`/`Long`/`Boolean`/
  `String`/`Arrays` statics listed above and the `String` instance methods are
  the whole library surface — no `java.util.*` collections, no `Math` constants
  (`Math.PI`), no boxed-type methods beyond the listed statics.
- **`return <value>` from `main`.** `main` is `void`; only a bare `return;`
  (which ends the program) is accepted there. Value returns work in methods.
- **Arrow `switch` expressions**, `switch` on enums/patterns.
- **Try-with-resources** (`try (var r = …)`) and **multi-catch**
  (`catch (A | B e)`) — both are rejected, not mis-parsed.
- **A `return`/`break`/`continue` that leaves a `try` with a `finally`.**
  javars would take the jump without running the `finally`, so the program is
  rejected at compile time instead. Without a `finally`, all three work.
- **Catching a runtime fault.** `catch` only sees objects a `throw` raised.
  javars's own faults — an out-of-range array index, a `NumberFormatException`
  from `Integer.parseInt`, integer division by zero — still abort the program
  with a `javars:` message instead of being catchable exceptions.
- **Lambdas, streams, `var` type inference beyond storage.**

## Modeled with a documented simplification

- **Types are not checked.** Declared types (`int`, `String`, …) are retained for
  diagnostics and for overload resolution / `/`-truncation typing, but do not gate
  execution — the runtime is dynamically typed on the fusevm value model.
  Definite-assignment and type errors that `javac` would reject may run.
- **No widening *value* conversion.** Overload *resolution* uses static types
  (so `f(int)` vs `f(double)` picks correctly), but the argument value is not
  coerced: an `int` bound to a `double` parameter (or `double d = 7;`) keeps its
  integer value, so it prints `7`, not `7.0`. A `char` argument is a one-character
  string (see below), so it selects a `String` overload rather than an `int` one.
- **`==` on objects is reference identity; on strings it compares by value.**
  Object and array handles (`Value::Obj`) are identity-comparable, so `x == y`,
  `x == z` after `z = x`, and `obj.field == null` all behave like Java's reference
  `==`. String `==`, however, compares by *value* (Java's is identity) — this
  matches the far more common intent and avoids surprising `"ab" == "a"+"b"`.
- **`char` literals are one-character strings**, not an integer `char` type.
- **Floating `/` routes through a builtin.** Statically-integral division keeps
  the native op pair (`Div` + `TruncInt`) so the JIT can trace it; a floating or
  statically-unknown operand routes through `JDIV`, because Java floating
  division is IEEE-754 (`x / 0.0` is a signed infinity, `0.0 / 0.0` is NaN)
  where the native op yields `Undef`.
- **32-bit `int` wrapping needs a statically known `int` type.** An arithmetic
  operation whose operands are *statically* `int` (or the `byte`/`short` that
  promote to it) wraps at 32 bits exactly like Java — literals, `int` locals,
  parameters, fields, array elements, `int`-returning methods, `Integer.parseInt`,
  and `Math.abs`/`max`/`min` of `int` arguments all qualify, and so do the
  compound forms (`x *= k`, `x++`, `a[i] += k`, `obj.f -= k`). `long` stays
  64-bit. When an operand's static type cannot be determined the operation keeps
  fusevm's 64-bit result, so an overflow there still differs from Java. The wrap
  is a native `Shl 32; Shr 32` pair rather than a builtin, so hot integer loops
  keep their JIT trace.
- **Uninitialized locals are unbound** rather than rejected; reading one before
  assignment yields `null` instead of a compile error.
