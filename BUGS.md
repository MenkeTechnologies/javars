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
  (`for (int i = 0, j = n; i < j; i++, j--)`), the init's declarators sharing one
  type on the rule in the next entry. `synchronized (m) { … }` is a statement too: javars
  runs one thread, so the monitor is unobservable, but the expression is
  evaluated exactly once and a `null` monitor throws before the body.
- **Declarations with more than one declarator.** `int a = 1, b = 2;` in
  statement position, in a `for` init clause, and as an instance or `static`
  field. Declarators run left to right, so a later initializer may read an
  earlier name (`int a = 1, b = a + 1;`), and any of them may be left
  uninitialized (`int a, b = 2, c;`); `final` applies to the whole statement. The
  C-style array suffix binds to its own *declarator*, which is what makes
  `int p[] = {1}, q;` an `int[]` and an `int` rather than two arrays; it is
  accepted on locals, fields, and parameters (`int add(int xs[], int n)`).
  `var a = 1, b = 2;` is rejected, because Java forbids the compound form there.
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
  and the integral narrowings are two's-complement — except `(char)`, which is
  *unsigned* 16-bit, so `(char) -1` is 65535. Widening and identity casts emit
  the operand alone, so `(int) i` stays native.
- **Checked reference casts.** `(Cat) animal`, `(Integer) o`, `(Marker) x` verify
  the receiver's *runtime* class against the same supertype graph `instanceof`
  walks, and throw `ClassCastException` when it does not fit — `class Dog cannot
  be cast to class Cat`, or Java's full module-and-loader wording when both types
  are `java.lang` ones. `null` casts to anything, `(Object) x` always passes, and
  a cast does not erase what the operand is (`println((Object) dog)` still finds
  `Dog`'s `toString`). The wrapper types javars's value model tells apart
  (`String`, `Integer`, `Double`, `Boolean`, `Number`, `CharSequence`) are checked
  too; the ones it cannot (`int` and `long` are one integer here) are allowed
  rather than guessed at.
- **`char` arithmetic.** A `char` is Java's 16-bit integral type, not a string:
  `"abc".charAt(2) + 1` is 100, `'z' - 'a'` is 25, and `c - '0'` reads a digit.
  It runs as its code point and takes Java's *string conversion* (JLS 5.1.11)
  back to the one-character String at every boundary where Java applies one —
  `println(c)`, `"x" + c`, `String.valueOf`/`format`/`join`, a `String`-method
  argument (`indexOf('l')`, `replace('l', 'L')`), and boxing into a collection.
  A `char[]` (from `toCharArray()`, a `{'a', 'b'}` literal, or `Arrays.copyOf`)
  holds code points, so its elements do arithmetic and `Arrays.sort` orders them
  numerically, while `Arrays.toString`, `new String(cs)`, and `String.valueOf(cs)`
  still render characters. `Character.toUpperCase`/`toLowerCase` return a `char`
  (Java's one-to-one code-point map, so `ß` is left alone), and the predicates
  take one. `switch` on a `char`, `char` fields/parameters/returns, and the
  conditional's constant rule (`flag ? 'a' : 98` stays a `char`, JLS 15.25) all
  follow.
- **Regular expressions.** `String.split`/`replaceAll`/`replaceFirst`/`matches`
  run real `java.util.regex` patterns on `fancy-regex` (`src/regex.rs`), whose
  backtracking VM is what makes Java's backreferences (`(a)\1`) and lookaround
  (`(?<=a)b`) expressible at all — the linear-time `regex` crate rejects both by
  construction. Java's *defaults* are not the engine's, and every place they
  differ is rewritten rather than documented, because each would otherwise be a
  silently different answer: `\d`/`\w`/`\s`/`\b` are ASCII-only in Java
  (`"١٢٣".replaceAll("\\d", ".")` changes nothing) where the engine's are
  Unicode-aware; `(?i)` folds ASCII only (`"Ä".matches("(?i)ä")` is false), so it
  is expanded into the pattern and the engine's own flag dropped; `.` excludes
  all five of Java's line terminators, not just `\n`; and a default-mode `$` also
  matches *before* a final terminator, so `"abc\n".replaceAll("c$", "X")`
  replaces. `\Q…\E`, `\h`/`\v`/`\R`, `\Z`, `\uHHHH`, and the POSIX `\p{Alpha}`
  names are translated too. On top of the engine, the three methods' specified
  behaviour: `split`'s trailing-empty removal, its leading zero-width-match rule,
  and its `limit`; the `$n`/`${name}` replacement grammar with Java's `\`
  escaping (which is not the `regex` crate's); and `matches`'s whole-region
  anchoring. A malformed pattern raises `java.util.regex.PatternSyntaxException`
  and a replacement naming an absent group raises `IndexOutOfBoundsException`,
  both catchable.
- **32-bit `float`.** `float` is Java's 32-bit type, not an alias for `double`:
  `1.0f / 3.0f` is `0.33333334`, `(float) 0.1` is not `0.1`, and `(double) 0.1f`
  is `0.10000000149011612`. fusevm has one floating representation, so a `float`
  is a `double` *kept* at 32-bit precision — the same per-site discipline the
  32-bit `int` wrap uses, one width down. Java rounds a `float` operation **once**
  at 32 bits, and computing it in 64 bits and narrowing afterwards rounds twice
  and can land a ulp away (`16777217.0f * 0.2f` is 3355443.2 rounded once and
  3355443.3 rounded twice), so a `float` operation runs on the host rather than as
  a native op with a narrowing appended — the only Java arithmetic in javars that
  does. Literals, locals, fields, parameters, returns, `float[]` elements,
  compound assignment, and `++`/`--` all carry the width; mixing in a `double`
  promotes the operation and mixing in an `int` does not. `Float.toString` is the
  shortest decimal that round-trips at **32** bits, selected by Java's rule
  (closest to the value, ties to the even last digit — Rust's formatter breaks
  that tie the other way), and it is emitted wherever a statically-`float` value
  crosses into a String. `Float.parseFloat`/`valueOf`/`toString`/`compare`/
  `isNaN`/`isInfinite` and the `Float.MAX_VALUE`/`MIN_VALUE`/`MIN_NORMAL`/`NaN`/
  infinity constants answer at 32 bits too.
- **`toString`'s two-digit widening at the subnormal floor.** `Double.toString`
  and `Float.toString` are not "shortest decimal that round-trips". The
  specification restricts the candidates to the minimal length only when that
  length is at least 2; when the shortest form has a *single* digit the
  candidates are the decimals of length 1 **or 2**, and the one nearest the value
  wins (ties to even). Across the whole normal range the two rules coincide — a
  normal's binary ulp is some sixteen decimal orders below the value, so the
  nearest two-digit decimal is always the one-digit answer with a `0` appended,
  which canonicalizes straight back. At the bottom of the exponent range the
  binary ulp is the size of the value itself and they part: `Double.MIN_VALUE` is
  4.9406…E-324, which `5.0E-324` does round-trip to but `4.9E-324` is nearer, and
  the same for `Float.MIN_VALUE` (`1.4E-45`, not `1.0E-45`) and for every
  subnormal whose shortest form is one digit. Deciding it needs the value's
  *exact* decimal expansion, because `10^exp` is not itself representable down
  there — the arithmetic `v / 10^(exp-1)` underflows to zero.
- **`%e` and `%g` round HALF_UP.** Java's `Formatter` rounds through
  `BigDecimal.ROUND_HALF_UP`, so `%e` of 5592405.5 is `5.592406e+06` where
  half-to-even gives `5.592405e+06`. `%f` already did this; the scientific and
  general conversions now take their digits from the value's own decimal
  expansion for the same reason — scaling by a power of ten first would round the
  tie away before it could be seen.
- **Fully-qualified type names.** `java.util.List<String> l`,
  `new java.util.ArrayList<>()`, `static java.lang.String greet(java.lang.String
  who)`, a qualified field type, `catch (java.util.regex.PatternSyntaxException
  e)`, `java.lang.System.out.println(…)`, and the qualified *static call*
  (`java.util.Arrays.sort(x)`) all resolve to the simple name — javars keys every
  type on it, and an `import` line is skipped for the same reason. A package
  segment is recognised by starting lowercase and being followed by another name,
  which is what keeps the identical `outer.next.n = 9` shape an expression.
- **A compound assignment narrows back to its target's width.** JLS 15.26.2
  makes `b += 100` on a `byte` mean `b = (byte) (b + 100)`, so it overflows at
  the *target's* width, not at `int`'s: `byte` and `short` sign-extend (`-56`,
  `-32768`), `char` masks to 16 unsigned bits (`65535` + 1 is 0). Applies to
  every compound operator and to `++`/`--`, on a local, a field, a `static`, and
  an array element.
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
  `contentEquals`, `toCharArray`, `formatted`, and the four
  `java.util.regex` methods `split`/`replaceAll`/`replaceFirst`/`matches` (see
  the regular-expression entry below).
  `x.getClass()` evaluates to the runtime class *name*, over which `getName`
  and `getSimpleName` are `String` methods.
  Index/length semantics use Unicode scalar positions — exact for ASCII/BMP,
  one unit per astral character (which Java counts as two UTF-16 units).
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
- **`java.lang.Object`.** `new Object()` allocates the fieldless root instance,
  with a distinct identity per allocation — so it works as a lock, as a sentinel
  compared with `==`, and as a `HashMap` key or `HashSet` element (two of them
  occupy two slots). The methods a class inherits and does not override answer
  from `Object`: `equals` is reference identity, `getClass().getName()` is
  `java.lang.Object` (`getSimpleName()` is `Object`), and `toString()` is the
  `java.lang.Object@<hash>` form. `Object` is deliberately not in the class table
  — it is also the erasure of every type variable, so a receiver statically typed
  `Object` has to keep dispatching on its runtime value — and a class that
  declares its own `equals`/`toString` always wins over the inherited one.
  `hashCode()` answers on an `Object` (see the `hashCode` entry below for why a
  user class's is still an error).
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
  widening < reference upcast < boxing; ambiguity is an error). Applies to
  `static` methods, instance methods (with virtual dispatch keyed on the
  statically-chosen signature), and constructors.
- **Varargs (`T... xs`) in a user-declared method, constructor, or instance
  method.** `static int sum(int... xs)` is callable as `sum()`, `sum(1)` and
  `sum(1, 2, 3)`: the trailing arguments are packed into a `T[]` at the call
  site, which is what the body then sees. Resolution follows Java's phase
  ordering — the variable-arity phase runs only after both fixed-arity phases
  find nothing applicable — so a fixed-arity overload always wins at its own
  arity (`f(int, int)` beats `f(int...)` for `f(1, 2)`), and an argument that
  already *is* the array matches the declared `T[]` in the fixed-arity phase and
  passes through unwrapped (`sum(new int[]{1, 2})` is two elements, not one).
  That same rule is why `f(null)` against `f(Object... xs)` passes `null` as the
  whole array — the reading `javac` warns about and then compiles. Among two
  variable-arity candidates the more specific element type wins
  (`h(String...)` over `h(Object...)`, including for the zero-argument call);
  mutually-specific ones are ambiguous and select nothing, exactly as `javac`
  reports for `g(int, int...)` against `g(int...)`. All three call sites pack
  identically, so a `T...` declaration means the same thing wherever it sits.
  `main(String... args)` is unaffected: it is the entry point and takes the
  real argument array.
- **`static` methods resolve against the class they are called on.** A qualified
  `C.m(args)` is looked up in `C`'s own declarations first and then up its
  superclass chain, so a subclass's `static` *hides* the one it inherits
  (`Derived.kind()` and `Base.kind()` are different methods) and a same-named
  `static` on an *unrelated* class is never reachable. An unqualified `m(args)`
  prefers the enclosing class's chain and falls back to the program-wide pool,
  which is what lets a nested class call a sibling's helper. Subroutine names
  carry the declaring class too, so two classes may declare the same signature
  without colliding.
- **`List.remove` picks its overload from the argument's static type**, the way
  Java does: an `int`/`short`/`byte`/`char` argument is an *index*
  (`l.remove(1)` drops the second element), and any reference argument — a
  boxed `Integer`, an explicit `Integer.valueOf(x)`, a `String` — is a *value*,
  removing the first element equal to it and answering whether one was found.
  An argument javars cannot type statically keeps the by-index reading.
- **`String.join(sep, iterable)`.** Java's second `join` overload takes an
  `Iterable<CharSequence>`, so a `List` or `Set` argument joins its *elements*
  (`String.join("-", List.of("a", "b"))` is `a-b`); the varargs and array forms
  are the same method.
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
- **`List.subList(from, to)` is a real view, not a copy.** It owns no elements:
  every read and write goes to its window of the backing list, so the aliasing
  works in *both* directions — `list.set(i, v)` shows through the view, and
  `view.set(i, v)` shows in the list. Structural changes made *through* the view
  land in the backing list too (`view.add`/`remove`/`clear` grow, shrink and
  splice the parent), and a `subList` of a `subList` composes offsets down to
  the same backing list rather than snapshotting. A structural modification of
  the backing list made *behind* a view — including `Collections.sort`, which
  bumps `modCount` without changing the length — invalidates it, and the next
  operation on it (or rendering it) throws `ConcurrentModificationException`,
  exactly as Java's `checkForComodification` does. Bounds match Java's two
  distinct failures: an endpoint outside the list is an
  `IndexOutOfBoundsException` naming the offending index, a reversed range is an
  `IllegalArgumentException`. `ConcurrentModificationException` joins the
  modeled throwable hierarchy, so it can be named in a `catch` clause and prints
  as `java.util.ConcurrentModificationException`.
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

- **Streams** — `Arrays.stream(a)`, `list.stream()`, `IntStream.range`,
  `Stream.of`, the intermediate operations (`map`, `filter`, `sorted`,
  `distinct`, `limit`), and the terminals (`collect`, `count`, `sum`, `reduce`,
  `findFirst`, `anyMatch`). Naming any of them is a compile error, not a wrong
  answer.

  The obstacle is not the pipeline shape, it is *where a lambda can be called
  from*: a host builtin cannot re-enter the VM (the same limit that keeps an
  element's `toString()` from running inside a collection, below), so a stream
  cannot be a host object that calls `map`'s function per element. It has to be
  **fused at compile time** — the whole `source.op(f).op(g).terminal(h)` chain
  lowered into one loop with the closure calls emitted inline — which is a
  compiler pass rather than a library addition, and it drags in `Optional` for
  the four terminals that return one (`findFirst`, `min`, `max`, `average`).
  That substrate is not built, and half of it would be worse than none: a
  pipeline that silently drops a stage is exactly the failure mode this file
  exists to prevent.

  The `default` methods the JDK's functional interfaces carry
  (`Function.andThen`/`compose`, `Predicate.negate`/`and`/`or`,
  `Comparator.reversed`/`comparing`) are absent for a smaller reason: the
  interfaces javars supplies declare only their single abstract method, so
  calling a `default` one is a compile error. Lambdas and method references
  themselves are implemented (above).
- **`hashCode()` on a user class**, including a `record`'s derived one. A record
  supplies its accessors, `toString`, and `equals`; calling `hashCode()` is a
  compile error ("class `Pt` has no method `hashCode`") rather than a wrong
  number. That is the point of the omission: a record's hash is derived from its
  *components*, so falling back to an identity hash would answer a plausible
  wrong number where the error is honest. `Object.hashCode()` itself *is*
  supplied (on `new Object()`, and on any receiver javars types dynamically),
  because there the specified answer is the identity hash and nothing else.
- **Class literals** (`C.class`, `int.class`). `x.getClass()` works — it
  evaluates to the runtime class name, over which `getName`/`getSimpleName`
  answer — but there is no way to name a class without an instance, so
  `synchronized (C.class)` and `C.class.getName()` are parse errors.
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
- **The regular-expression constructs with no faithful translation.** Regular
  expressions themselves are implemented (see the entry under "Implemented");
  what is refused — as a `PatternSyntaxException` naming the construct — is the
  tail that `fancy-regex` cannot reproduce: possessive quantifiers (`a*+`),
  atomic groups (`(?>…)`), `\G`, `\X`, `\cX`, `\N{…}`, `\0n` octal escapes,
  conditionals `(?(1)…)`, Unicode *blocks* (`\p{InGreek}`) and the
  `\p{javaLowerCase}` predicates, and the `(?m)`/`(?x)`/`(?d)`/`(?u)`/`(?U)`
  flags. OpenJDK *accepts* every one of these, so this is a deliberate
  difference: the engine would compile most of them into a subtly different
  language (`(?m)$` matches before a `\n` where Java's matches before any of its
  five line terminators; `(?x)` ignores whitespace inside a character class in
  Java but not in the engine), and a named error beats a silently different
  answer. `java.util.regex.Pattern`/`Matcher` themselves are also absent — the
  four `String` methods are the whole surface.
- **The other collection view methods** (`Map.entrySet`, `List.listIterator`).
  `List.subList` is implemented as a real aliasing view (above); these two are
  not, and an unsupported-method error is the honest answer until they are.
- **`super.method(args)`** — a qualified call to the superclass's version of an
  overridden method. `super(args)` (constructor chaining) works; `super.m(…)` in
  a method body does not, and instead of a compile error it reaches the runtime
  as a call on a null receiver (`NullPointerException: Cannot invoke
  "String.m()"`). The error shape is wrong as well as the support: `super` is
  being lowered as an ordinary name rather than rejected.
- **`return <value>` from `main`.** `main` is `void`; only a bare `return;`
  (which ends the program) is accepted there. Value returns work in methods.
- **`switch` *patterns*** (`case Integer i ->`, `case null`, guarded
  `when` clauses). The arrow form itself is implemented (above); it is pattern
  labels that are not.
- **`Enum.compareTo`, `EnumSet`, `EnumMap`.**
- **Sealed types, inner (non-`static`) classes,** and **anonymous classes** other
  than the enum-constant body form.

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
  integer value, so it prints `7`, not `7.0`.
- **`==` on objects is reference identity; on strings it compares by value.**
  Object and array handles (`Value::Obj`) are identity-comparable, so `x == y`,
  `x == z` after `z = x`, and `obj.field == null` all behave like Java's reference
  `==`. String `==`, however, compares by *value* (Java's is identity) — this
  matches the far more common intent and avoids surprising `"ab" == "a"+"b"`.
- **A boxed `Character` is a one-character String.** The `char` *type* is a real
  16-bit integral value (see the "`char` arithmetic" entry under "Implemented"),
  but javars boxes no primitive, so a `char` entering an erased position — a
  `List<Character>` element, a `Map<Character, …>` key — is stored as its
  one-character String instead of as a `Character` object. That is what makes
  `System.out.println(list)` print `[p, q]` like Java's. The visible difference
  is `==` on two boxed values: Java compares `Character` *references* (and
  caches the ASCII range, so `Character.valueOf('a') == Character.valueOf('a')`
  is true), javars compares the strings by value — the same String-`==` model
  above.
- **A `ClassCastException` message drops the module-and-loader clause for a user
  class, and a cast javars cannot decide passes through.** Reference casts *are*
  checked (see "Checked reference casts" under "Implemented"), with two bounded
  gaps. Java appends `(X and Y are in unnamed module of loader
  com.sun.tools.javac.launcher.MemoryClassLoader @<identity hash>)` to a
  user-class message, which javars has no counterpart for, so it ends after
  `class X cannot be cast to class Y`; between two `java.lang` types the whole
  message is exact. And a value whose class javars erases — an array (element
  type gone), a collection, a lambda — passes any cast rather than inventing a
  failure, so `(String) anIntArray` succeeds where Java throws. A boxed
  `Character` is the one-character String javars models it as, so a failing cast
  from one names `java.lang.String` and `(String) aBoxedChar` succeeds.
- **A `float` reaching `String.format` through a *non-literal* format string
  prints its `double` form under `%s`.** `String.format` is the one place a
  `float` argument splits by conversion — `%s` wants `Float.toString`, `%f` and
  `%e` want the widened `double` — so the compiler scans the format string to
  see which slots are text conversions. When the format is not a literal there is
  nothing to scan, and the numeric conversions are kept exact at the cost of
  `%s`. Every literal format string, which is what programs write, is exact on
  both sides.
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
  the callee rather than Java's `Cannot invoke "P.get()"`, and a `null`
  `synchronized` monitor says `Cannot enter synchronized block because the
  monitor is null` where Java names the expression it came from.
- **The identity hash is javars's heap handle, not the JVM's.** It shows in
  `Object.hashCode()` and in the `@<hash>` half of the default `toString()`.
  Java's number is not reproducible either — it differs between runs of the same
  program — so no deterministic program can print it, and the properties one
  *can* rely on hold here: stable within a run, equal for equal references,
  different for the objects a `HashMap`/`HashSet` has to keep apart.
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
  The same boundary decides `toString()`/`equals()` called on a receiver javars
  types only *dynamically*: `Pt p = new Pt(1, 2); p.toString()` dispatches to the
  record's own version and is exact, while `Object o = new Pt(1, 2);
  o.toString()` falls to `Object`'s (`Pt@2`, and `o.equals(…)` is identity),
  because a builtin has no way to call back into the override. A receiver
  declared with its class — which is how the overwhelming majority of Java is
  written — is on the static path and unaffected.
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
