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
  target held, the pre-form to the value it takes — on every lvalue Java allows,
  not just a plain local: an array element (`a[i]++`, `++g[r][c]`), an instance
  field (`p.n++`, `++this.n`), and a `static` (`C.total++`). JLS 15.14.2/15.15.1
  define these as `+= 1`/`-= 1` *with* the implicit narrowing cast a compound
  assignment carries, so they share that lowering: `byte b = 127; b++` is -128,
  and the array, index, or receiver is evaluated exactly once
  (`a[idx()]++` calls `idx()` a single time).
- **Unary `+`.** It changes no bits, but it is not a no-op: JLS 5.6.1 applies
  unary numeric promotion, so `+aChar` is an `int`. That is visible wherever the
  static type picks the rendering — `"" + +'A'` is `"65"`, not `"A"` — and in
  overload resolution.
- **Cast expressions** (`(int) d`, `(byte) n`, `(char) 65`, `(Object) x`). Java's
  narrowing primitive conversions are real value changes: floating → integral
  *saturates* (`(int) 1e18` is `Integer.MAX_VALUE`) and truncates toward zero,
  and the integral narrowings are two's-complement — except `(char)`, which is
  *unsigned* 16-bit, so `(char) -1` is 65535. Widening and identity casts emit
  the operand alone, so `(int) i` stays native.
- **Checked reference casts.** `(Cat) animal`, `(Integer) o`, `(Marker) x`,
  `(String) aList`, `(HashMap) aTreeMap` verify the receiver's *runtime* class —
  read from the same `value_class` `instanceof` reads — against the same
  supertype graph `instanceof` walks, and throw `ClassCastException` when it does
  not fit — `class Dog cannot be cast to class Cat`, or Java's full
  module-and-loader wording when both types are JDK ones. A cast throws exactly
  when `instanceof` is false, `Object` and `null` aside.
  `null` casts to anything, `(Object) x` always passes, and
  a cast does not erase what the operand is (`println((Object) dog)` still finds
  `Dog`'s `toString`). The wrapper types javars's value model tells apart
  (`String`, `Integer`, `Double`, `Boolean`, `Number`, `CharSequence`) are checked
  too; the ones it cannot (`int` and `long` are one integer here, and a
  `LinkedList` is the mutable list an `ArrayList` is) are allowed rather than
  guessed at. **An array is the one value the cast still declines**: its element
  type is erased, so javars can produce neither `[I` nor `[Ljava.lang.String;`,
  and it does not throw an exception whose message would have to invent the class
  it names — `(String) anIntArray` passes where Java throws.
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
- **`%f`, `%e` and `%g` round the *shortest* decimal HALF_UP.** Java's
  `Formatter` rounds through `BigDecimal.ROUND_HALF_UP`, so `%e` of 5592405.5 is
  `5.592406e+06` where half-to-even gives `5.592405e+06` — and the digits it
  rounds are the value's shortest round-trip decimal, not its exact binary
  expansion. That is the whole of why `%.2f` of 1.005 is `1.01`: the exact value
  is 1.00499999999999989…, so rounding the expansion answers `1.00`, while the
  digits `1.005` round up. The same source is why `%.20f` of 0.1 is
  `0.10000000000000000000` and not `0.10000000000000000555` — a precision past
  the digits the value has pads with zeros. Scaling by a power of ten before
  rounding would round the tie away before it could be seen, so all three
  conversions take their digits from the same place.
- **Fully-qualified type names.** `java.util.List<String> l`,
  `new java.util.ArrayList<>()`, `static java.lang.String greet(java.lang.String
  who)`, a qualified field type, `catch (java.util.regex.PatternSyntaxException
  e)`, `java.lang.System.out.println(…)`, and the qualified *static call*
  (`java.util.Arrays.sort(x)`) all resolve to the simple name — javars keys every
  type on it, and an `import` line is skipped for the same reason. A package
  segment is recognised by starting lowercase and being followed by another name,
  which is what keeps the identical `outer.next.n = 9` shape an expression.
- **Widening primitive conversion.** Java's assignment and method-invocation
  conversions (JLS 5.2 / 5.3) change the *value*, not only its static type, so
  `double d = 7;` stores 7.0 and prints `7.0`. javars's runtime is dynamically
  typed on the fusevm value model, which means the conversion has to be emitted
  at each site the language performs one, and it is: a local initializer and
  assignment, an instance or `static` field's initializer and assignment, an
  array-element store, a typed (`new double[]{…}`) or untyped (`double[] a =
  {1, 2}`) array literal, a method or constructor argument, a `return`, a
  floating conditional's integral branch (`flag ? 1 : 2.0` is 1.0, JLS 15.25),
  and a lambda whose functional interface returns `float`/`double`. `double`
  emits the native `Op::TruncFloat` — truncation is the identity on a whole
  number, so the conversion costs one op and no builtin call, and unlike the
  `float` path it adds nothing to `--tiers`'s block-JIT-ineligible list.
  `float` rounds to 32-bit precision through the same host cast `(float) x`
  uses, because that is a real value change: `float f = 16777217;` is
  1.6777216E7 where `double d = 16777217;` is 1.6777217E7.
- **A compound assignment narrows back to its target's width.** JLS 15.26.2
  makes `b += 100` on a `byte` mean `b = (byte) (b + 100)`, so it overflows at
  the *target's* width, not at `int`'s: `byte` and `short` sign-extend (`-56`,
  `-32768`), `char` masks to 16 unsigned bits (`65535` + 1 is 0). Applies to
  every compound operator and to `++`/`--`, on a local, a field, a `static`, and
  an array element. The same rule reads `(T)` literally when `T` is integral and
  the right-hand side is floating: `int x = 1; x += 1.5;` is `x = (int)(1 + 1.5)`
  and stores 2, and the cast saturates the way `(int)` does rather than wrapping,
  so `int o = 2147483647; o += 1.0;` stays `2147483647`.
- **The whole integer-literal syntax.** Decimal, hex (`0x1F`), binary
  (`0b1010`), octal (`017`), and `_` digit separators. Hex and binary are read as
  a *bit pattern* at the literal's width, so `0xFFFFFFFF` is the `int` -1 and
  `0xFFFFFFFFFFFFFFFFL` is the `long` -1.
- **Standard-library essentials.** `Math.abs`/`max`/`min`/`pow`/`sqrt`/`floor`/
  `ceil`/`round`/`signum`/`floorDiv`/`floorMod`/`toRadians`/`toDegrees`/`rint`/
  `copySign`/`ulp`/`nextUp`/`nextDown`/`nextAfter`/`fma` (with
  Java's int-vs-double overload result typing) and the `Math.PI`/`Math.E`
  constants; the whole exact-arithmetic family —
  `addExact`/`subtractExact`/`multiplyExact`/`divideExact`/`floorDivExact`/
  `ceilDivExact`/`incrementExact`/`decrementExact`/`negateExact`/`absExact`/
  `toIntExact` — and `Math.clamp`, each resolved to the overload the arguments'
  static types select
  — so `Math.addExact(2000000000, 2000000000)` is `ArithmeticException: integer
  overflow` where `Math.addExact(2000000000L, 2000000000L)` is 4000000000, and
  `Math.clamp(0.1f, 0f, 3f)` renders as a `float`. A call whose argument types
  javars cannot infer is refused, naming the method, rather than answered from a
  guessed overload; the `Integer`/`Long`/`Short`/`Byte`/`Double` `MAX_VALUE`/`MIN_VALUE`
  constants plus `Double.NaN` and the infinities; `Integer.parseInt` (with
  radix)/`valueOf`/`toString` (with radix)/`toBinaryString`/`toHexString`/
  `toOctalString`/`compare`/`max`/`min`/`sum`/`signum` and the `Long`
  equivalents; `Double.parseDouble`/`valueOf`/`toString`/`compare`/`isNaN`/
  `isInfinite`; `Boolean.parseBoolean`/`toString`/`compare`; the `Character`
  predicates and case conversions; `Integer`/`Long`/`Double`/`Float`/`Boolean`/
  `Character`'s `hashCode(x)`, each folding at its own width (so
  `Float.hashCode(1.5f)` and `Double.hashCode(1.5)` are different numbers);
  `String.valueOf`/`copyValueOf`/`join`/`format`; and
  `Arrays.toString`/`deepToString`/`sort`/`fill`/`equals`/`copyOf`/
  `copyOfRange`/`binarySearch`/`hashCode`; `List.of`/`Set.of`/`Map.of`, each
  immutable and each rejecting what the JDK's factory rejects (a repeated
  element or key, a `null`); and `hashCode`/`equals` on all three collections,
  computed by the `AbstractList`/`AbstractSet`/`AbstractMap` rules — the list's
  `31 * h + e` fold, the set's order-independent sum, the map's sum of
  `key ^ value` — so an `ArrayList` and an `Arrays.asList` holding the same
  elements hash alike and a `HashMap` equals a `LinkedHashMap` holding the same
  entries. `String.format` covers `%d %s %S %f
  %e %E %g %G %b %B %h %H %x %X %o %c %%` and `%n`, all seven flags
  (`-`/`#`/`+`/` `/`0`/`,`/`(`), width, `.precision`, and explicit argument
  indexes (`%2$s`); `%f` rounds
  the value's shortest round-trip decimal HALF_UP, as Java's `Formatter` does.
  The ` ` and `#` flags used to be *parsed and discarded*, so `% d` of 42
  answered `42` where Java answers ` 42` and `%#x` of 255 answered `ff` where
  Java answers `0xff`. The radix conversions read the argument as an unsigned
  bit pattern **at its declared width**, which is a static-type question in a
  value model that keeps every integral in one 64-bit slot: `%x` of the `int`
  -1 is `ffffffff`, of the `long` -1 sixteen `f`s, and of a `byte` -1 just `ff`.
  Zero padding goes between the sign and the digits and inside the `(` flag's
  parentheses, so `% 08d` of 1 is ` 0000001` and `%(08d` of -1 is `(000001)`.
  A flag the conversion does not accept (`%,x`, `%#d`, `%0s`), a pair that
  contradict each other (`%+ d`, `%-05d`), and `-`/`0` without a width (`%-d`)
  are `FormatFlagsConversionMismatchException` / `IllegalFormatFlagsException` /
  `MissingFormatWidthException`, each with Java's own detail message and each
  catchable — they were silently ignored before, so the format string said
  something the program could not have meant and nothing reported it. Each
  conversion is type-checked against its argument's boxed class the way
  `java.util.Formatter` is, so `String.format("%.2f", 3)` raises
  `java.util.IllegalFormatConversionException: f != java.lang.Integer` instead of
  formatting the `int`; a `null` argument prints as `null` under every
  conversion but `%b`. The value model cannot tell an `Integer` from a `Long`
  from a `char`, so the compiler sends each argument's static Java type along
  with the values. The other four ways a specifier can be wrong raise Java's own
  classes too, all of them `java.util.IllegalFormatException` subclasses and so
  catchable as `IllegalArgumentException`: too few arguments is
  `MissingFormatArgumentException: Format specifier '%,10.2f'` (the specifier
  verbatim), an undefined conversion is `UnknownFormatConversionException:
  Conversion = 'z'`, and a width or precision too large for an `int` is
  `IllegalFormatWidthException` / `IllegalFormatPrecisionException` with Java's
  own `-2147483648` message. Those four were internal `javars:` errors that ended
  the run; two of them were worse than uncatchable, because javars parsed width
  and precision into a `usize` — `%.99999999999f` reached
  `format!("{:.*}", prec + 30, x)` and *panicked* ("Formatting argument out of
  range"), and `%99999999999d` reached the padder and hung. Measured against
  `openjdk 21.0.12`; five records in the frozen corpus.
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
  `repeat`, `indexOf(t, from)`, `startsWith(p, offset)`, `lastIndexOf`,
  `codePointAt`, `strip`,
  `stripLeading`, `stripTrailing`, `isBlank`, `hashCode`, `intern`,
  `contentEquals`, `toCharArray`, `formatted`, and the four
  `java.util.regex` methods `split`/`replaceAll`/`replaceFirst`/`matches` (see
  the regular-expression entry below).
  `x.getClass()` evaluates to the runtime class's *binary name*, over which
  `getName` (the name itself) and `getSimpleName` (that name with the package
  and enclosing-class qualifiers dropped, or an array descriptor decoded back to
  `int[]`) are `String` methods.
  `trim`, `strip` and `isBlank` are three different rules and javars keeps them
  apart: `trim` removes everything `<= U+0020`, while `strip`/`isBlank` use
  `Character.isWhitespace`, which excludes the non-breaking spaces `U+00A0`,
  `U+2007`, `U+202F` and `U+0085` and includes the information separators
  `U+001C`–`U+001F`. Neither is Rust's `char::is_whitespace`.
  Index/length semantics use Unicode scalar positions — exact for ASCII/BMP,
  one unit per astral character (which Java counts as two UTF-16 units).
- **`StringBuilder` and `StringBuffer`.** The mutable character sequence, as a
  host shape rather than a class instance: `append` (every overload, including
  `char[]` and another builder), `appendCodePoint`, `insert`, `delete`,
  `deleteCharAt`, `replace`, `reverse`, `setCharAt`, `charAt`, `substring`,
  `subSequence`, `indexOf`/`lastIndexOf` (with and without a start), `length`,
  `isEmpty`, `setLength`, `capacity`, `ensureCapacity`, `trimToSize`, `repeat`,
  `compareTo`, and `toString`. A builder is a *reference*: passing one to a
  method, storing it in an array or a map, and reading it back all denote the
  one object, and `equals`/`hashCode` stay `Object`'s (two builders holding the
  same text are unequal, exactly as in Java) while `toString` — and therefore
  `println(sb)`, `"" + sb`, `%s`, and a list element — is the contents.
  `capacity()` is modeled rather than delegated to Rust's own allocation,
  because it is observable: the JDK starts at 16 (plus the initial string's
  length), and grows to `2 * old + 2` or the required size, whichever is larger.
  Every bounds failure carries the JDK's own detail message, which is three
  wordings and not one — `Index i out of bounds for length n` for a single
  index, `Range [s, e) out of bounds for length n` for a pair, and
  `String index out of range: n` for `setLength` — and `delete`/`replace` clamp
  their end to the length before the check while `substring` does not, which is
  why `delete(2, 100)` truncates and `substring(1, 9)` throws. `StringBuffer`
  differs only in its class name: javars runs one thread, so the synchronized
  methods are unobservable. Index and length semantics use Unicode scalar
  positions, the same simplification the `String` methods take.
  `System.out.println(char[])` writes the characters too — it is a distinct
  `PrintStream` overload, and it was rendering the array handle.
- **A boxed primitive's own methods.** `Number`'s six converters
  (`intValue`/`longValue`/`shortValue`/`byteValue`/`doubleValue`/`floatValue`),
  `Boolean.booleanValue`, `Character.charValue` and `Object.hashCode` answer on
  a boxed receiver. They are checked *before* the `String` table, which is what
  a boxed receiver used to reach after being rendered to text — that table has
  no `intValue`, but it does have `hashCode`, so `Integer.valueOf(300)
  .hashCode()` answered 50547 (the hash of `"300"`) instead of 300. The
  converters narrow the way Java's casts do, and the two receiver kinds differ:
  a `double` saturates at the `int` bounds (`Double.valueOf(1e30).intValue()` is
  `Integer.MAX_VALUE`) where a `long` wraps
  (`Long.valueOf(4294967296L).intValue()` is 0). A user class declaring a method
  of the same name still wins, because its own resolution runs first.
- **Reference arrays.** `new T[n]` (default-valued), `{…}` literals (and
  `new T[]{…}`), get/set indexing (`a[i]`, `a[i] = v`, compound `a[i] += v`,
  `a[i]++`), and `a.length`. Arrays are heap objects (`Value::Obj` handles into a
  host slab in `src/host.rs`), so Java's *reference* semantics hold: pass an array
  to a method, mutate an element, and the caller observes the change. Out-of-range
  indices fault like `ArrayIndexOutOfBoundsException`.
- **Classes and objects.** `class C { fields; C(params){…}; methods }` — instance
  fields, constructors,
  instance methods, `this`, `new C(…)`, field access (`obj.f`, `obj.f = v`), and
  implicit-`this` field/method access. Multiple classes per file, including nested
  `static` classes. Instances are heap objects with reference/aliasing semantics.
- **Instance initialization (JLS 8.6, 8.8.7, 12.5).** Allocation seeds every
  field in the whole chain with its type's default. Each constructor then runs
  the three steps JLS 12.5 fixes, in that order: the **superclass constructor**
  (an explicit `super(…)`, or the implicit `super()` a body that does not open
  with `this(…)`/`super(…)` gets), then **this class's** field initializers and
  bare `{ … }` instance-initializer blocks in textual order, then the
  **constructor body**. `this` is bound throughout, so a block can call an
  instance method and a field initializer can read a field declared above it —
  or an inherited one the superclass constructor just assigned
  (`class A { int x; A() { x = 100; } } class B extends A { int y = x + 1; }`
  gives `y == 101`).

  The step order is observable in both directions. A virtual call made from a
  superclass constructor reaches the subclass's override — and sees the
  subclass's fields still at their **defaults**, because the subclass's
  initializers have not run yet. That is why the initializers are emitted inside
  each constructor rather than at the allocation site: running them all up front
  would show that override the finished values, and would let a subclass
  initializer overwrite what the superclass constructor assigned.

  A class that declares no constructor gets Java's default one, whose body is
  `super()` followed by the same initializers — so a superclass constructor runs
  whether or not the subclass wrote one, and an intermediate class that declares
  no constructor still contributes its `{ … }` blocks at its own point in the
  chain.
- **Explicit constructor invocation.** `super(args)` chains to the superclass
  constructor and `this(args)` delegates to another constructor of the same class
  (the telescoping-constructor shape); the delegate is what runs the `super()`
  chain, so it is never run twice. Both resolve their target by argument type,
  variable-arity constructors included.
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
  `equals` that compares each component the way JLS 8.10.3 says to — a `float`
  or `double` through `Float.compare`/`Double.compare` (so
  `new D(Double.NaN).equals(new D(Double.NaN))` is `true` and
  `new D(0.0).equals(new D(-0.0))` is `false`, both the opposite of `==`), any
  other primitive with `==`, and a reference through `Objects.equals`, which
  reaches a component class's own body. A compact constructor
  (`Pt { if (…) throw …; }`) runs its validation
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
- **`super.member`.** `super.method(args)` is the one call in Java that must
  *not* dispatch on the receiver's runtime class, and javars emits it as a direct
  call to the body resolution finds starting at the **declaring** class's
  superclass. That is what lets an override call the version it overrides
  (`public String toString() { return "C{" + super.toString() + "}"; }`
  terminates instead of recursing), and it walks past a parent that does not
  declare the method to reach the grandparent's body. Overload selection
  (`super.f(2.5)` picking `f(double)` over `f(int)`) and varargs packing
  (`super.vs(xs)` passing an array through unwrapped, `super.vs(1, 2)` packing)
  both happen at the superclass, like any other call. Only that one call site is
  de-virtualized: a `super.m()` whose callee calls an unqualified `n()` still
  reaches the *subclass's* `n`, which is Java's rule. It works from an ordinary
  method, from inside a lambda body (the closure carries `this`), through a
  `super::m` method reference, and from an `enum` constant's body to the enum's
  own method. `super.field` reads and writes the same cell `this.field` does
  (plain, compound, and `++`/`--`), and when no user class up the chain declares
  the member, `super.toString`/`equals`/`hashCode` answer `java.lang.Object`'s.
  `super` outside an instance method, and a member no superclass has, are
  compile errors — `javac` rejects both too.
- **`instanceof` over every shape the value model names.** The runtime type test
  answers a boxed primitive (`Integer`/`Double`/`Boolean` plus `Number`,
  `Comparable`, `Serializable`), a `String` (plus `CharSequence`), an array
  (`Cloneable`, `Serializable`), each modeled collection against both its
  concrete kind and the `java.util` interfaces above it — including the two
  pairs a name match gets wrong, `LinkedHashMap extends HashMap` and
  `LinkedHashSet extends HashSet` where the tree kinds do not — the three list
  views (`List.of`, `Arrays.asList`, `subList`) that are `List`s but not
  `ArrayList`s and disagree with each other on `AbstractList` and
  `Serializable`, `Set.of` which is a `Set` but not a `HashSet` (it reaches
  `AbstractCollection` but not `AbstractSet`, and is not `Cloneable`), a user
  class or interface over its declared graph, and the two
  supertypes `javac` supplies implicitly so the source never mentions them: an
  `enum` is a `java.lang.Enum` (hence `Comparable`) and a `record` a
  `java.lang.Record`. Every non-null reference is an `Object`, and `null` is an
  instance of nothing, including `Object`.

  The JDK half of that graph is a table of *direct* supertypes (`jdk_supers`),
  walked by the same routine that walks the program's own declarations, and the
  reference cast reads it too rather than keeping a second copy of the wrapper
  supertypes. `catch` matching shares the builtin, so a handler claims exactly
  what an `instanceof` of the same type would.
- **Inheritance.** `extends`, `super(…)` constructor chaining, inherited fields
  and methods, method overriding, and `instanceof` (respecting the subclass
  chain). `toString()` overrides are honoured wherever a value renders, whatever
  the receiver's static type: `System.out.println(obj)`, string concatenation
  (`"x = " + obj`, `s += obj`), an explicit `obj.toString()`,
  `String.valueOf(obj)`, `Arrays.toString`/`deepToString`, `String.join`,
  `String.format("%s", obj)` / `"%s".formatted(obj)`, and every element of a
  `List`/`Set`/`Map` at any depth. A subclass that declares none inherits its
  ancestor's body; a class whose chain declares none renders the default
  `Class@hash` form.

  **How concatenation reaches it, when the numeric hook cannot.** fusevm has two
  host-callback shapes and only one of them carries a VM:

  | callback | type | gets a `VM`? |
  | --- | --- | --- |
  | registered builtin | `BuiltinHandler = fn(&mut VM, u8) -> Value` | **yes** |
  | numeric hook | `NumericHook = Arc<dyn Fn(NumOp, &Value, &Value) -> …>` | no |
  | sited numeric hook | `SitedNumericHook`, whose `NumericCall` adds `chunk`/`ip` | no |

  Every rendering surface but one is a builtin, so each can re-enter and run the
  override: `println` is `print_args`, `list.toString()` is `coll_method`,
  `String.valueOf`/`Arrays.toString`/`String.join` are the static dispatch
  builtin, `String.format`/`formatted` are `b_format`. The exception was `"" +
  obj`, which the compiler lowered to `Op::Add`, whose non-numeric operand lands
  in `host::numeric_hook` — three values and no VM. Fixing only the builtin side
  would have made `println(list)` and `println("" + list)` print the same list
  two different ways in one program, so the fix is not to make the hook re-enter
  (it cannot) but to keep concatenation away from it: an operand whose static
  type does not name a user class goes through the `JSTRINGIFY` builtin
  (`Compiler::emit_host_stringified`) instead of `Op::Add`, and the host resolves
  the override by looking `Class#toString#` up in `chunk.sub_entries` and calling
  `run_sub`, walking supertypes for a subclass that declares none.

  Both halves are gated on the program declaring an override at all — the
  compiler's on a whole-program scan, the host's on a chunk-name scan — and the
  compiler additionally skips any operand it has typed as a primitive, a wrapper
  or a `String`, none of which can be a heap handle. A program with no override
  emits byte-identical bytecode: `--disasm` of a 200k-iteration concatenation
  loop is unchanged, and so is its user time (1.01/1.01/1.04 s before,
  1.05/1.00/1.03 s after). With an override declared but only primitives
  concatenated, the bytecode is unchanged too (1.00/1.01/1.00 s).

  Running the override is running user code, with the consequences that implies:
  it may print (rendering happens before stdout is locked, so its output comes
  first), and it may throw (the throwable propagates out of the surface that was
  rendering, and the half-built text is discarded rather than written).
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
  prints `java.lang.Foo: message`. A user class may `extend` any of them, and a
  user subclass reports *its own* runtime class everywhere Java does:
  `class MyEx extends RuntimeException` nested in `T` renders `T$MyEx: b` from
  `toString()`, from `"" + e`, from `println(e)`, and from the uncaught report.
  Two separate defects used to break exactly that case. The prelude was injected
  only for a program that wrote `throw`/`try`/`new <throwable>`, so a program
  that merely *subclassed* one got no `Throwable` at all — the `extends` dangled
  silently and `e.getMessage()` was "class `MyEx` has no method `getMessage`".
  And each prelude class carried its own `toString()` with the qualified name
  written into a string literal, so once the prelude was present a user subclass
  inherited `RuntimeException`'s and rendered `java.lang.RuntimeException: b`.
  There is now one `toString()`, on `Throwable`, reading
  `this.getClass().getName()` the way Java's own does; the uncaught report asks
  `qualified_or_binary` rather than `qualified_throwable`, which had answered
  `None` for a user class and printed the simple name `MyEx`.
  `getLocalizedMessage()` is supplied alongside `getMessage()`. Four records in
  the frozen corpus. An
  exception no handler claims reports Java's `Exception in thread "main" …` line
  on stderr and exits non-zero — the same exit status (1) Java uses, with the
  line prefixed `javars: ` and no `at T.main(T.java:1)` frame after it, because
  javars keeps no call-site table to unwind. Verified one probe against
  `openjdk 21.0.12`: both print `x` to stdout and exit 1, and stdout is
  identical.
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
  count is negative: -1`). A `null` *argument* faults where Java faults, rather
  than being coerced to the empty string and answered: the ten `String` methods
  that dereference their argument (`compareTo`, `compareToIgnoreCase`,
  `contains`, `indexOf`, `startsWith`, `endsWith`, `concat`, `split`, `replace`,
  and `String.join`'s delimiter) raise `NullPointerException` with the JDK's own
  wording, while `equals` and `equalsIgnoreCase` still answer `false` and a null
  `String.join` *element* still renders `null`, because those are specified not
  to throw. `"abc".compareTo(null)` used to answer 3, `startsWith(null)` `true`,
  and `split(null)` the whole string — a wrong answer where Java throws, with
  nothing marking it. `x.compareTo(null)` on a box names the box's own parameter
  (`anotherInteger`, `anotherDouble`, `anotherCharacter`, `anotherLong`, `b`),
  measured one type at a time rather than sharing one wording. The integral
  parsers separate a null from an empty string — `Integer.parseInt(null)`,
  `Long.parseLong(null)` and `Integer.valueOf(null)` are `NumberFormatException:
  Cannot parse null string`, not `For input string: ""` — and the *floating*
  parsers differ in class, not merely in text: `Double.parseDouble(null)` and
  `Float.parseFloat(null)` are a `NullPointerException`, because
  `FloatingDecimal.readJavaFormatString` has no null check and the dereference is
  what fails. A `catch (NumberFormatException e)` therefore does not catch them,
  and javars used to send them there. `Integer.valueOf(null)` did not fault at
  all: it read `null` as the `int` overload and answered 0. All measured against
  `openjdk 21.0.12`. A fault raised inside a called method unwinds to the
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
- **The functional interfaces' `default` and `static` members.**
  `Function.andThen`/`compose`/`identity`, `BiFunction.andThen`,
  `Predicate.and`/`negate`/`or`/`not`, the `BiPredicate` and `IntPredicate`
  forms of the same three, `Consumer.andThen`/`BiConsumer.andThen`/
  `IntConsumer.andThen`, `UnaryOperator.identity`,
  `IntUnaryOperator.compose`/`andThen`/`identity`,
  `BinaryOperator.minBy`/`maxBy`, and `Comparator.reversed`/`thenComparing`.
  Each is the JDK's own body, written in Java in `src/prelude.rs` and compiled
  through the paths a user's own `default` method already used — no builtin.
  Reaching one on a *lambda* receiver is the half that needed work: a closure's
  runtime class matches no concrete-class arm of the dispatch chain, so the
  interface's own body is emitted as its arm (and as the whole call when no
  class implements the interface at all). The JDK's leading
  `Objects.requireNonNull(after)` is the one thing dropped — it would pull the
  entire throwable prelude into every program that writes a lambda — so
  composing with `null` raises the same `NullPointerException` at the composed
  function's first call rather than at composition.
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
  loop still emits exactly the ops it did before), `keySet()`/`values()` produce
  the map's keys and values in the map's own order (as *copies*, not views — see
  "Runs wrong rather than being rejected"), and `sort`/`forEach` take a lambda.
  **`HashMap`/`HashSet` iterate in Java's real bucket order**, not insertion
  order: Java indexes a power-of-two table with `(capacity - 1) & (h ^ (h >>> 16))`,
  appends within a bucket, and preserves relative order across a resize, so the
  order is a stable sort of the insertion sequence by bucket index — reproduced
  exactly, and checked against OpenJDK for `String` and `Integer` keys including
  across the resize at 13 entries. `LinkedHashMap`/`LinkedHashSet` keep insertion
  order and `TreeMap`/`TreeSet` sort, each because Java does, not by default.
  `Arrays.asList` is fixed-size and `List.of`/`Set.of` immutable, so a structural
  write to any of them throws `UnsupportedOperationException` exactly as Java's
  does — including `Set.of(1).remove(9)`, which Java refuses before deciding the
  removal would have changed nothing — and an out-of-range `get` throws
  `IndexOutOfBoundsException` with Java's message. `Set.of` is also the one
  set-building factory that *rejects* a repeat instead of dropping it:
  `Set.of(1, 2, 1)` is `IllegalArgumentException: duplicate element: 1`, naming
  the element through its `toString`, where `new HashSet<>(…)` would have
  answered a two-element set. Silently de-duplicating turned a program Java
  refuses to run into one that ran and answered.
- **A collection's membership test calls the element's own `equals()`.**
  `contains`/`indexOf`/`lastIndexOf`/`remove(Object)`/`List.equals`, `Set`
  de-duplication (`add`, `addAll`, `Set.of`, `new HashSet<>(seq)`), and
  `Map.get`/`getOrDefault`/`containsKey`/`containsValue`/`put`/`putIfAbsent`/
  `remove` all compare through the class's own body, at every depth of a
  `subList` view, so `list.contains(new R(1))` on a `record` is `true` and a
  `HashSet` of two equal records has size 1.

  The receiver is the *query*, not the element, because that is the direction
  the JDK calls in — `ArrayList.indexOf(o)` runs `o.equals(element)`,
  `HashMap.getNode` runs `key.equals(storedKey)` — so an asymmetric `equals`
  answers here exactly as it does there. The scan stops at the first hit, in the
  direction the calling method scans, so a body with a side effect runs the same
  number of times on both sides. A throwable it raises propagates out of the
  collection call rather than being swallowed into a verdict.

  Mechanically this is the [`JSTRINGIFY`] shape: a body needs `&mut VM` and no
  outstanding borrow of the heap slab, so `host::eq_plan` resolves the
  comparisons *before* the borrow the call takes and the borrowed section reads
  a plain index. A program that declares no `equals` anywhere never builds a
  plan and keeps the code path it always had.

  Two boundaries are deliberate rather than missing:

  * **A hash container asks for a `hashCode` too.** `HashMap`/`HashSet` find an
    element only when its hash puts it in the bucket being searched, so a class
    that overrides `equals` and leaves `hashCode` alone is not found by *Java*
    either — the two instances get distinct JVM identity hashes and never meet.
    javars cannot compute a JVM identity hash, so the declaration is the signal:
    a class declaring `hashCode`, or a `record`/`enum`, is trusted; anything
    else keeps the identity comparison Java effectively performs. An
    `ArrayList` hashes nothing and so asks this of nobody. A class whose
    `hashCode` contradicts its `equals` is where the two models part, and that
    program has no defined answer in Java either.
  * **`TreeSet`/`TreeMap` locate by `compareTo`, not `equals`.** Those keep the
    value model; a user class in a sorted collection is a separate gap.

  One shape is a real divergence rather than a boundary: an `equals` that
  *structurally modifies the collection being searched*. javars resolves the
  position against a snapshot and then indexes the live list, so an insertion
  the body made **before** that position shifts it —
  `list.remove(new M(2))` where `M.equals` prepends answers `true` and a
  three-element list on `openjdk 21.0.12`'s `false` and four. A body that
  *appends* agrees exactly (frozen in the corpus). Java is scanning a live array
  and javars a snapshot, and neither answer is specified; the program is one
  `ConcurrentModificationException` short of being well defined in either.
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
- **`compareTo` on every receiver that has one.** `String.compareTo` /
  `compareToIgnoreCase` return Java's *difference* (the first differing `char`,
  else the length difference) rather than only its sign — programs print the
  number, so the sign alone would be wrong. The boxed types do not agree on
  which number: `Integer`/`Long`/`Double`/`Float` answer the sign,
  `Byte`/`Short`/`Character` the arithmetic difference, and `Boolean` is
  `false < true`. The receiver's static Java type therefore rides along to
  `JCOMPARE_TO` as a tag, and the runtime value classifies the ones the compiler
  could not name. Before that, every non-`String` `compareTo` compared two
  `toString`s as text — `Integer.valueOf(10).compareTo(9)` answered -8
  (`'1' - '9'`) where Java answers 1, and a user `Pt` whose own `compareTo`
  returns 5 answered -1.
- **Multi-dimensional arrays.** `new int[m][n]` (rectangular), `new int[m][]`
  (jagged, inner rows `null`), `a[i][j]` read/write, nested array literals
  (`{{1, 2}, {3, 4}}`), and `.length` at each level. Rows are reference arrays,
  so aliasing holds.

## Runs wrong rather than being rejected

No construct is *silently* mis-run: every case where javars computes something
other than Java is named in "Modeled with a documented simplification" below,
and each is a deliberate, bounded model rather than a bug found and left.

Some of those simplifications do print a different answer for a program `javac`
accepts, and it is worth naming which. The first two are missing *conversions*
javars's untyped runtime never performs, not wrong arithmetic; the last two are
storage-model differences:

- `s.get() / 2` on a `Supplier<Integer>` prints `3.5`, not `3` (the erased
  interface returns `Object`, so javars cannot type the result as `int`).
- `int` arithmetic whose operand types are not statically known keeps fusevm's
  64-bit result rather than wrapping at 32 bits.
- A subclass that **re-declares a field its parent already declares** gets one
  cell rather than two, so the parent's own methods and a parent-typed reference
  read the subclass's value. See "Field *hiding* collapses to one cell" below
  for the reproducer and the exact divergence.
- **`keySet()`/`values()` are copies, not views.** Java's are backed by the map,
  so `m.keySet().remove(k)` removes the entry; javars builds a fresh set or list
  in the map's order, so the removal is lost:

  ```java
  Map<String, String> m = new HashMap<>();
  m.put("k", "v"); m.put("j", "w");
  m.keySet().remove("k");
  System.out.println(m.size());   // Java: 1     javars: 2
  ```

  `List.subList` *is* a real aliasing view (above); these two are not, which is
  also why a cast of one names the set or list javars modeled it as rather than
  `HashMap$KeySet`.

Everything else javars accepts runs with Java's meaning, and the differential
fuzzer (`parity-fuzz`) generates none of the above precisely because they are
known.

### A duplicate declaration is rejected, with one exception

A compilation unit that declares the same name twice is not a program `javac`
would run, but javars *did* run it — against whichever declaration the table it
landed in happened to keep. Every one of those tables is keyed by name, and
they do not agree on which duplicate wins: the class table
(`resolve_classes`' `out.insert`) and a class's field list keep the **last**,
while fusevm's `sub_entries` lookup returns the **first**. So

    class Pt { int v() { return 1; } }
    class Pt { int v() { return 2; } }   // javars printed 1
    class C  { int x = 1; int x = 2; }   // javars printed 2

each answered silently, and the two forms that *did* fail failed at the caller:
a duplicate `static` method as "no `f` overload matches 1 argument(s)" and a
duplicate constructor as "no constructor taking 1 argument(s) (declared arities:
`[1, 1]`)" — that repeated `1` being the duplicate itself, reported as though
the call were at fault.

`Compiler::check_duplicate_declarations` now rejects all of them at the
declaration, in `javac`'s own wording: a duplicate class, field, instance or
`static` method (keyed by name **and** parameter types, so real overloading is
untouched), constructor, `enum` constant, or parameter name.

The exception is a **duplicate local variable**, which still runs and keeps the
last assignment:

    int x = 1;
    int x = 2;   // javac: "variable x is already defined in method main(String[])"

Java's rule is scoped, not flat — two sibling blocks may each declare `x`, but a
nested block may not redeclare an enclosing local — and javars's `MethodScope`
is one flat set per method, with sibling blocks deliberately sharing a slot.
Detecting this needs block-scope tracking that does not exist yet; a flat check
would reject the sibling-block form that Java accepts, which is the worse error.

## Not implemented (parse or compile errors today)

- **`StackOverflowError` and `OutOfMemoryError` are never raised**, and a
  program that would provoke either does not fail — it runs until the OS stops
  it. javars's call frames are heap-allocated `Vec` entries, not native stack
  frames, so there is no stack to overflow: `static int f(int n) { return f(n+1); }`
  returned nothing after 20s under `gtimeout` (rc=124) where a JVM throws in
  milliseconds, and a *bounded* recursion of depth 100000 that the JDK refuses
  (`Exception in thread "main" java.lang.StackOverflowError`) javars completes
  and prints `100000`. `new int[Integer.MAX_VALUE]` and
  `Arrays.copyOfRange(new int[]{1}, 0, Integer.MAX_VALUE)` likewise hang where
  the JDK answers `OutOfMemoryError: Requested array size exceeds VM limit`.
  Both are `Error`s, not `Exception`s, but both are catchable and both are
  therefore observable. Raising them needs a depth counter checked at every call
  — a `CallBuiltin` in the prologue of every method, which aborts JIT trace
  recording and taxes every non-recursive call to serve a case a correct program
  never reaches. Doing it only for methods in a call-graph cycle would confine
  the cost, and is the shape a fix should take; it is not built. Because neither
  class is modeled, `catch (StackOverflowError e)` is now a compile error (see
  the catch-type entry below) rather than an arm that compiles and can never
  fire.
- **`ArrayStoreException` is never raised.** A reference array carries no
  element type at runtime, so storing the wrong type through a widened reference
  succeeds silently: `Object[] o = new String[2]; o[0] = Integer.valueOf(3);`
  stores and continues, where the JDK throws `ArrayStoreException:
  java.lang.Integer`. Measured on `openjdk 21.0.12`. The array's element type
  would have to be recorded on the host object and checked on every store.
- **`Throwable.getCause()`, `getSuppressed()`, `initCause`, `addSuppressed`,
  `printStackTrace()`, and `getStackTrace()`** — and with them the two-argument
  `(String, Throwable)` constructor every modeled throwable lacks, so
  `new RuntimeException("wrap", cause)` is ``javars: class `RuntimeException` has
  no constructor taking 2 argument(s) (declared arities: [0, 1])``. Cause
  chaining and suppression are a *structural* part of a Java throwable, not
  decoration: try-with-resources suppression and `e.getCause()` unwrapping are
  ordinary idioms. javars keeps no call-site table (see the uncaught-report entry
  above), so a stack trace has nothing to print, but the cause and suppressed
  lists have no such obstacle and are simply not built. `getLocalizedMessage()`
  *is* supplied, being `getMessage()` verbatim.
- **An unmodeled `catch` type is a compile error.** javars models the throwable
  subset in `src/prelude.rs`; a `catch` naming anything else — a name that does
  not exist (`catch (TotallyBogusException e)`), or a real JDK throwable outside
  the subset (`catch (StackOverflowError e)`, `catch (java.io.IOException e)`) —
  is ``javars: unknown class `X` (line N)``. `javac` rejects the first and
  accepts the other two, so this is stricter than Java for a name the JDK
  defines. It is deliberate, and it replaces something worse: the arm used to
  *compile*, `JINSTANCEOF` answered `false` for every throwable, and the handler
  was dead code — so the one place a program states what it handles silently
  handled nothing. `new` and `throw` already enforced the same rule; only `catch`
  did not. Each alternative of a multi-catch is checked separately, so a good
  first alternative does not launder a bad second one.
- **Streams** — `Arrays.stream(a)`, `list.stream()`, `IntStream.range`,
  `Stream.of`, the intermediate operations (`map`, `filter`, `sorted`,
  `distinct`, `limit`), and the terminals (`collect`, `count`, `sum`, `reduce`,
  `findFirst`, `anyMatch`). Naming any of them is a compile error, not a wrong
  answer — measured on `openjdk 21.0.12`'s side and javars's, one probe each:
  `list.stream()` is
  ``javars: unsupported List method `stream` with 0 argument(s)``,
  `Arrays.stream(a)` is
  ``javars: unsupported static method `Arrays.stream` with 1 argument(s)``, and
  a bare `IntStream.range(0, 3)` / `Stream.of(1, 2)` under
  `import java.util.stream.*;` is ``javars: cannot find symbol: `IntStream` `` /
  ``javars: cannot find symbol: `Stream` ``. That last one was *not* true
  until the undeclared-name check landed — `IntStream.range(0, 3).sum()` used to
  reach the runtime and report
  `NullPointerException: Cannot read field "util"` / `Cannot invoke
  "String.range()"`, because an unmodeled class name was just an undeclared
  variable reading `null`.

  This entry used to say the obstacle was *where a lambda can be called from* —
  that "a host builtin cannot re-enter the VM", so a stream could not be a host
  object calling `map`'s function per element, and the pipeline would have to be
  fused at compile time. **That was wrong**, and it is worth being precise about
  why, because the same sentence appears elsewhere in this file for a case where
  it *is* true.

  fusevm has two host-callback shapes and only one of them is VM-less:

  | callback | type | gets a `VM`? |
  | --- | --- | --- |
  | registered builtin | `BuiltinHandler = fn(&mut VM, u8) -> Value` | **yes** |
  | numeric hook | `NumericHook = Arc<dyn Fn(NumOp, &Value, &Value) -> …>` | no |
  | sited numeric hook | `SitedNumericHook`, whose `NumericCall` carries `op`/`a`/`b`/`chunk`/`ip` | no |

  A builtin holds `&mut VM`, and javars already re-enters through it:
  `host::run_sub` pushes a frame and calls `vm.run()`, `host::invoke_closure`
  wraps that, and `coll_method`'s `forEach` and `sort` arms call a user lambda
  once per element — including a lambda that calls a user `static` method,
  assigns a `static` field, allocates its own collection, and re-enters a
  collection builtin from inside, all matching `java`. So a stream *can* be a
  host object: `stream()` allocates one, each intermediate pushes an op onto it,
  and the terminal drives the elements through the chain with `invoke_closure`,
  exactly as `forEach` already does. Compile-time fusion is one way to build
  this, not the only way.

  What is genuinely not built is the surface: the sources, the intermediate ops
  with Java's laziness (a `peek` before a `limit` must see only the elements the
  `limit` demands, and `sorted` is a full barrier), the terminals, and
  `Optional`/`OptionalInt`/`OptionalDouble` for the four terminals that return
  one. Half of that would be worse than none — a pipeline that silently drops a
  stage is exactly the failure mode this file exists to prevent — so it stays a
  compile error until it is whole.

  The `default` and `static` members the JDK's functional interfaces carry are
  all implemented, `Comparator.comparing`/`naturalOrder`/`reverseOrder`
  included. They order *arbitrary* elements by their natural ordering, which was
  out of reach while the only `compareTo` a prelude body could call on an element
  it cannot type was the `String` one; with the erased receiver now dispatching
  on the runtime class they are the JDK's own bodies, transliterated
  (`(a, b) -> a.compareTo(b)` and so on). The same lambda is what the compiler
  supplies for a sort that names no comparator — `Collections.sort(l)`,
  `l.sort(null)` — because the host's natural order knows numbers and strings
  and answered "equal" for everything else, so a `List` of a user `Comparable`
  came back in insertion order.
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
  `Boolean`/`Character`/`String`/`Arrays`/`Collections` statics listed above,
  `Objects.equals` (the one `Objects` member, because a `record`'s derived
  `equals` is specified in terms of it), the `String` instance methods, and the
  `java.util` collections are the whole
  library surface — no boxed-type methods beyond the listed statics, no
  `Iterator`/`entrySet`/`Deque`/`Queue`/`Optional`, no I/O. A `hashCode` a
  collection computes reads each element's, and a class that declares its own
  `hashCode` body still does not have that body run (see the `hashCode` entry
  below), so a collection of such instances hashes by identity where Java hashes
  by value. `Math.powExact` and
  the `unsignedMultiplyExact`/`unsignedPowExact` pair, and
  `StringBuilder.append(char[], int, int)`, are on the same footing: a call is a
  compile error naming the method. `System` carries
  only its two streams: `System.exit(3)` is
  ``javars: only `System.out`/`System.err` are supported, not `System.exit` ``,
  which also means a program cannot choose its exit status — 0 for a clean run
  and 1 for an uncaught throwable are the only two javars produces.
- **`Math`'s transcendentals** (`sin`, `cos`, `tan`, `asin`, `acos`, `atan`,
  `atan2`, `exp`, `log`, `log10`, `cbrt`, `hypot`, `sinh`/`cosh`/`tanh`). The JDK
  answers these from its own fdlibm-derived implementation and permits a 1-ulp
  error; Rust's libm does not reproduce it bit-for-bit. A 180-value differential
  sweep against OpenJDK 26 diverged in the last digit for every one of them
  (`sin` 14/180, `cbrt` 25/180, `tan` 5/10), so they are left out: an
  unregistered static is a clear error, a silently different last digit is not.
  `sqrt`, `pow`, `abs`, `floor`, `ceil`, `round`, `max`, `min`, `signum`,
  `floorDiv`, `floorMod`, `toRadians`, and `toDegrees` are exact and supported,
  as are the `Math.PI`/`Math.E` constants — and so are `rint`, `copySign`,
  `ulp`, `nextUp`/`nextDown`/`nextAfter` and `fma`, which sit on the exact side
  of the same line: each is an IEEE operation or a walk over the bit pattern, so
  there is one right answer rather than a 1-ulp allowance. A 30x30 sweep of
  every pair drawn from the boundary values (both zeros, both infinities, NaN,
  `MIN_VALUE`, `MAX_VALUE`, the ties `rint` rounds to even, the subnormals) is
  byte-identical to openjdk 26.0.2.
- **The bit-twiddling statics.** `Integer`/`Long`'s
  `bitCount`, `highestOneBit`/`lowestOneBit`, `numberOfLeadingZeros`/
  `numberOfTrailingZeros`, `reverse`/`reverseBytes`, `rotateLeft`/`rotateRight`,
  and the unsigned family (`divideUnsigned`, `remainderUnsigned`,
  `toUnsignedLong`, `toUnsignedString`); `Double.isFinite`/`max`/`min`;
  `Character.compare` and `isAlphabetic`. Each is an unregistered static, so a
  call is a compile error naming the method rather than a wrong answer.
  `copySign`, `rint`, `ulp`, `nextUp`/`nextDown`/`nextAfter`, `fma`, and the
  `Exact` family with `clamp` were on this list and are now implemented — see
  the `Math` entries above. `Short` and `Byte` are further
  along that scale: their `MAX_VALUE`/`MIN_VALUE` constants resolve, but the
  types carry no statics at all, so `Short.compare(a, b)` is
  ``javars: cannot find symbol: `Short` ``.
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
- **`Map.of`.** `List.of` and `Set.of` are implemented, each as the immutable
  collection Java returns rather than as a mutable one wearing the same name;
  the map factory is not, so there is no `HashMap`-vs-`Map.of` pair for the type
  test or the reference cast to tell apart yet.
- **`return <value>` from `main`.** `main` is `void`; only a bare `return;`
  (which ends the program) is accepted there. Value returns work in methods.
- **`switch` *patterns*** (`case Integer i ->`, `case null`, guarded
  `when` clauses). The arrow form itself is implemented (above); it is pattern
  labels that are not.
- **`EnumSet`, `EnumMap`.** `Enum.compareTo` *is* supplied — it is `final` in
  Java and is the ordinal difference, so it is synthesized onto every enum
  alongside `name`/`ordinal`/`toString`/`equals`, which is also what makes
  `Collections.sort` of an enum list work.
- **Sealed types, inner (non-`static`) classes,** and **anonymous classes** other
  than the enum-constant body form.
- **Assignment as an *expression*.** `n = 5` is a statement here, not a value, so
  the idioms that read the assigned value back — `if (f && (n = 5) > 0)`,
  `while ((c = next()) != -1)`, `a = b = 0` — stop at
  ``javars: expected RParen but found Assign``. The AST has no value-producing
  assignment node: `Assign`/`IndexAssign`/`FieldAssign` are all `StmtKind`, and
  each of the four target kinds (local, `static`, field, array element) has its
  own lowering that stores without leaving the value. Compound forms, the
  narrowing cast above, and the once-only evaluation of a target's subexpressions
  all have to survive the addition, so it is a real change rather than a parser
  tweak.
- **A type *parameter* that shadows a class name.** `class Box<T>` inside a
  program that also declares a class `T` reads the declared return type `T` as
  that class, so `box.get().length()` is rejected as ``class `T` has no method
  `length` `` where Java erases the parameter to `Object` and dispatches at
  runtime. Generic code whose type parameters do not collide with a declared
  class name (the ordinary case) is unaffected.

## Modeled with a documented simplification

- **Types are not checked.** Declared types (`int`, `String`, …) are retained for
  diagnostics and for overload resolution / `/`-truncation typing, but do not gate
  execution — the runtime is dynamically typed on the fusevm value model.
  Definite-assignment and type errors that `javac` would reject may run.
  *Resolution* is checked, though: a name nothing declares is
  `javars: cannot find symbol: \`n\``, not a `null`. It used to read the unset
  cell, so `undefinedVar` printed `null` and `undefinedVar + 1` printed `null1`.
- **Class initialization is eager, not lazy.** Java initializes a class the
  first time it is used; javars runs *every* class's `static` field initializers
  and `static { … }` blocks once, before `main`, in source-declaration order
  (after seeding every static with its type's default, and after building the
  enum constants). The two differ only when one class's initializer reads
  another's static: Java would force that class's initialization first, javars
  gives whatever the declaration order already produced (the default value if
  the other class is declared later). A `static` initializer with an observable
  side effect (printing) also runs in a program that never touches its class.
- **Field *hiding* collapses to one cell.** A class's fields are resolved once,
  ancestors first, into a single per-instance map keyed by name, so a subclass
  that re-declares a field its parent already declares does not get a second
  cell — both names address the one slot the subclass's initializer last wrote.
  Java gives each declaration its own storage and selects between them by the
  reference's *static* type, so

  ```java
  class A { int n = 1; String w() { return "A" + n; } }
  class B extends A { int n = 2; String w() { return "B" + n + "/" + super.n + "/" + super.w(); } }
  B b = new B(); A a = b;
  System.out.println(b.w() + " " + a.n + " " + b.n);
  ```

  prints `B2/1/A1 1 2` on OpenJDK 26 and `B2/2/A2 2 2` here: `super.n`, `a.n`
  and the parent's own `w()` all see the subclass's value. Fixing it means
  per-declaring-class field storage plus static-type-directed selection at every
  read and write, which is a storage-model change rather than a call-site one.
  It is called out rather than papered over because the wrong answer is a
  plausible number; field hiding is also the one inheritance shape Java itself
  discourages, which is why the rest of the model is unaffected. `super.method()`
  — the far more common qualified access — is exact (see "Implemented").
- **An unqualified name that is another class's `static` field is rejected, as
  `javac` rejects it.** javars resolves an unqualified static only against the
  enclosing class and its ancestors, so reading `v` from outside the class
  declaring `static int v` reaches no declaration — and the undeclared-name
  check makes that ``javars: cannot find symbol: `v` `` rather than a `null`
  read. Measured against `openjdk 21.0.12`: `javac` answers
  `error: cannot find symbol / symbol: variable v` and both exit non-zero.
  `C.v` is the spelling that works, and it is the only one valid Java uses.
  (This entry said the opposite until the undeclared-name check landed; it is
  kept, corrected, because "unbound" was the documented behaviour long enough
  to be worth contradicting explicitly. An *uninitialized local* is still
  unbound — see below — so the two are no longer the same case.)
- **A widening conversion needs a statically known *source* type.** The
  conversion itself is performed (see "Widening primitive conversion" under
  "Implemented"), but it is emitted only when the value's own type is
  statically integral, so a value arriving from an erased generic position is
  left alone: `static <T> T id(T x)` makes `double d = id(7);` print `7`, and
  `double e = l.get(0);` on a `List<Integer>` prints `9`, where Java unboxes and
  widens both to `7.0`/`9.0`. This is the same erasure limit as the
  `s.get() / 2` entry above. Every source javars can type — a literal, a local,
  a field, an array element, a declared-return method — converts.
- **`==` on objects is reference identity; on strings it compares by value.**
  Object and array handles (`Value::Obj`) are identity-comparable, so `x == y`,
  `x == z` after `z = x`, and `obj.field == null` all behave like Java's reference
  `==`. String `==`, however, compares by *value* (Java's is identity) — this
  matches the far more common intent and avoids surprising `"ab" == "a"+"b"`.
- **A boxed primitive is a real reference, in a *statically typed* position
  only.** `Integer`, `Long`, `Short`, `Byte`, `Character`, `Float` and `Double`
  each allocate a heap object with the `java.lang` class they name, cached over
  the range JLS 5.1.7 mandates (`-128..=127`, `0..=127` for `Character`, nothing
  for the two floating classes). So `Integer a = 127, b = 127; a == b` is `true`
  and the same pair at 128 is `false`; `Integer a = 1000; Integer b = a; a == b`
  is `true`, the two names denoting one box; `Integer.valueOf(1).equals(
  Long.valueOf(1))` is `false`; and `((Object) aLong).getClass().getName()` is
  `java.lang.Long`.

  The box is emitted where Java performs a *boxing conversion* and javars can
  see the types: assigning a primitive expression into a slot declared with a
  wrapper type (a local, a field, an array element, a parameter, a `return`, a
  conditional branch), an explicit `X.valueOf(x)`, and a cast `(Integer) x`. It
  is removed again at the matching unboxing conversion. An expression javars
  cannot type statically converts neither way — which is deliberate, because
  boxing a value that is already a reference would break the aliasing above.

  **`Boolean` is deliberately not boxed.** Its cache covers `true` and `false`
  both, so every autoboxed pair Java can produce is already the same object and
  `==` on them is always `true` — the box would buy no fidelity while putting a
  heap handle where the VM tests truth, which is not a numeric surface and so
  would not unbox.

  A primitive crossing into **any** reference position boxes, not just a
  wrapper-typed one: `Object o = 1000L` and `(Object) 1000L` produce a `Long`,
  a generic slot (`Box<Long>`'s `E`) produces one, and an `Object[]` element
  produces one — so `((Object) aLong).getClass().getName()` is
  `java.lang.Long` rather than the `java.lang.Integer` a bare `Value::Int`
  reads as. The argument of `x.equals(y)` and both arguments of
  `Objects.equals` box for the same reason: `equals` compares references, so
  `Integer.valueOf(1000).equals(1000L)` is `false`, which needs the argument to
  carry the `Long` class it autoboxes into.

  A primitive entering a **collection** element or key position is boxed too,
  which is what keeps a `Map` keyed on `1`, `1.0` and `1L` at the three entries
  Java's holds rather than the one a single numeric kind collapses them to, and
  makes `List<Integer> l; l.add(128); l.add(128); l.get(0) == l.get(1)` the
  `false` Java answers. An *index* argument is not an element and stays the
  `int` it is; the list methods that take one are a closed set
  (`Compiler::list_index_arg`). A `char` element is still javars's
  one-character String — see the `Character` entry below.

  The collection *factories* — `List.of`, `Set.of`, `Map.of`, `Arrays.asList` —
  box their arguments for the same reason, so `List.of(128, 128).get(0) ==
  get(1)` is `false` and `Map.of(1, …, 1.0, …, 1L, …)` holds three entries.
  `Arrays.fill` deliberately does not: its second argument is an *array
  element*, and an `int[]` slot holds the primitive.

  What is left is every position javars cannot type at all: an expression whose
  static type erasure has thrown away converts neither way, so a value that
  reached a collection from one — an element copied out of another erased
  container, a value returned by a method javars cannot type — is stored as the
  bare primitive and compares by value. Mixing the two is safe rather than
  merely tolerable: `value_eq` compares a box against a bare value by unboxing,
  so a lookup finds a key however it got there; it is only *two* boxes that
  compare by class. The collection key index is derived so anything `value_eq`
  calls equal lands in one bucket, and the cases where the correspondence is not
  provable (a `Float` key, a magnitude past 2^53 where several `long`s round to
  one `double`) fall back to the linear scan the index replaced.
- **What `instanceof` still cannot decide is what the value model does not
  record.** The type test is exact for every shape javars names (see the
  `instanceof` entry under "Implemented"); three answers are left, and each is a
  *representation* the model shares rather than a missing branch:
  * **A lambda answers `Object` and nothing else.** The closure carries its body
    and its captures, not the functional interface it was assigned to, so
    `((Object) aCalc) instanceof Calc` is `false` where Java says `true`.
    Recording the interface means threading the assignment's target type into
    closure creation, which is a change to how lambdas are lowered rather than
    to the type test.
  * **`Long`/`Short`/`Byte`/`Float`/`Character` cannot be separated from the
    wrapper that shares their representation.** `int` and `long` are one
    `Value::Int` and `double` and `float` one `Value::Float`, so `instanceof`
    answers the common member. Java's left operand has to be a *reference*
    (`42L instanceof Long` is `error: unexpected type … required: reference`,
    not an answer), so the observable is a boxed one: after `Object o = 42L;`,
    `o instanceof Long` is `true` on `openjdk 21.0.12` and `false` here, while
    `o instanceof Integer` after `Object o = 42;` agrees at `true`. This is the
    same erasure the reference cast documents just below, decided the other way
    — a cast cannot prove itself wrong and allows the sibling, while a type test
    has to answer a boolean and answers for the member programs actually write.
  * **`new LinkedList<>()` is modeled as the mutable list an `ArrayList` is**, so
    it answers `instanceof ArrayList` `true` where Java says `false`. Every
    interface above it — `List`, `Collection`, `Iterable`, `SequencedCollection`
    — is exact.

  One further limit is about reach rather than about the answer: pattern binding
  (`x instanceof Point p`) does not parse, the right-hand side being a bare type
  name.

  The *reference cast* reads the same answer. It used to keep its own, narrower
  copy of "what class is this value" — one that named a `String`, a boxed
  primitive and a user instance and gave up on every collection — and a second,
  narrower copy of "which targets can be checked", an eleven-name `java.lang`
  list in the compiler that never emitted a check for a collection target at
  all. So `aList instanceof String` was `false` while `(String) aList` passed.
  Both copies are gone: the check reads `value_class` and
  `is_checkable_cast_target`, and a cast throws exactly when `instanceof` is
  false, `Object` and `null` aside. The `ClassCastException` message names the
  JDK's own implementation classes, including the ones that depend on the value
  rather than its kind — `ImmutableCollections$List12` at one or two elements
  and `$ListN` otherwise, `Arrays$ArrayList`, and a `subList` named for the root
  list it is a window onto.

  Two message shapes it does not reach. An **array** is declined outright rather
  than named: its element type is erased, so neither `[I` nor
  `[Ljava.lang.String;` is available, and `(String) anIntArray` passes where Java
  throws. A **`keySet()`/`values()` result** is a plain set or list here rather
  than a distinct view class, so the cast *throws* where Java throws but names
  what javars modeled it as — `java.util.LinkedHashSet` where Java says
  `java.util.HashMap$KeySet`, `java.util.Arrays$ArrayList` where Java says
  `java.util.HashMap$Values`. The control flow is right and only the class in the
  message is wrong; naming it exactly needs the view to be a shape of its own,
  which is the same change that would make it alias.
- **A `Character` in a collection is a one-character String.** The `char`
  *type* is a real 16-bit integral value (see the "`char` arithmetic" entry
  under "Implemented"), and a `char` crossing into a statically-typed reference
  slot — a `Character` variable, an `Object` one, a cast — now boxes as a real
  `Character` (see the boxing entry above), so `((Object) 'x').getClass()` is
  `java.lang.Character`. A `char` entering a **collection** element or key
  position is still stored as its one-character String, deliberately: that is
  what makes the rendering below work, and it is the one position where the two
  models still differ. That is what makes
  `System.out.println(list)` print `[p, q]` like Java's. The visible difference
  is `==` between two such values: Java compares `Character` references, javars
  compares the strings by value — the same String-`==` model above.
- **`java Foo.java` is `javac Foo.java && java Foo`, not the JDK's source-file
  mode.** The two entry points select a different class to run. JDK 21's
  source-file launcher runs the FIRST top-level class in the file; `java -cp . T`
  runs the one named on the command line. javars runs the class that declares
  `main`, wherever it sits — which matches the second, so a file whose first
  declaration is not the main class runs here and is refused by the reference:

  ```
  $ cat T4.java
  enum Color { RED, GREEN }
  public class T4 { public static void main(String[] a) { System.out.println("T4 ran " + Color.RED); } }

  $ java T4.java                        # openjdk 21.0.12
  error: can't find main(String[]) method in class: Color
  $ javac T4.java && java -cp . T4
  T4 ran RED
  $ java T4.java                        # javars
  T4 ran RED
  ```

  The divergence is in the permissive direction — javars accepts a file the
  reference refuses, never the reverse — and no observable answer differs for a
  file the reference accepts. It matters mostly to the harness: `tests/parity.rs`
  replays the frozen corpus through `java T.java`, while
  `scripts/capture-parity.sh` captured it through `javac` + `java -cp . T`, so
  the script now requires a new record to produce the same output under BOTH
  before it is written. Four records predating that check declare an `enum`
  first, so the reference's source launcher prints nothing for them and only
  javars's reading produces what they froze; they are kept — the *behaviour*
  they pin (enum constants, bodies, `values()`, an empty enum) is right under
  the entry point that ran them — and the same four programs are in the corpus a
  second time with `public class T` declared first, which both entry points
  agree on.
- **`String.format` uses the root locale, always.** javars has no locale model:
  `%,d` groups with `,` and `%,.2f` separates with `.` whatever
  `Locale.getDefault()` is. The reference agrees on this machine (`en_US`) and
  disagrees under another — `java -Duser.language=de -Duser.country=DE` renders
  `String.format("%,d", 1234567)` as `1.234.567` where javars keeps
  `1,234,567`. javars accepts no `-D` option, so a program cannot ask for the
  other behaviour; the gap is only reachable by changing the machine's locale.

  Four frozen records depend on it — the `%,d`, `%e|%E`, `%.0f,%.0f,%.3f` and
  `%08.2f` cases — measured by replaying the whole corpus through
  `java -Duser.language=tr -Duser.country=TR`, where those four and only those
  four change (`1.234.567`, `1,234568e+03`, `-1,500`, `00003,50`). They were
  captured under `en_US`, which happens to agree with the root locale, so they
  are correct today; a re-capture on a machine set to another locale would have
  frozen the wrong separators. Both harnesses now pin the oracle to
  `Locale.ROOT` (`-Duser.language= -Duser.country=`) and *measure* that the pin
  took, for the same reason they measure the `Double.toString` version. The
  locale of the machine is therefore no longer an input to either.
  `toUpperCase`/`toLowerCase` do not need the pin: they are locale-sensitive in
  Java (the Turkish `i`/`İ`), but javars models only the root mapping and no
  frozen record exercises the difference.
  This entry previously claimed that "a class-instance handle, an enum, a record
  and a modeled JDK type all report correctly" from `getClass().getName()`.
  Half of that was false: only the *user* shapes reported. Measured on
  `openjdk 21.0.12`, `new ArrayList<>().getClass().getName()`,
  `"s".getClass().getName()` and `((Object) 1).getClass().getName()` each
  printed the empty string here against `java.util.ArrayList`,
  `java.lang.String` and `java.lang.Integer` there — `getClass()` answered from
  the virtual-dispatch builtin, which names only user instances, while
  `binary_name` (reached only from the `ClassCastException` message) already
  knew every one of those answers. The two share one path now, so a modeled JDK
  type, the private collection classes (`ImmutableCollections$List12`,
  `Arrays$ArrayList`, `ArrayList$SubList`), and arrays all report the JDK's own
  spelling. An array's descriptor comes from the receiver's *static* type, since
  its element type is erased at runtime — so `getClass()` on a value whose
  static type javars could not infer to be an array still answers the empty
  string, and `getSimpleName()` of one decodes the descriptor back
  (`[I` → `int[]`).
- **A `ClassCastException` message drops the module-and-loader clause for a user
  class, and a cast javars cannot decide passes through.** Reference casts *are*
  checked (see "Checked reference casts" under "Implemented"), with two bounded
  gaps. Java appends a parenthetical naming each class's module and loader, and
  for a user class that clause is not even a property of the program — it is a
  property of how the JVM was *launched*. Measured on `openjdk 21.0.12`, one
  source file, one cast:

  ```
  java CC.java          … (CC$A is in unnamed module of loader
                            com.sun.tools.javac.launcher.Main$MemoryClassLoader @5e25a92e; …)
  javac CC.java && java -cp . CC
                        … (CC$A is in unnamed module of loader 'app'; …)
  ```

  An identity hash that changes per run, and a loader name that changes per
  entry point. javars has a counterpart for neither, so the message ends after
  `class X cannot be cast to class Y`; between two `java.lang` types, where the
  clause is the fixed `are in module java.base of loader 'bootstrap'`, the whole
  message is exact. The head is exact in every case, nested classes included —
  `class CC$A cannot be cast to class java.lang.String`.

  The second gap: a value whose class javars erases — an array (element type
  gone), a collection, a lambda — passes any cast rather than inventing a
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
  both sides. The same gap narrows the conversion type check: an argument whose
  static type the compiler could not infer is classified from its runtime value,
  which reads every integer as `Integer` and every float as `Double` — such an
  argument still throws exactly where Java throws, but a `Long`/`Float`/
  `Character` one names the wrong class in the detail message.
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
  assignment yields `null` instead of a compile error. Measured: `int n;
  System.out.println(n);` prints `null` and exits 0 here, where `javac` answers
  `error: variable n might not have been initialized`. Definite-assignment
  analysis is what would be needed, and javars has none.
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

  "Keeps the operation half of the wording" holds for a field read, a field
  assignment, an array element and an array length, and it is *not* true of
  three sites, each measured on `openjdk 21.0.12`:

  | program | JDK 21 | javars |
  | --- | --- | --- |
  | `((Supplier) null).get()` | `Cannot invoke "java.util.function.Supplier.get()" because …` | `Cannot invoke a functional interface method because the target is null` |
  | `for (int v : (List<Integer>) null)` | `Cannot invoke "java.util.List.iterator()" because …` | `Cannot iterate over a null reference` |
  | `((List) null).add(1)` | `Cannot invoke "java.util.List.add(Object)" because …` | `Cannot invoke "add()" because the receiver is null` |

  Those three sentences are javars's own, not a truncation of Java's. Two more
  are truncations that lose more than the provenance clause: a `String` method
  omits the erased parameter list Java prints (`Cannot invoke
  "String.substring()"` against Java's `"String.substring(int)"`), and a
  receiver whose static type is `Object` is still named `String`
  (`((Object) null).hashCode()` reports `"String.hashCode()"` where Java reports
  `"Object.hashCode()"`). All five need the compiler to hand the receiver's
  static type and the callee's erased signature to the raising site, which it
  does not do today.
- **`Arrays.copyOfRange` with a `from` outside the source omits the message
  text.** Java's bounds check for that case happens inside `System.arraycopy`,
  whose message names the element type — `arraycopy: source index -1 out of
  bounds for int[3]` on `openjdk 21.0.12`. javars erases element types at
  runtime, so it raises the right class (`ArrayIndexOutOfBoundsException`) with
  no detail message rather than inventing a type name. The two checks
  `copyOfRange` performs itself are exact: a reversed range is
  `IllegalArgumentException: 2 > 1`, and a negative length to `copyOf` is
  `NegativeArraySizeException: -1`.
- **An enhanced `for` over a collection iterates a snapshot, so structurally
  modifying it does not raise `ConcurrentModificationException`.** Java's
  iterators are fail-fast on `modCount`; javars copies the elements up front and
  walks the copy, so the loop neither sees the change nor objects to it.
  Measured on `openjdk 21.0.12` against a two-element `ArrayList`:

  ```
  for (int x : l) l.add(9);      JDK: ConcurrentModificationException
                                 javars: completes, size 4
  for (int x : l) l.remove(0);   JDK: completes, size 1  (the check is
                                       skipped when hasNext() goes false early)
                                 javars: completes, size 0
  ```

  The second line is the sharper one: Java does not throw there either, so
  "always throw" would be as wrong as never throwing. Both answers follow from
  the snapshot, and matching Java needs a real iterator with `modCount`, which
  is also what `List.iterator()` would need — that method is unimplemented
  today, so no program can hold an iterator across a modification in the first
  place. A `subList` view *does* raise `ConcurrentModificationException`,
  because it holds a live window rather than a copy.
- **Unboxing a `null` wrapper yields `null` instead of throwing.** `Integer a =
  null; int v = a;` prints `null` here and throws
  `NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because
  "<local1>" is null` on `openjdk 21.0.12`. javars has one integral value kind
  and no unboxing conversion to hang the check on, so the `null` flows into the
  `int` local unchanged. This is a missing *exception*, not a wrong message.
- **`Double.parseDouble` rejects a hexadecimal literal.** Java's grammar admits
  `HexFloatingPointLiteral` (`0x1p3` is 8.0); javars validates only the decimal
  form and answers `NumberFormatException` for the hex one. Correctly-rounded
  hex-float parsing is real work and Rust's `str::parse` does not do it either,
  so the form is refused outright rather than approximated — the same choice the
  transcendentals entry makes. No frozen record exercises it.
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

## What the harnesses cannot see

Every entry above is a divergence somebody *found*. This section is the other
half: what the three harnesses are structurally incapable of reporting, so that
a gap's absence from this file is not mistaken for evidence that it does not
exist. Each row is a property of the harness's own construction, not a bug in
it.

The three are `tests/parity.rs` (replays the frozen corpus, no JDK needed),
`scripts/capture-parity.sh` (writes the corpus from a real JDK), and
`src/bin/parity_fuzz.rs` (generates programs and diffs the two live).

| Harness | Cannot report | Why |
| --- | --- | --- |
| all three | **stderr text** | Every comparison reads stdout only (`parity.rs` `run`, the capture script's `2>/dev/null`, `RunOut { stdout, ok }`). A stack trace, a `printStackTrace()`, or an uncaught throwable's report is invisible unless the program catches it and prints the message itself. |
| all three | **the exit *code*** | Only "exited 0 or not" survives (`status.success()`, `rc != 0`). Java's `System.exit(3)` versus `System.exit(1)` is one bit here. |
| all three | **`javac` diagnostics** | No harness compares a rejection *message*. The capture script drops a program `javac` refuses; the fuzzer counts it a skip; the corpus can only hold programs that ran. Every "javars: cannot find symbol" wording in this file was checked by hand. |
| all three | **anything needing a JVM flag** | The launcher is invoked with the source file and (for the oracle) the locale pin. No `-ea`, no `-g`, no `-Xss`, no `--enable-preview`, no classpath beyond the working directory. |
| all three | **more than one source file** | One `T.java` per run. Packages, imports of a second unit, and split compilation are unreachable by construction. |
| all three | **stdin** | The fuzzer nulls it; the other two inherit a terminal. `Scanner`/`System.in` has no probe. |
| `parity.rs` + capture | **empty output** | A record is written only when stdout is non-empty, and the replay asserts a frozen string, so "prints nothing" cannot be frozen as the expected answer. |
| `parity.rs` + capture | **a non-zero exit** | Rejected at capture time. An uncaught exception's exit status is therefore never frozen. |
| `parity.rs` + capture | **trailing blank lines** | `$(...)` strips them and the script puts exactly one back, so the number of trailing newlines is pinned to 1 for every record. |
| `parity.rs` + capture | **a multi-line program** | The two halves decode the record's *program* field differently. `capture-parity.sh` expands `\n` to a newline before writing `T.java` (`perl -pe 's/\\n/\n/g'`), while `parity.rs` `run` writes `src` verbatim — so a program containing `\n` compiles under the capture and reaches `javac` as a literal backslash-n under the replay. The script will happily emit such a record, and it then fails the replay it was captured for. No record has ever contained one (checked: 0 of the 323 that predate round 7), so the convention is single-line programs, and every round-7 addition was rewritten onto one line rather than relying on it. Making the two agree means changing the capture script, which is measurement infrastructure and out of scope for the round that found it; it is noted here rather than patched alongside the work it measures. |
| `parity.rs` | **the JDK's source-launcher rule** | It replays through `javars T.java`, which is `javac` + `java T` (see the entry above), while the capture script's second entry point is the JDK's real `java T.java`. Four records predate the check that the two agree; they declare an `enum` first, so a real JDK runs the enum and prints nothing where the record says otherwise. The gate rejects any new record of that shape. |
| fuzzer | **a mutual timeout** | A program both sides hang on yields `ok=false` and empty stdout on both, which `differs` reads as agreement. |
| fuzzer | **generator-visible axes only** | A divergence has to be *generated* to be found. The pool is one `public class T` with a fixed set of support classes; anything not in a generator is not tested, and the honest way to find those is this table rather than a clean sweep. Two were found this way: no probe rendered a user `toString()` through a collection (closed by the `render` mode), and no probe put a heap element in a collection at all — every element was an `Integer` or a `String`, which javars compares structurally, so the whole `equals` axis was invisible (closed by the `equals` mode). |
| fuzzer | **non-determinism it pins away** | Identity hashes, `HashSet` iteration of user objects, and lambda `toString` are kept out of the generators on purpose, because they have no reproducible answer. That is correct, and it also means those three are permanently the hand-checked kind. |
