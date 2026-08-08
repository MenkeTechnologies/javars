# Known gaps

An honest list of what javars does **not** do yet. Every unsupported construct
is reported as a parse or compile error rather than being run with the wrong
meaning; the handful of places where an *accepted* construct computes something
other than Java are all named under "Modeled with a documented simplification"
at the bottom, and are summarized in the section right after this one.

## Implemented

- **Control flow.** `if`/`else`, `while`, `do`/`while`, C-style `for`, and the
  ternary `cond ? a : b` (right-associative, result typed by branch promotion).
  `switch (x) { case L: … break; default: }` on `int` and `String`, with
  fall-through between groups and grouped labels (`case 1: case 2:`). `break`
  and `continue`, including the labeled forms (`outer: for (…) { break outer; }`)
  — a labeled `break` exits the named loop/`switch`, a labeled `continue` steps
  the named loop. A `for` header takes comma-separated init and update clauses
  (`for (int i = 0, j = n; i < j; i++, j--)`), where the init's later declarators
  reuse the first one's type.
- **The enhanced `for`** (`for (String s : arr)`, `for (var v : arr)`) over an
  array or a collection — including a `T[][]` row (`for (int[] row : grid)`), an
  empty array, a `List`/`Set`/`keySet()`/`values()`, and labeled
  `break`/`continue`. The iterable expression is evaluated exactly once, and the
  element type drives `/` typing the same way a declared local does. An array
  iterable emits exactly the ops it always did; a collection is snapshotted into
  an array first, which is also what makes the loop safe against a body that
  mutates the collection.
- **The full operator set.** Arithmetic, comparison, and the short-circuiting
  `&&`/`||`, plus the bitwise `&`/`|`/`^`/`~` (which are Java's
  *non-short-circuiting logical* operators on `boolean` operands) and the shifts
  `<<`/`>>`/`>>>`. A shift masks its distance to the **left** operand's width — 5
  bits for `int`, 6 for `long`, so `1 << 33` is `1 << 1` — and only that operand
  is promoted, so `1 << 2L` is still an `int`. `>>>` zero-fills at the operand's
  width. Every compound form (`&=`, `|=`, `^=`, `<<=`, `>>=`, `>>>=`) applies the
  same rules. `>>` lexes as one token, and the generic-argument skippers weigh a
  closer by how many `>` it spells, so `List<List<String>>` still parses.
- **`++`/`--` in value position.** The post-form evaluates to the value the
  variable held, the pre-form to the value it takes.
- **Cast expressions** (`(int) d`, `(byte) n`, `(char) 65`, `(Object) x`). Java's
  narrowing primitive conversions are real value changes: floating → integral
  *saturates* (`(int) 1e18` is `Integer.MAX_VALUE`) and truncates toward zero,
  and the integral narrowings are two's-complement. Widening and identity casts
  emit the operand alone, so `(int) i` stays native.
- **The whole integer-literal syntax.** Decimal, hex (`0x1F`), binary
  (`0b1010`), octal (`017`), and `_` digit separators. Hex and binary are read as
  a *bit pattern* at the literal's width, so `0xFFFFFFFF` is the `int` -1 and
  `0xFFFFFFFFFFFFFFFFL` is the `long` -1.
- **Standard-library essentials.** `Math.abs`/`max`/`min`/`pow`/`sqrt`/`floor`/
  `ceil`/`round`/`signum`/`floorDiv`/`floorMod`/`toRadians`/`toDegrees` (with
  Java's int-vs-double overload result typing) and the `Math.PI`/`Math.E`
  constants; the `Integer`/`Long`/`Short`/`Byte`/`Double` `MAX_VALUE`/`MIN_VALUE`
  constants plus `Double.NaN` and the infinities; `Integer.parseInt` (with
  radix)/`valueOf`/`toString` (with radix)/`toBinaryString`/`toHexString`/
  `toOctalString`/`compare`/`max`/`min`/`sum`/`signum` and the `Long`
  equivalents; `Double.parseDouble`/`valueOf`/`toString`/`compare`/`isNaN`/
  `isInfinite`; `Boolean.parseBoolean`/`toString`/`compare`; the `Character`
  predicates and case conversions; `String.valueOf`/`join`/`format`; and
  `Arrays.toString`/`deepToString`/`sort`/`fill`/`equals`/`copyOf`/
  `copyOfRange`/`binarySearch`/`hashCode`. `String.format` covers `%d %s %S %f
  %e %E %g %G %b %B %h %H %x %X %o %c %%` and `%n`, the `-`/`0`/`+`/`,`/`(`
  flags, width, `.precision`, and explicit argument indexes (`%2$s`); `%f` rounds
  HALF_UP on the double's exact value, as Java's `Formatter` does.
  `System.out.printf` (and the `System.err` form) is that same formatter with no
  trailing newline. Malformed numeric input faults like `NumberFormatException`;
  an unregistered static method is an error. `System.err.print[ln]` writes to
  stderr.
- **User-defined `static` methods.** `static <ret> name(<params>) { … }` lowers
  to fusevm's native `Op::Call` frame ABI — parameters and locals live in call-
  frame slots, so recursion, mutual recursion, and forward references all work.
  `return <expr>;` returns a value; `void` methods return `null` on fall-off.
  Arity is checked at compile time.
- **`String` instance methods.** `recv.method(args)` dispatches on `String`
  receivers through the host: `length`, `isEmpty`, `charAt`, `substring`,
  `indexOf`, `contains`, `equals`, `equalsIgnoreCase`, `toUpperCase`,
  `toLowerCase`, `trim`, `startsWith`, `endsWith`, `concat`, `replace`,
  `repeat`, `indexOf(t, from)`, `lastIndexOf`, `codePointAt`, `strip`,
  `stripLeading`, `stripTrailing`, `isBlank`, `hashCode`, `intern`,
  `contentEquals`, `toCharArray`, `formatted`, and the literal-pattern
  `split`/`replaceAll`/`replaceFirst`/`matches` (see the regex entry below).
  `x.getClass()` evaluates to the runtime class *name*, over which `getName`
  and `getSimpleName` are `String` methods.
  Index/length semantics use Unicode scalar (`char`) positions — exact
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
- **`static` fields and `static { }` blocks.** A `static` field is one cell per
  class, stored in a compiler-minted global (`#static#C#n`), seeded with its
  declared type's default before any user code runs and then initialized — field
  initializers and `static { … }` blocks together, in textual order — ahead of
  `main`. Read and written unqualified inside the declaring class (from `main`,
  a `static` method, an instance method, or a constructor), qualified as `C.n`
  from anywhere, and through an inheriting class (`Sub.n` names the base's cell).
  Plain, compound (`C.n += 3`), and `++`/`--` forms all write the one cell, and
  the field's declared type drives `/`-truncation and the 32-bit `int` wrap the
  same way a local's does. `C.staticMethod(args)` resolves too.
- **`main`'s `args`.** `public static void main(String[] args)` binds `args` to
  the real program arguments — `java Prog.java a b` gives a length-2 `String[]`
  — and to a zero-length array (never `null`) when none are passed. The `String...`
  varargs spelling is accepted, and a `main()` with no parameter is left alone.
- **`record` types.** `record Pt(int x, int y) { … }` — the components become
  final instance fields plus the canonical constructor, one accessor each
  (`p.x()`), a `toString()` in Java's `Pt[x=1, y=2]` form, and a component-wise
  `equals`. A compact constructor (`Pt { if (…) throw …; }`) runs its validation
  before the fields are assigned. The body may add methods, `static` members, and
  `implements`; anything it declares itself (its own `toString()`, its own
  accessor) wins over the derived member. Records nest inside a class or stand at
  top level, and `record` stays usable as an ordinary identifier.
- **Abstract classes.** `abstract class Shape { abstract double area(); … }` with
  a constructor chained by `super(…)`, concrete methods that call the abstract
  one (resolved to the subclass's override), and dispatch through a variable,
  parameter, or array of the abstract type.
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
  arms (first matching type wins) including the multi-catch `catch (A | B e)`
  (whose alternatives are tested in order against the throwable's class), and a
  `throws` clause (parsed and discarded —
  javars has no checked-exception analysis). The thrown object unwinds real
  fusevm call frames: a `throw` several methods deep lands in the caller's
  handler, and a `finally` runs on both the normal and the exceptional path. The
  `java.lang` throwable hierarchy from `Throwable` down through `Exception`/
  `RuntimeException` to `IllegalArgumentException`, `NumberFormatException`,
  `IllegalStateException`, `ArithmeticException`, `NullPointerException`,
  `ClassCastException`, `UnsupportedOperationException`,
  `NegativeArraySizeException`, and the `IndexOutOfBounds` pair is supplied as an
  implicit prelude (`src/prelude.rs`), so `catch (Exception e)` matches a thrown
  `NumberFormatException`, `e.getMessage()` works, and `System.out.println(e)`
  prints `java.lang.Foo: message`. A user class may `extend` any of them. An
  exception no handler claims reports Java's `Exception in thread "main" …` line
  on stderr and exits non-zero.
- **Runtime faults are catchable exceptions.** javars's own faults raise the
  throwable Java raises, carrying Java's exact detail message, so they are
  caught, typed, and re-thrown like any other: an out-of-range array index
  (`ArrayIndexOutOfBoundsException: Index 5 out of bounds for length 3`), a null
  array/receiver (`NullPointerException`), `Integer.parseInt`/`valueOf`/
  `Long.parseLong` on malformed, whitespace-padded, out-of-range, or bad-radix
  input (`NumberFormatException: For input string: "abc"`, `… under radix 16`),
  integral `/ 0` and `% 0` (`ArithmeticException: / by zero`), a negative array
  size (`NegativeArraySizeException: -1`), `String.charAt`/`substring` out of
  range (`StringIndexOutOfBoundsException: Range [2, 9) out of bounds for length
  3`), and `String.repeat` with a negative count (`IllegalArgumentException:
  count is negative: -1`). A fault raised inside a called method unwinds to the
  caller's handler like a `throw`; one no handler claims still prints Java's
  uncaught line and exits non-zero. The `/ 0` check is emitted inline (`Dup`,
  compare, branch) and is skipped entirely for a literal non-zero divisor, so
  constant-divisor loops keep the bare native op pair and stay JIT-traceable.
- **A jump out of a `try` runs its `finally` first.** `return`, `break`,
  `continue`, and their labeled forms emit every cleanup block they leave,
  innermost first, before taking the jump — including a `break`/`continue` that
  crosses several `try`s inside the targeted loop. The returned value is fixed
  before the cleanup runs (a `finally` that reassigns the variable cannot change
  it), and a `return` inside a `finally` replaces the pending one, both like
  Java. An exception raised inside a `catch` arm also runs the `finally` on its
  way out to the enclosing handler.
- **Try-with-resources.** `try (T r = e; U s = f) { … }` — with or without
  `catch`/`finally` arms, and the Java 9 bare-name form `try (existing)`.
  Desugared into the nested `try`/`finally` shape Java specifies it as, so
  resources close in reverse declaration order, close before any `catch`/
  `finally` of the outer statement runs, and close on the exceptional path and on
  a `return` out of the block. `close()` is called on the declared type through
  ordinary dispatch; javars has no `java.lang.AutoCloseable`, so implementing it
  is optional (an unknown interface name is inert).
- **`enum` types.** `enum Color { RED, GREEN, BLUE }`, with or without a body of
  its own (`; int rank() { … }`), `implements I`, and an empty constant list.
  Each constant is a singleton instance built before `main` runs and held in a
  compiler-minted global, so `Color.RED == Color.RED` is reference identity,
  `instanceof` works, and an enum-typed array or parameter is an ordinary
  reference. `name()`, `ordinal()`, `toString()` (defaulting to `name()`), and
  `equals` are synthesized unless the body declares them; `values()` returns a
  fresh array in declaration order and `valueOf(s)` raises Java's
  `IllegalArgumentException: No enum constant Color.PINK` on a miss.
  `switch (c) { case RED: … }` takes the unqualified label, and a bare constant
  name resolves inside the enum's own body (`this == MUL`).
- **Enum constants with state and bodies.** `EARTH(5.97e24)` runs the enum's own
  constructor with those arguments, so each constant keeps its own field values.
  A constant with a body (`PLUS { int apply(int a, int b) { … } }`) is compiled
  to a real synthetic subclass of the enum, which is exactly what Java specifies
  it as — so its overrides are reached by the ordinary runtime-class virtual
  dispatch, an `abstract` method on the enum resolves to the per-constant body,
  and a constant that declares no override inherits the enum's own.
- **Lambdas and functional interfaces.** `() -> e`, `x -> e`, `(a, b) -> { … }`,
  and explicitly-typed `(int a, String b) -> …`. A lambda outlives the frame it
  was written in, so it compiles to a heap closure (`HostObj::Closure`) carrying
  a **by-value snapshot** of the enclosing locals plus `this`; the body is an
  ordinary subroutine invoked in its own fusevm call frame. Java only lets a
  lambda read effectively-final locals, so the snapshot is observationally
  exact — and it is what gives the enhanced `for` Java's per-iteration capture.
  The target is **any interface with exactly one abstract method**, which is
  Java's own rule, so a user-declared `interface Calc { int of(int a); }` needs
  no registration; `Runnable`, `Callable`, `Supplier`, `Consumer`/`BiConsumer`,
  `Function`/`BiFunction`, `UnaryOperator`/`BinaryOperator`,
  `Predicate`/`BiPredicate`, `Comparator`, and the `Int*`/`To*Function` shapes
  are supplied the same way, as one-method interfaces in `src/prelude.rs`. A
  variable of a functional-interface type may hold a lambda *or* a class
  instance: the runtime-class dispatch chain gains an arm for the closure, so
  both work through the same call site. The interface's declared parameter
  types travel into the body (from a local's declared type, a parameter type, or
  a method's return type), so `Calc c = (a, b) -> a / b;` truncates and
  `(a, b) -> a * b` wraps at 32 bits exactly where `int` says it should. `return`
  inside a lambda returns from the *lambda*; `break`/`continue`, `try`/`finally`,
  and a `throw` all work inside one, and an exception it raises unwinds to the
  caller's handler.
- **Method references.** `String::length` and `Integer::parseInt` (unbound
  receiver / stdlib static), `Point::area` (unbound instance), `obj::method` and
  `this::method` (bound — the receiver is captured), `Point::new`, and
  `System.out::println`. Java infers the reference's arity from its *target*
  type; javars has no target-typing pass, so the arity comes from the referenced
  member's own declaration — which resolves every unambiguous form and rejects
  an overloaded name with a diagnostic rather than guessing an overload.
- **`final` on a local or enhanced-`for` variable.** Parsed and dropped: it
  constrains reassignment, which `javac` has already checked.
- **`java.util` collections.** `List`/`ArrayList`/`LinkedList`,
  `Map`/`HashMap`/`LinkedHashMap`/`TreeMap`, `Set`/`HashSet`/`LinkedHashSet`/
  `TreeSet`, the copy constructors (`new ArrayList<>(other)`), `Arrays.asList`,
  `List.of`/`Set.of`, and `Collections.sort`/`reverse`/`max`/`min`. Collections
  are heap objects like arrays and instances, so passing one to a method and
  mutating it is visible to the caller. The enhanced `for` iterates them (the
  compiler routes a non-array iterable through a snapshot builtin, so an array
  loop still emits exactly the ops it did before), `keySet()`/`values()` are
  views in the map's own order, and `sort`/`forEach` take a lambda.
  **`HashMap`/`HashSet` iterate in Java's real bucket order**, not insertion
  order: Java indexes a power-of-two table with `(capacity - 1) & (h ^ (h >>> 16))`,
  appends within a bucket, and preserves relative order across a resize, so the
  order is a stable sort of the insertion sequence by bucket index — reproduced
  exactly, and checked against OpenJDK for `String` and `Integer` keys including
  across the resize at 13 entries. `LinkedHashMap`/`LinkedHashSet` keep insertion
  order and `TreeMap`/`TreeSet` sort, each because Java does, not by default.
  `Arrays.asList` is fixed-size and `List.of` immutable, so a structural write
  to either throws `UnsupportedOperationException` exactly as Java's does, and an
  out-of-range `get` throws `IndexOutOfBoundsException` with Java's message.
- **`var` type inference.** A `var` local records the *type* it infers, not just
  the value: `var i = 7; i / 2` truncates, `var big = 100000; big * big` wraps at
  32 bits, `var p = new Pt(9, 4); p.sum()` dispatches, and `for (var v : arr)`
  takes the array's element type (which `new int[]{…}` and `new int[][]{…}` carry
  along for exactly this reason). `var` in a C-style `for` init, over a jagged
  array, and over a collection all work.
- **`long` literals.** An `L`-suffixed literal is typed `long` and so is exempt
  from the 32-bit `int` wrap — `3000000000L + 3000000000L` is `6000000000`, not
  `1705032704`.
- **Arrow `switch`, as an expression and as a statement.**
  `int r = switch (x) { case 1, 2 -> 10; default -> { … yield v; } };` and the
  statement form `switch (x) { case 1 -> …; default -> …; }`, which is the same
  construct with its value discarded — one parser, one lowering. Arms do not
  fall through and exactly one runs, so it lowers to a `?:`-style chain rather
  than the classic form's laid-out group bodies: multi-label arms
  (`case A, B ->`), an `enum` discriminant with unqualified labels, a `String`
  or `int` discriminant, a `default` written anywhere among the arms (it is laid
  out last, so an arm after it still matches), a `throw` arm, and a block arm
  whose `yield` supplies the value — running any `finally` it leaves on the way
  out, exactly as `return` does. `yield` is a keyword only inside an arm body,
  so a variable or method named `yield` still works. A switch *expression* also
  accepts the classic `case X:` arm — every such arm must complete with `yield`
  or `throw`, so only an empty one falls through, and that just groups its labels
  onto the next arm. The classic colon form as a *statement*, with its
  fall-through, is untouched.
- **`String.compareTo` / `compareToIgnoreCase`**, returning Java's *difference*
  (the first differing `char`, else the length difference) rather than only its
  sign — programs print the number, so the sign alone would be wrong.
- **Multi-dimensional arrays.** `new int[m][n]` (rectangular), `new int[m][]`
  (jagged, inner rows `null`), `a[i][j]` read/write, nested array literals
  (`{{1, 2}, {3, 4}}`), and `.length` at each level. Rows are reference arrays,
  so aliasing holds.

## Runs wrong rather than being rejected

No construct is *silently* mis-run: every case where javars computes something
other than Java is named in "Modeled with a documented simplification" below,
and each is a deliberate, bounded model rather than a bug found and left.

Some of those simplifications do print a different answer for a program `javac`
accepts, and it is worth naming which — they are all missing *conversions*
javars's untyped runtime never performs, not wrong arithmetic:

- `double d = 7;` prints `7`, not `7.0` (no widening value conversion).
- `s.get() / 2` on a `Supplier<Integer>` prints `3.5`, not `3` (the erased
  interface returns `Object`, so javars cannot type the result as `int`).
- `int` arithmetic whose operand types are not statically known keeps fusevm's
  64-bit result rather than wrapping at 32 bits.

Everything else javars accepts runs with Java's meaning, and the differential
fuzzer (`parity-fuzz`) generates none of the above precisely because they are
known.

## Not implemented (parse or compile errors today)

- **Streams** (`.stream()`, `map`/`filter`/`collect`), and the `default` methods
  the JDK's functional interfaces carry (`Function.andThen`/`compose`,
  `Predicate.negate`/`and`/`or`, `Comparator.reversed`/`comparing`). The
  interfaces javars supplies declare only their single abstract method, so
  calling a `default` one is a compile error, not a wrong answer. Lambdas and
  method references themselves are implemented (above).
- **`hashCode()`**, including a `record`'s derived one. A record supplies its
  accessors, `toString`, and `equals`; calling `hashCode()` is a compile error
  ("class `Pt` has no method `hashCode`") rather than a wrong number.
- **Most of the standard library.** The `Math`/`Integer`/`Long`/`Double`/
  `Boolean`/`Character`/`String`/`Arrays`/`Collections` statics listed above, the
  `String` instance methods, and the `java.util` collections are the whole
  library surface — no boxed-type methods beyond the listed statics, no
  `Iterator`/`entrySet`/`Deque`/`Queue`/`Optional`, no I/O.
- **`Math`'s transcendentals** (`sin`, `cos`, `tan`, `asin`, `acos`, `atan`,
  `atan2`, `exp`, `log`, `log10`, `cbrt`, `hypot`, `sinh`/`cosh`/`tanh`). The JDK
  answers these from its own fdlibm-derived implementation and permits a 1-ulp
  error; Rust's libm does not reproduce it bit-for-bit. A 180-value differential
  sweep against OpenJDK 26 diverged in the last digit for every one of them
  (`sin` 14/180, `cbrt` 25/180, `tan` 5/10), so they are left out: an
  unregistered static is a clear error, a silently different last digit is not.
  `sqrt`, `pow`, `abs`, `floor`, `ceil`, `round`, `max`, `min`, `signum`,
  `floorDiv`, `floorMod`, `toRadians`, and `toDegrees` are exact and supported,
  as are the `Math.PI`/`Math.E` constants.
- **Regular expressions.** `String.split`/`replaceAll`/`replaceFirst`/`matches`
  accept only a pattern with no metacharacter (`\ . [ ] { } ( ) * + ? ^ $ |`),
  which is the single-separator call the JDK itself fast-paths. A pattern that
  would need real matching is reported ("supports only a literal pattern")
  rather than treated as a literal and answered wrong. javars links no regex
  engine.
- **`return <value>` from `main`.** `main` is `void`; only a bare `return;`
  (which ends the program) is accepted there. Value returns work in methods.
- **`switch` *patterns*** (`case Integer i ->`, `case null`, guarded
  `when` clauses). The arrow form itself is implemented (above); it is pattern
  labels that are not.
- **`Enum.compareTo`, `EnumSet`, `EnumMap`.**
- **Sealed types, inner (non-`static`) classes,** and **anonymous classes** other
  than the enum-constant body form.
- **Fully-qualified type names in *declaration* position**
  (`java.util.function.Supplier<T> s`). An `import` line is skipped, and the
  simple name is what resolves. A qualified *static call* does work
  (`java.util.Arrays.sort(x)`), because the package prefix is dropped and the
  simple name dispatches.

## Modeled with a documented simplification

- **Types are not checked.** Declared types (`int`, `String`, …) are retained for
  diagnostics and for overload resolution / `/`-truncation typing, but do not gate
  execution — the runtime is dynamically typed on the fusevm value model.
  Definite-assignment and type errors that `javac` would reject may run.
- **Class initialization is eager, not lazy.** Java initializes a class the
  first time it is used; javars runs *every* class's `static` field initializers
  and `static { … }` blocks once, before `main`, in source-declaration order
  (after seeding every static with its type's default, and after building the
  enum constants). The two differ only when one class's initializer reads
  another's static: Java would force that class's initialization first, javars
  gives whatever the declaration order already produced (the default value if
  the other class is declared later). A `static` initializer with an observable
  side effect (printing) also runs in a program that never touches its class.
- **A `record`'s `equals` compares components with javars's `==`.** Java's
  derived `equals` uses `Objects.equals` for a reference component; javars emits
  `==`, which is value equality for a `String` component (so those agree) but
  reference identity for a user-class or array component (where Java would call
  the component's own `equals`).
- **An unqualified name that is another class's `static` field is unbound**
  rather than a compile error. `javac` rejects reading `v` from outside the
  class declaring `static int v`; javars resolves an unqualified static only
  against the enclosing class and its ancestors, and an unresolved name reads as
  `null` (the same behaviour as an uninitialized local, below). `C.v` is the
  spelling that works, and it is the only one valid Java uses.
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
  A consequence: `"abc".charAt(2) + 1` concatenates to `c1` where Java promotes
  the `char` to its code point and prints `100`. `(int) c` and `(char) n` do
  convert, so the code point is reachable explicitly.
- **A reference cast is a no-op, so there is no `ClassCastException`.** The host
  heap already carries each object's class and javars does not box primitives, so
  `(Integer) o` on a `String` has no representation to change and no boxed type
  to check — it passes the value through where Java throws. A *primitive* cast is
  a real conversion (above).
- **`float` shares `double`'s width.** There is no 32-bit float representation,
  so `(float) x` only makes an integral operand floating and `1.0f / 3.0f` prints
  `0.3333333333333333` where Java prints `0.33333334`.
- **`Double.toString` of a subnormal can differ in the last digit.** javars
  renders `Double.MIN_VALUE` as `5.0E-324` where Java prints `4.9E-324`: Java's
  shortest-round-trip algorithm and Rust's disagree at the very bottom of the
  exponent range. Normal-range doubles agree.
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
- **`NullPointerException` detail messages drop the provenance clause.** Java's
  "helpful NPE" names the *bytecode local slot* of the null reference — `Cannot
  read field "x" because "<local4>" is null` — which javars cannot reproduce: it
  has no `javac` slot numbering. javars keeps the operation half of the wording
  and ends there (`Cannot read field "x" because the receiver is null`). The
  exception *class* is right, so `catch (NullPointerException e)` behaves
  identically; only `e.getMessage()`/`e.toString()` text differs. A method call
  on a null user-class receiver reports the first field access that fails inside
  the callee rather than Java's `Cannot invoke "P.get()"`.
- **A throwing `close()` replaces the body's exception rather than being
  suppressed.** Java records it via `Throwable.addSuppressed`; javars has no
  suppression list, so the later exception wins.
- **A lambda's own `toString()` is a marker, not Java's.** Java renders one as
  `Class$$Lambda/0x…@<identity hash>`, which is neither reproducible nor stable
  across JVM runs; javars prints `<lambda>@<handle>`. Printing a lambda is not
  something a deterministic program does, so this only shows up if you ask for it.
- **An element's `toString()` override is not called inside a collection.**
  Printing a `List<Pt>` of records gives `[Pt@2]` where Java gives
  `[Pt[x=1, y=2]]`, because rendering happens in a host builtin that cannot
  re-enter the VM to run the element's Java-level `toString()`. This is the same
  limitation `Arrays.toString(objArray)` and `String.valueOf(obj)` already have
  (above); an `enum` constant is the exception, since its name is a real field.
- **`Arrays.asList(arr)` always spreads a lone array argument.** Java's varargs
  spreads a *reference* array (`String[]` → a 3-element list) but not a
  primitive one (`int[]` → a 1-element `List<int[]>`); javars erases element
  types, so it cannot tell them apart and spreads both. The reference case —
  the one programs actually write — is exact.
- **A lambda's result type is its interface's erasure.** `Supplier<Integer> s`
  declares `Object get()` after erasure — exactly what `javac` compiles it to —
  so javars cannot statically type `s.get()` as `int` and `s.get() / 2` does not
  truncate. Java recovers the type from the generic signature and inserts a
  checked cast; javars does not model generic signatures (see the type-erasure
  entry above). A lambda's *parameters* are typed, because those come from the
  interface's own declaration rather than from a type argument.
