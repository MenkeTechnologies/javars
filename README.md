```
     ██╗ █████╗ ██╗   ██╗ █████╗ ██████╗ ███████╗
     ██║██╔══██╗██║   ██║██╔══██╗██╔══██╗██╔════╝
     ██║███████║██║   ██║███████║██████╔╝███████╗
██   ██║██╔══██║╚██╗ ██╔╝██╔══██║██╔══██╗╚════██║
╚█████╔╝██║  ██║ ╚████╔╝ ██║  ██║██║  ██║███████║
 ╚════╝ ╚═╝  ╚═╝  ╚═══╝  ╚═╝  ╚═╝╚═╝  ╚═╝╚══════╝
```

[![CI](https://github.com/MenkeTechnologies/javars/actions/workflows/ci.yml/badge.svg)](https://github.com/MenkeTechnologies/javars/actions/workflows/ci.yml)
![Rust](https://img.shields.io/badge/Rust-2021-05d9e8?style=flat-square)
![license](https://img.shields.io/badge/license-MIT-ff2a6d?style=flat-square)
![status](https://img.shields.io/badge/status-active%20%C2%B7%20in%20development-9b5de5?style=flat-square)

### `[JAVA, COMPILED TO BYTECODE — JIT-COMPILED, NOT WALKED — NO JVM]`

> *"The JVM runs Java on the JVM. javars runs Java on fusevm."*

**Java in Rust** — a Java frontend that lexes and parses Java source, lowers it
to [`fusevm`](https://github.com/MenkeTechnologies/fusevm) bytecode, and runs it
on the shared three-tier Cranelift JIT — the same engine behind `zshrs`,
`stryke`, `awkrs`, `elisp`, and `ruby`. No bespoke VM. No JVM. No `.class`
files.

---

## Table of Contents

- [\[0x00\] Overview](#0x00-overview)
- [\[0x01\] Install](#0x01-install)
- [\[0x02\] Usage](#0x02-usage)
- [\[0x03\] Language Features](#0x03-language-features)
- [\[0x04\] Command-Line Flags](#0x04-command-line-flags)
- [\[0x05\] Architecture](#0x05-architecture)
- [\[0x06\] Status & Roadmap](#0x06-status--roadmap)
- [\[0xFF\] License](#0xff-license)

---

## [0x00] OVERVIEW

Every Java runtime in existence targets the JVM: `javac` emits `.class`
bytecode, and a JVM (HotSpot, OpenJ9, GraalVM) interprets and JIT-compiles it.
`javars` takes a different path — it lexes and parses Java to an AST, lowers
that AST **directly to fusevm bytecode**, and runs it on fusevm's compiled VM
with a Cranelift tracing JIT. javars carries no VM or JIT of its own; it is a
pure frontend over the shared engine. Highlights:

- **Compiled, not tree-walked** — arithmetic, comparisons, and control flow
  lower to native fusevm ops (`LoadInt`, `Add`, `NumLt`, `JumpIfFalse`, …), so
  the tracing JIT compiles hot loops to native code.
- **fusevm-hosted, no JVM** — no local `vm.rs` / `jit.rs`, no `.class` files, no
  `libjvm`. The same three-tier Cranelift engine that hosts zshrs, stryke,
  awkrs, elisp, and ruby runs Java too. `jit-disk-cache` persists native code
  across runs.
- **Java print semantics** — `System.out.print[ln]` lowers to a formatting
  builtin so `boolean` prints `true`/`false`, `double` prints `3.0`, and `null`
  prints `null` — matching `java`, not the VM's shell-flavoured default.
- **Java `+` overloading** — a strict numeric hook supplies string
  concatenation (`"x=" + x`) for the mixed operands the VM's native arithmetic
  does not compute, while all-numeric arithmetic stays on the JIT fast path.
- **Verified against OpenJDK** — the example programs and the test corpus are
  diffed byte-for-byte against a reference `java`; the tests freeze that output
  so CI needs no JDK installed.

Covered today: locals, arithmetic, the C-style control statements,
`System.out.print[ln]`, user-defined `static` methods (recursion, parameters,
value returns), `String` instance methods, **reference arrays** including
**multi-dimensional** (`new int[n]`, `new int[m][n]`, `{…}` and `{{…},{…}}`
literals, indexing, `.length`, and true pass-by-reference/aliasing on a
host-owned object heap), and a **class/object model** (fields, constructors,
instance methods, `this`, `new`, field access, single inheritance with
`extends`, `super(…)` chaining and non-virtual `super.member` access,
`instanceof`, virtual method dispatch, and `toString()`
overrides). **Interfaces** (abstract + `default` methods, multiple
implements, interface inheritance, polymorphic dispatch), **method overloading
by parameter type** (most-specific resolution for methods and constructors,
including Java's variable-arity phase so `T...` parameters take loose
arguments), and
**type-erased generics** (`class Box<T>`, `<T> T id(T x)`, bounded `<T extends
X>`, the diamond, erased library type args) all run. `String.format` and
`System.out.printf` (a `Formatter` subset covering `%d %s %S %f %e %E %g %G %b
%B %h %H %x %X %o %c`, the `-`/`0`/`+`/`,`/`(` flags, width, precision, and
argument indexes) and the `Arrays` statics round out the stdlib essentials; the
wider standard library is the next wave (see [`BUGS.md`](BUGS.md)). **Exceptions**
(`throw`/`try`/`catch`/`finally`, try-with-resources, and javars's own runtime
faults raised as catchable throwables), **`static` fields and `static { }`
blocks**, **`record` types**, **abstract classes**, and **`enum` types** — down
to per-constant state (`EARTH(5.97e24)`) and per-constant bodies — run too, and
`main`'s `args` carries the real program arguments. An unsupported construct is
a parse or compile error rather than a silent mis-run: there is no construct
javars accepts and then runs with the wrong meaning (see [`BUGS.md`](BUGS.md)).
A program `javac` itself rejects is rejected here too where javars can see it —
an undeclared name, and a class, field, method, constructor, `enum` constant, or
parameter declared twice, each reported at the duplicate in `javac`'s own
wording rather than resolved silently to one of the two.

---

## [0x01] INSTALL

```sh
git clone https://github.com/MenkeTechnologies/javars
cd javars
cargo build

# run a .java file
./target/debug/java examples/FizzBuzz.java
```

`javars` is a standalone Rust crate (an explicit empty `[workspace]` keeps it
independent of the meta repo). `fusevm` is pulled from crates.io with the `jit`,
`jit-disk-cache`, `aot`, and `ffi` features, and `fancy-regex` supplies the
backtracking engine behind `java.util.regex`. Run the tests with `cargo test`
(no JDK required).

#### Zsh tab completion

```sh
cp completions/_java /usr/local/share/zsh/site-functions/_java
# or: fpath=(/path/to/javars/completions $fpath) in .zshrc
autoload -Uz compinit && compinit
```

---

## [0x02] USAGE

```java
public class FizzBuzz {
    public static void main(String[] args) {
        for (int i = 1; i <= 15; i++) {
            if (i % 15 == 0) {
                System.out.println("FizzBuzz");
            } else if (i % 3 == 0) {
                System.out.println("Fizz");
            } else if (i % 5 == 0) {
                System.out.println("Buzz");
            } else {
                System.out.println(i);
            }
        }
    }
}
```

```sh
$ java FizzBuzz.java
1
2
Fizz
4
Buzz
...
```

---

## [0x03] LANGUAGE FEATURES

Implemented and checked against the reference `java`:

- **Entry point** — `public class Name { public static void main(String[] args) { … } }`.
  Every other member — `static` helpers, `static` fields, instance fields,
  constructors, instance methods — is compiled too. `args` is bound to the real
  program arguments (`java Prog.java a b`), and to a zero-length `String[]`
  (never `null`) when none are passed.
- **`static` fields** — one cell per class, seeded with the declared type's
  default and then initialized by the field initializers and `static { … }`
  blocks in textual order, all before `main`. Reached unqualified inside the
  declaring class, as `C.n` from anywhere, and through an inheriting class;
  compound assignment and `++`/`--` write the same cell, and the field's
  declared type drives `/`-truncation and the 32-bit `int` wrap.
- **`record` types** — `record Pt(int x, int y) { … }` derives the final fields,
  the canonical constructor, an accessor per component, `toString()` in Java's
  `Pt[x=1, y=2]` form, and a component-wise `equals`. A compact constructor
  validates before the fields are assigned; a member the body declares itself
  wins over the derived one.
- **`toString()` overrides, wherever a value renders** — `println(obj)`,
  `"x " + obj`, `obj.toString()`, `String.valueOf(obj)`,
  `Arrays.toString`/`deepToString`, `String.join`, `String.format("%s", obj)` /
  `"%s".formatted(obj)`, and every element of a `List`/`Set`/`Map` at any depth,
  whatever the receiver's *static* type — an `Object`, an erased `get()`, a map
  value. A subclass that declares none inherits its ancestor's body. The
  override is real code: it may print (its output comes first, before the
  `println` it was rendering for) and it may throw (the throwable propagates,
  and no half-built text reaches the stream). A program that declares no
  override compiles to byte-identical bytecode.
- **Nested type names** — `getClass().getName()`, the default `Class@hash`
  rendering, and a `ClassCastException`'s head all spell Java's binary name for
  a nested declaration (`Outer$Nested`, `A$B$C` when doubly nested);
  `getSimpleName()` stays the simple one.
- **Abstract classes** — `abstract class Shape { abstract double area(); … }`,
  with `super(…)` chaining and concrete methods that call the abstract one
  (resolved to the subclass's override at runtime).
- **Locals** — `int` / `long` / `double` / `boolean` / `String` / `var`
  declarations with optional initializers; plain and compound assignment
  (`=`, `+=`, `-=`, `*=`, `/=`, `%=`); post-increment / post-decrement
  (`i++`, `i--`). A `var` records the *type* it infers, so `var i = 7; i / 2`
  truncates and `var big = 100000; big * big` wraps — including the element type
  of a `var` enhanced-`for` over an array literal.
- **Multi-declarator declarations** — `int a = 1, b = 2;` in statement position,
  in a `for` init clause, and as a field. Declarators are evaluated left to
  right, so a later one may read an earlier (`int a = 1, b = a + 1;`), and any of
  them may be left uninitialized (`int a, b = 2, c;`). The C-style array suffix
  binds to the *declarator* rather than the type, exactly as Java specifies, so
  `int p[] = {1}, q;` declares an `int[]` and an `int` — the suffix is accepted
  on locals, fields, and parameters (`static int add(int xs[], int n)`).
- **`java.lang.Object`** — `new Object()` is the fieldless root instance, with a
  distinct identity per allocation: it works as a lock, as a sentinel, and as a
  map key or set element. The methods every class inherits and does not override
  answer from `Object` — `equals` is reference identity, `getClass().getName()`
  is `java.lang.Object`, and `toString()` is the `java.lang.Object@<hash>` form.
  `synchronized (m) { … }` runs its body after evaluating the monitor once
  (javars runs one thread, so the lock itself is unobservable) and throws
  `NullPointerException` for a `null` monitor.
- **Expressions** — integer (decimal, `0x`, `0b`, octal, `_`-separated) /
  floating / string / char / boolean literals; the binary operators `+ - * / %`,
  `== != < > <= >=`, `&& ||` (short-circuiting), the bitwise `& | ^` (Java's
  non-short-circuiting logical operators on booleans), and the shifts
  `<< >> >>>` with Java's per-width distance masking; unary `-`, `!`, `~`, and
  `++`/`--` in both prefix and postfix value position; cast expressions with
  Java's saturating and two's-complement narrowing conversions (a *reference*
  cast is checked against the receiver's runtime class and throws
  `ClassCastException`); parenthesised
  grouping; Java's `+` string concatenation. A `char` is the 16-bit *integral*
  type it is in Java — `"abc".charAt(2) + 1` is 100 and `c - '0'` reads a digit
  — and takes Java's string conversion back to a one-character String wherever
  one applies (`println`, `+` with a String, `String.valueOf`, a `String`-method
  argument). A compound assignment narrows back to its target's width, so
  `byte b = 100; b += 100;` is -56. `float` is Java's 32-bit type rather than an
  alias for `double`: it narrows at every operation (Java rounds *once* at 32
  bits, so the operation runs on the host rather than being computed in 64 bits
  and narrowed afterwards) and prints the shortest decimal that round-trips at 32
  bits, so `1.0f / 3.0f` is `0.33333334`. Both `toString`s follow the
  specification rather than a plain shortest-round-trip search, which differs at
  the subnormal floor: the candidate set widens to two digits whenever the
  shortest form has one, so `Double.MIN_VALUE` prints `4.9E-324` and
  `Float.MIN_VALUE` prints `1.4E-45`.
- **Control flow** — `if` / `else if` / `else`, `while`, the C-style
  `for (init; cond; update)` (with comma-separated init and update clauses), the
  enhanced `for (T x : arr)` (over an array or a collection), `break`,
  `continue`, and `return` (a bare `return;` ends `main`; `return <expr>;`
  returns a value from a method).
- **Arrow `switch`** — as an expression
  (`int r = switch (x) { case 1, 2 -> 10; default -> { … yield v; } };`) and as
  a statement. Multi-label arms, `enum`/`String`/`int` discriminants, a `throw`
  arm, and a block arm whose `yield` runs any `finally` it leaves. The classic
  colon form, with its fall-through, still works unchanged.
- **Exceptions** — `throw`, `try` / `catch` / `finally`, try-with-resources,
  multiple `catch` arms matched by class, and the modeled `java.lang` throwable
  hierarchy (`RuntimeException`, `IllegalArgumentException`,
  `NumberFormatException`, …) supplied as an implicit prelude. A throw unwinds
  real fusevm call frames into the caller's handler; an uncaught one reports and
  exits non-zero. A `return`/`break`/`continue` leaving a guarded block runs the
  `finally` on its way out, and so does an exception raised inside a `catch` arm.
- **`enum` types** — constants as singletons with reference identity,
  `name()`/`ordinal()`/`toString()`/`equals`, `values()`/`valueOf`, unqualified
  `switch` labels, enum bodies with their own methods, and `implements`. A
  constant may carry constructor arguments (`EARTH(5.97e24)`) for per-constant
  state, or a body (`PLUS { int apply(int a, int b) { … } }`) — compiled to the
  synthetic subclass Java specifies it as, so its override (including of the
  enum's own `abstract` method) dispatches on the runtime class.
- **Catchable runtime faults** — javars's own faults are Java exceptions rather
  than aborts: an out-of-range array index, a null receiver, `Integer.parseInt`
  on junk, integral `/ 0` and `% 0`, a negative array size, and the `String`
  index/argument faults all raise the throwable Java raises, with Java's detail
  message, catchable by any supertype.
- **`int` width** — Java's 32-bit `int` wrapping, for operations whose operand
  types are statically `int`; `long` stays 64-bit.
- **Division** — Java's binary numeric promotion: `int / int` truncates toward
  zero (`7 / 2` → `3`, `-7 / 2` → `-3`), and a `double` operand keeps the
  fractional result (`7.0 / 2` → `3.5`), decided from the operands' static types.
- **Static methods** — `static <ret> name(<params>) { … }` compiled to fusevm's
  `Op::Call` frame ABI: parameters and locals in call-frame slots, so recursion,
  mutual recursion, and forward references work; `void` and value returns; arity
  checked at compile time.
- **`String` methods** — postfix `recv.method(args)` dispatch on `String`
  receivers: `length`, `isEmpty`, `charAt`, `substring`, `indexOf`, `contains`,
  `equals`, `equalsIgnoreCase`, `compareTo`, `compareToIgnoreCase`,
  `toUpperCase`, `toLowerCase`, `trim`, `startsWith`, `endsWith`, `concat`,
  `replace`, `repeat` (chainable). `compareTo` is not one of them for a
  non-`String` receiver: every boxed primitive and every user `Comparable`
  declares it with a different answer (the sign, the arithmetic difference, or a
  user body), so it dispatches on the receiver's static type and, when that is
  erased, on its runtime class.
- **Regular expressions** — `split` (with `limit`), `replaceAll`,
  `replaceFirst`, and `matches` run real `java.util.regex` patterns on
  `fancy-regex`, whose backtracking VM is what makes Java's backreferences and
  lookaround expressible. `src/regex.rs` translates the Java source first,
  because the two flavours' *defaults* differ where it changes answers: Java's
  `\d`/`\w`/`\s`/`\b` are ASCII-only, its `(?i)` folds ASCII only, its `.`
  excludes five line terminators, and its default-mode `$` matches before a
  final one. A construct with no faithful translation (a possessive quantifier,
  an atomic group, a Unicode block) raises `PatternSyntaxException` naming it
  rather than compiling into a different language.
- **Lambdas and functional interfaces** — `() -> e`, `x -> e`,
  `(a, b) -> { … }`, and the explicitly-typed `(int a, String b) -> …`. A lambda
  compiles to a heap closure carrying a by-value snapshot of the enclosing
  locals (and `this`), because it outlives the fusevm call frame those locals
  live in — which is also what gives the enhanced `for` Java's per-iteration
  capture. The target is any interface with one abstract method, Java's own
  rule, so a user-declared `interface Calc { int of(int a); }` works with no
  registration; `Runnable`, `Supplier`, `Consumer`, `Function`, `BiFunction`,
  `Predicate`, `Comparator`, `UnaryOperator`/`BinaryOperator` and the `Int*`
  shapes are supplied as one-method interfaces in the prelude. A
  functional-interface variable may hold a lambda *or* a class instance; the
  runtime-class dispatch chain routes each. `return`, `break`/`continue`,
  `try`/`finally` and `throw` all work inside a lambda body.
- **Method references** — `String::length`, `Integer::parseInt`, `Point::area`,
  `obj::method`, `this::method`, `Point::new`, `System.out::println`.
- **`java.util` collections** — `List`/`ArrayList`, `Map`/`HashMap`/
  `LinkedHashMap`/`TreeMap`, `Set`/`HashSet`/`LinkedHashSet`/`TreeSet`, the copy
  constructors, `Arrays.asList`, `List.of`/`Set.of`, and
  `Collections.sort`/`reverse`/`max`/`min`. `Arrays.asList` is fixed-size and
  `List.of`/`Set.of` immutable, so a structural write to one throws
  `UnsupportedOperationException` as Java's does, and neither factory answers
  `instanceof` as the mutable kind it is not. They are heap objects like arrays, so
  reference semantics hold; the enhanced `for` iterates them; `sort` and
  `forEach` take a lambda. A sort is a stable merge sort driven by the
  comparator; naming none (`Collections.sort(l)`, `l.sort(null)`) orders by the
  element's own `compareTo`, and `Comparator.naturalOrder`/`reverseOrder`/
  `comparing` build that comparator explicitly. `HashMap`/`HashSet` iterate in Java's **real bucket
  order** — `(capacity - 1) & (h ^ (h >>> 16))` over a power-of-two table,
  reproduced exactly rather than approximated with insertion order.
- **Output** — `System.out.println(x)` / `System.out.print(x)` with Java value
  formatting.
- **Inline Rust FFI** — a `rust { pub extern "C" fn … }` block inside `main`
  compiles to a cached cdylib whose exported functions are callable by name
  (via `fusevm::ffi`); see [`examples/Ffi.java`](examples/Ffi.java).
- **Comments** — `//` line, `/* … */` block.

---

## [0x04] COMMAND-LINE FLAGS

| Flag | Effect |
| --- | --- |
| `FILE [args…]` | Run a `.java` file. |
| `-version` / `--version` | Print the version banner and exit. |
| `-h` / `--help` | Print usage and exit. |
| `--dump-tokens FILE` | Print the lexer token stream and exit. |
| `--dump-ast FILE` | Print the parsed AST and exit. |
| `--disasm FILE` | Print the lowered fusevm bytecode and exit. |
| `--tiers FILE` | Run it, then report which fusevm execution tier took each of its chunks. |
| `--lsp` | Speak the Language Server Protocol over stdio (completion, hover, diagnostics). |
| `--dap` | Speak the Debug Adapter Protocol over stdio (breakpoints, stepping, locals). |

### Editor tooling

`java --lsp` runs a read-only language server: completion and hover from the
language-reference corpus in `src/reference.rs` — every keyword, operator, type,
library method, throwable, functional interface, synthesized class member,
`String.format` conversion, and runtime builtin the build implements, each with
its signature, a description, and an example — and diagnostics from the runtime's
own parser, where a syntax error maps to a diagnostic on the reported line. The
same table generates
[`docs/reference.html`](https://menketechnologies.github.io/javars/reference.html),
so the editor and the published reference cannot drift apart.

`java --dap` runs a Debug Adapter over stdio: line breakpoints, single-stepping
(`next` / `stepIn` / `stepOut` advance to the next statement in the single `main`
frame), a `stackTrace` of the current frame, and `variables` inspection of
`main`'s locals. The program is compiled with per-statement line markers only in
this mode; `System.out` output is captured and forwarded as `output` events so it
never corrupts the protocol channel.

`java --version` reports the targeted language level (`java 21`) followed by the
real engine (`javars <crate-version>`) and the host triple, so nothing is
misrepresented as the JDK.

### Inline Rust FFI

A `rust { … }` block inside `main` embeds native Rust in a Java program:

```java
public class Ffi {
    public static void main(String[] args) {
        rust { pub extern "C" fn j_triple(x: i64) -> i64 { x * 3 } }
        System.out.println(j_triple(14));   // => 42
    }
}
```

Before lexing, the block is rewritten to a `__rust_compile("<base64>", line)`
call; the runtime hands the base64-encoded body to `fusevm::ffi`, which compiles
it to a cdylib (cached by content hash) and registers its `pub extern "C"`
exports. A bareword call whose name is not a local resolves to an FFI export by
name — but only when the program contains a `rust { … }` block; without one, an
unknown call stays an ordinary `unresolved reference` compile error.

---

## [0x05] ARCHITECTURE

javars contains no virtual machine or JIT of its own. The execution path
mirrors how `zshrs` hosts zsh and `ruby` hosts Ruby:

```
Java source → lexer → parser (AST) → lower to fusevm bytecode → fusevm VM + Cranelift JIT
                                            │
                              strict numeric hook (Java `+` concat)
                              print builtins (Java value formatting)
```

| Piece | How |
| --- | --- |
| **fusevm-hosted** | No local `vm.rs` / `jit.rs`, no JVM. Java lowers to fusevm bytecode and runs on the shared three-tier Cranelift JIT; `jit-disk-cache` persists native code across runs. |
| **Native arithmetic** | Operators lower to native fusevm ops; the JIT traces hot integer loops. A strict numeric hook supplies Java's `+` string concatenation only for non-numeric operands. |
| **Java print semantics** | `System.out.print[ln]` lowers to a registered builtin that formats values Java-style (`true`/`false`, `3.0`, `null`), rather than the VM's shell-flavoured `PrintLn`. |

---

## [0x06] STATUS & ROADMAP

This release: `main`, locals (including multi-declarator statements and the
C-style array suffix, `int a = 1, b[] = {2};`), arithmetic / comparison / logic, Java
integer-vs-float division, the ternary `?:` operator, `if` / `while` /
`do`-`while` / `for` / `switch` (with fall-through, on `int` and `String`) /
`break` / `continue` (including labeled `break outer;` / `continue outer;`) /
`return`, the bitwise and shift operators (`& | ^ ~ << >> >>>` and their
compound forms), cast expressions, `++`/`--` in value position, `System.out`/`System.err` `print[ln]`, string concatenation,
user-defined `static` methods (recursion, parameters, value returns over
fusevm's `Op::Call` frame ABI), `String` instance methods, a first slice of the
standard library (`Math.*`, the `Integer`/`Long`/`Double` parsing, radix, and
constant statics, `Boolean.parseBoolean`, the `Character` predicates,
`String.valueOf`/`join`/`format`, `System.out.printf`, and the `Arrays`
statics including `sort`/`fill`/`copyOf`/`deepToString`), **reference arrays** including **multi-dimensional**
(default-valued `new T[n]` / `new T[m][n]`, `{…}` and nested `{{…},{…}}` literals,
get/set indexing, `.length` at each level, and reference/aliasing semantics on a
host-owned object heap keyed by `Value::Obj`), a **class/object model** (instance
fields with initializers, constructors, instance methods dispatched over the
frame ABI, `this`, `new`, field access `obj.f`, single inheritance with `extends`
and `super(…)`, the non-virtual `super.method(args)`/`super.field` access an
override uses to reach the version it overrides, `instanceof` over every shape
the value model names (user classes, boxed primitives, collections, arrays,
`enum` as `Enum` and `record` as `Record`) with the checked reference cast
reading that same answer, runtime-class
virtual dispatch for overrides, and
`toString()` overrides honoured wherever a value renders), **interfaces** (abstract +
`default` methods, multiple `implements`, `interface extends`, polymorphic
dispatch through an interface type), **method overloading by parameter type**
(most-specific resolution for methods and constructors, plus Java's third —
variable-arity — phase, so a `T...` parameter is callable with loose arguments
at every call site), **type-erased
generics** (`class Box<T>`, `<T> T id(T x)`, bounded `<T extends X>`, the
diamond), **`static` fields** with `static { }` initializer blocks, **`record`
types** with their derived accessors / `toString` / `equals`, **abstract
classes**, **`enum` constants carrying state or bodies**, and **lambdas +
method references** (heap closures capturing by value, dispatched through any
single-abstract-method interface), and the **`java.util` collections**
(`List`/`Map`/`Set` with Java's real `HashMap` bucket iteration order, and
`List.subList` as a genuine aliasing view — writes cross in both directions and
a backing list modified behind it raises `ConcurrentModificationException`),
**arrow `switch` expressions** with `yield`, and **`java.lang.Object`**
(`new Object()` as a lock or sentinel, plus the `equals`/`hashCode`/`toString`/
`getClass` every class inherits from it) — all verified byte-for-byte against
OpenJDK.

The object heap lives host-side in `src/host.rs`: `Value::Obj(u32)` is an opaque
handle into a frontend-owned slab of arrays and instances (the same pattern the
`ruby`/`node`/`php` frontends use), so identity and aliasing are real rather than
value-copied.

Next waves, in priority order:

1. **Streams** — `.stream().map(…).filter(…)`, `IntStream.range`, the
   `collect`/`reduce`/`findFirst` terminals. The lambdas they take already work,
   as do the functional interfaces' `default` composition methods and statics
   (`Function.andThen`, `Predicate.negate`, `Comparator.reversed`/
   `naturalOrder`/`comparing`). What is missing is the surface itself — the
   sources, the intermediate operations with Java's laziness, the terminals, and
   `Optional`. A host builtin holds `&mut VM` and can re-enter it (that is how
   `forEach`, `sort` with a comparator, and a user `toString()` already run user
   code), so a stream can be a host object driving each stage's closure per
   element; compile-time pipeline fusion is one way to build it, not a
   prerequisite. See [`BUGS.md`](BUGS.md) for which callback shapes can re-enter
   and which cannot.
2. **`switch` patterns** (`case Integer i ->`, `case null`, `when` guards),
   class literals (`C.class`), `Iterator`/`entrySet`, and wider stdlib coverage
   (more `Math`/`Integer` statics, more `String` methods, a `record`'s derived
   `hashCode`).
3. **Lazy class initialization** — javars runs every class's `static`
   initializers before `main`; Java runs each class's on first use.

See [`BUGS.md`](BUGS.md) for the honest known-gaps list.

---

## [0xFF] LICENSE

MIT — free and open source. See [`LICENSE`](LICENSE).
