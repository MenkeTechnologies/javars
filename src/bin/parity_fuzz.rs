//! Differential parity fuzzer: reference `java <file>` vs our `java <file>`.
//!
//! Generates grammar-driven, deterministic-output Java programs, runs each
//! through a real JDK and through this frontend, and reports every case whose
//! stdout OR success/failure diverges. Each program is produced from a per-index
//! seed so any divergence replays exactly: `parity-fuzz --seed <N> --once`.
//!
//! The reference JDK pays a multi-second compile+JVM cost per invocation (it
//! compiles the single file in memory before running it), so every program packs
//! many independent probe statements (`--probes`, default 40) into one `main`; a
//! single invocation therefore exercises dozens of probes. On divergence,
//! [`minimize`] bisects the probe list down to the single offending probe before
//! reporting.
//!
//! The generator is biased toward the historically weak areas of a from-scratch
//! Java frontend: `int`-vs-`double` division dispatch (`7/2==3`, `7/2.0==3.5`),
//! `+` string-concatenation coercion, `Double.toString` notation (the
//! decimal/scientific threshold), `String.format`, the `String` instance methods,
//! `Math` overload result typing, the runtime faults javars raises itself
//! (`fault`), the cleanup blocks a jump has to run (`finally`),
//! try-with-resources close ordering (`resource`), `enum` identity, ordering,
//! per-constant state and constant bodies (`enum`), class-level `static` field
//! storage and initialization order (`static`), the members a `record` derives
//! from its components (`record`), and the two-sided `char` boundary where an
//! integral code point has to do arithmetic *and* render as a character
//! (`char`), the declaration statement that declares several variables at once
//! and the C-style array suffix that binds to one of them (`decl`), and the
//! identity `java.lang.Object` supplies to a lock, a sentinel, and a map key
//! (`object`). Pure random bytes only produce
//! mutual parse errors that agree on both sides and teach nothing.
//!
//! Five modes exist specifically because a *shape* the rest of the generator
//! could not emit hid a wrong answer behind an otherwise clean sweep:
//!   * `super` — the qualified `super.method(args)` / `super.field`. Not one
//!     probe in the generator wrote `super.` before this mode existed, so the
//!     whole form was unreachable: it reached the runtime as a call on a null
//!     receiver and every sweep was clean anyway. Its support chain declares the
//!     same method at three levels, so a `super` that dispatched virtually would
//!     recurse rather than answer.
//!   * `staticref` — a **qualified** `C.m(…)` where a second, unrelated class
//!     also declares `m`. With one static name per program, a resolver that
//!     ignores the receiver is indistinguishable from a correct one; with two,
//!     it runs the wrong body.
//!   * `loopkind` — `continue`/`break` in a `while` and a `do`/`while`, labelled
//!     and unlabelled, nested, through a `switch`, and through a `finally`.
//!     `continue` lowers differently in each of Java's three loops; the older
//!     probes emitted only the `for` one.
//!   * `narrow` — `byte` and `short`, the two integral widths no probe declared,
//!     and the implicit narrowing cast Java inserts into their compound
//!     assignments.
//!   * `listop` — the methods whose *meaning* depends on an argument's static
//!     type, above all `List.remove`, which is an index for an `int` and a value
//!     for an `Integer`. A mode that only *builds* collections cannot see it.
//!
//! Scope + determinism invariants (mirroring the scalars/node-js harnesses):
//!   * Only constructs javars actually implements are emitted — an unsupported
//!     construct would be a known gap, not a parity signal.
//!   * No nondeterministic output (no `Random`, no `currentTimeMillis`, no
//!     unordered collections). Every probe's output is a pure function of its
//!     source. An identity hash is never *printed* — Java's differs between runs
//!     of the same program — only the properties that do hold across runs
//!     (`o.hashCode() == o.hashCode()`, the `java.lang.Object@` prefix).
//!   * Documented `BUGS.md` simplifications are NOT generated, because they would
//!     only reproduce known entries rather than find anything:
//!       - `NullPointerException` detail messages (Java's helpful NPE names the
//!         javac local slot, which javars cannot reproduce — the `fault` mode
//!         raises every *other* runtime fault and prints its message),
//!       - widening *value* conversion (`double d = 7;` printing `7` not `7.0`),
//!       - `==` where javars compares by value and Java by reference: two
//!         `String`s, and two boxed `Character`s (which javars models as
//!         one-character `String`s). Reference `==` between *instances* is not
//!         a simplification — it is exact, and the `object` mode asserts it,
//!       - `int` arithmetic whose operand types are not statically known (the
//!         `overflow` mode uses only statically `int`-typed operands, which is
//!         exactly the subset javars wraps).
//!
//! Subprocess-only: this binary never links the javars library — it compares two
//! `java` processes, exactly as a user would observe them.
//!
//! Build:  cargo build --bin parity-fuzz
//! Run:    ./target/debug/parity-fuzz --iters 200
//!         ./target/debug/parity-fuzz --seed 12345 --once
//!         ./target/debug/parity-fuzz --mode concat --iters 50

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// xorshift64*, seeded per program index so any case replays from its seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(if seed == 0 { 0x9E3779B97F4A7C15 } else { seed })
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn pick<'a, T>(rng: &mut Rng, xs: &'a [T]) -> &'a T {
    &xs[rng.below(xs.len())]
}

// Operand pools. These integers stay well inside `int` range so the general
// modes test arithmetic rather than wrapping; 32-bit overflow has its own
// `overflow` mode with its own operand pool.
const INTS: &[&str] = &[
    "0", "1", "2", "3", "7", "10", "42", "100", "-1", "-7", "-42",
];
const DIVS: &[&str] = &["1", "2", "3", "4", "5", "7", "-2", "-3"];
const DBLS: &[&str] = &[
    "0.0",
    "1.0",
    "0.5",
    "2.5",
    "3.14",
    "-1.5",
    "100.0",
    "1e3",
    "1e-3",
    "0.1",
    "1234567.0",
    "1.0e7",
    "1.0e-7",
    "123456789.0",
];
/// `char` literals for the `char` mode: letters at both cases, a digit, a
/// space, and `~` so the code points span the printable ASCII range.
const CHARS: &[&str] = &["a", "b", "z", "A", "Z", "0", "9", " ", "~"];
const STRS: &[&str] = &[
    "\"\"",
    "\"a\"",
    "\"abc\"",
    "\"Hello\"",
    "\" x \"",
    "\"AbC\"",
    "\"aXbXc\"",
];
const BOOLS: &[&str] = &["true", "false"];
const AOPS: &[&str] = &["+", "-", "*"];
const CMPOPS: &[&str] = &["==", "!=", "<", ">", "<=", ">="];
const LOGOPS: &[&str] = &["&&", "||"];

fn p(body: String) -> String {
    format!("System.out.println({body});")
}

/// Integer arithmetic on native ops.
fn g_arith(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let c = pick(r, INTS);
    let o1 = pick(r, AOPS);
    let o2 = pick(r, AOPS);
    p(format!("({a} {o1} {b}) {o2} {c}"))
}

/// `int` division and modulo truncate toward zero; divisors are never 0.
fn g_intdiv(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, DIVS);
    let op = if r.below(2) == 0 { "/" } else { "%" };
    p(format!("{a} {op} {b}"))
}

/// `double` arithmetic — the other side of the `/` dispatch.
fn g_doublearith(r: &mut Rng) -> String {
    let a = pick(r, DBLS);
    let b = pick(r, DBLS);
    let op = pick(r, &["+", "-", "*", "/"]);
    p(format!("{a} {op} {b}"))
}

/// Mixed int/double: the operand that forces promotion.
fn g_mixeddiv(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, DBLS);
    if r.below(2) == 0 {
        p(format!("{a} / {b}"))
    } else {
        p(format!("{b} / {a}"))
    }
}

/// `Double.toString` notation — the decimal/scientific threshold.
fn g_doublefmt(r: &mut Rng) -> String {
    // Subnormals get their own share of the pool: the bottom of the exponent
    // range is where "shortest decimal that round-trips" and Java's actual
    // specification part company (its `p < 2` two-digit widening), and
    // `Double.MIN_VALUE * k` is the only way to spell one — a literal that small
    // is a `javac` error ("floating-point number too small").
    match r.below(8) {
        0 => p(format!("Double.MIN_VALUE * {}L", r.below(64) + 1)),
        1 => p("Double.MIN_VALUE".to_string()),
        2 => p("Double.MIN_NORMAL".to_string()),
        _ => p((*pick(r, DBLS)).to_string()),
    }
}

/// `+` string concatenation and its coercion rules.
fn g_concat(r: &mut Rng) -> String {
    let s = pick(r, STRS);
    match r.below(4) {
        0 => p(format!("{s} + {}", pick(r, INTS))),
        1 => p(format!("{s} + {}", pick(r, DBLS))),
        2 => p(format!("{s} + {}", pick(r, BOOLS))),
        _ => p(format!("{s} + {} + {}", pick(r, INTS), pick(r, STRS))),
    }
}

fn g_compare(r: &mut Rng) -> String {
    let op = pick(r, CMPOPS);
    if r.below(2) == 0 {
        p(format!("{} {op} {}", pick(r, INTS), pick(r, INTS)))
    } else {
        p(format!("{} {op} {}", pick(r, DBLS), pick(r, DBLS)))
    }
}

fn g_bool(r: &mut Rng) -> String {
    let op = pick(r, LOGOPS);
    p(format!(
        "({} < {}) {op} ({} > {})",
        pick(r, INTS),
        pick(r, INTS),
        pick(r, INTS),
        pick(r, INTS)
    ))
}

/// Ternary, with both branches the same static kind.
fn g_ternary(r: &mut Rng) -> String {
    p(format!(
        "({} < {}) ? {} : {}",
        pick(r, INTS),
        pick(r, INTS),
        pick(r, INTS),
        pick(r, INTS)
    ))
}

/// `String` instance methods on ASCII receivers (astral chars are a known gap).
fn g_strmethod(r: &mut Rng) -> String {
    let s = pick(r, STRS);
    match r.below(9) {
        0 => p(format!("{s}.length()")),
        1 => p(format!("{s}.isEmpty()")),
        2 => p(format!("{s}.toUpperCase()")),
        3 => p(format!("{s}.toLowerCase()")),
        4 => p(format!("{s}.trim()")),
        5 => p(format!("{s}.indexOf(\"b\")")),
        6 => p(format!("{s}.contains(\"b\")")),
        7 => p(format!("{s}.replace(\"X\", \"-\")")),
        _ => p(format!("{s}.concat(\"z\")")),
    }
}

/// `Math` statics — Java's int-vs-double overload result typing.
fn g_math(r: &mut Rng) -> String {
    match r.below(6) {
        0 => p(format!("Math.abs({})", pick(r, INTS))),
        1 => p(format!("Math.max({}, {})", pick(r, INTS), pick(r, INTS))),
        2 => p(format!("Math.min({}, {})", pick(r, DBLS), pick(r, DBLS))),
        3 => p(format!("Math.floor({})", pick(r, DBLS))),
        4 => p(format!("Math.ceil({})", pick(r, DBLS))),
        _ => p(format!(
            "Math.sqrt({})",
            pick(r, &["0.0", "1.0", "2.0", "9.0", "16.0"])
        )),
    }
}

/// `String.format` — the `Formatter` subset javars implements.
fn g_format(r: &mut Rng) -> String {
    let f = pick(r, FLTS);
    match r.below(11) {
        0 => p(format!("String.format(\"%d\", {})", pick(r, INTS))),
        1 => p(format!("String.format(\"%s\", {})", pick(r, STRS))),
        2 => p(format!("String.format(\"%5d|\", {})", pick(r, INTS))),
        3 => p(format!("String.format(\"%-5d|\", {})", pick(r, INTS))),
        4 => p(format!(
            "String.format(\"%x\", Math.abs({}))",
            pick(r, INTS)
        )),
        // A `float` argument splits by conversion: `%s` wants `Float.toString`,
        // every numeric conversion wants the widened `double`. Both spellings
        // have to be generated or the split is only half tested.
        5 => p(format!("String.format(\"%s\", {f} / 3.0f)")),
        6 => p(format!("String.format(\"%f\", {f} / 3.0f)")),
        7 => p(format!("String.format(\"%.9f\", {f} / 3.0f)")),
        8 => p(format!("String.format(\"%e|%s\", {f} / 3.0f, {f} / 3.0f)")),
        9 => p(format!("String.format(\"%2$s|%1$.4f\", {f}, {f} / 7.0f)")),
        _ => p(format!("String.format(\"%b %s %d%%\", true, {f}, 3)")),
    }
}

/// `Integer` statics.
fn g_integer(r: &mut Rng) -> String {
    match r.below(3) {
        0 => p(format!(
            "Integer.parseInt(\"{}\")",
            pick(r, &["0", "7", "-42", "100"])
        )),
        1 => p(format!("Integer.toString({})", pick(r, INTS))),
        _ => p(format!("Integer.valueOf({})", pick(r, INTS))),
    }
}

/// A counted loop with an accumulator — exercises the loop lowering and the
/// backpatched `break`/`continue` edges.
fn g_loop(r: &mut Rng) -> String {
    let n = 2 + r.below(5);
    let step = pick(r, &["1", "2", "3"]);
    format!(
        "{{ int acc = 0; for (int i = 0; i < {n}; i++) {{ if (i == 1) {{ continue; }} acc += i * {step}; }} System.out.println(acc); }}"
    )
}

/// `switch` with fall-through and a default.
fn g_switch(r: &mut Rng) -> String {
    let v = r.below(4);
    format!(
        "{{ int s = {v}; switch (s) {{ case 0: System.out.println(\"zero\"); break; case 1: case 2: System.out.println(\"small\"); break; default: System.out.println(\"other\"); }} }}"
    )
}

/// A one-dimensional array: literal, index, mutate, `.length`.
fn g_array(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    format!(
        "{{ int[] arr = {{{a}, {b}, 5}}; arr[1] += 2; System.out.println(arr[0] + \",\" + arr[1] + \",\" + arr.length); }}"
    )
}

/// 32-bit `int` overflow. Every operand is statically `int`-typed (a literal, an
/// `int` local, an `int` parameter, an `int[]` element, an `int` field) — the
/// shape javars models — so a divergence here is a real wrap bug and not the
/// documented "operand type not statically known" gap.
fn g_overflow(r: &mut Rng) -> String {
    let big = pick(
        r,
        &[
            "2147483647",
            "-2147483648",
            "2000000000",
            "1103515245",
            "65536",
            "100000",
            "46341",
        ],
    );
    let k = pick(r, &["2", "3", "31", "-1", "1103515245", "100000"]);
    match r.below(7) {
        0 => p(format!("{big} * {k}")),
        1 => p(format!("{big} + {big}")),
        2 => format!("{{ int ov = {big}; ov *= {k}; System.out.println(ov); }}"),
        3 => format!("{{ int ov = {big}; ov++; System.out.println(ov); }}"),
        4 => format!(
            "{{ int ov = 12345; for (int i = 0; i < 8; i++) {{ ov = ov * {k} + 7; }} System.out.println(ov); }}"
        ),
        5 => format!("{{ int[] ova = {{{big}}}; ova[0] *= {k}; System.out.println(ova[0]); }}"),
        // A `long` operand must NOT wrap — the other half of the rule.
        _ => format!("{{ long ovl = {big}; System.out.println(ovl * {k}); }}"),
    }
}

/// The enhanced `for` over an array — element rebinding, `.length` bound, and
/// the `break`/`continue` edges.
fn g_foreach(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    match r.below(3) {
        0 => format!(
            "{{ int[] fe = {{{a}, {b}, 5}}; int t = 0; for (int v : fe) {{ t += v; }} System.out.println(t); }}"
        ),
        1 => "{ String[] fs = {\"a\", \"bb\", \"ccc\"}; for (String v : fs) { System.out.print(v.length()); } System.out.println(); }"
            .to_string(),
        _ => format!(
            "{{ int[] fe = {{{a}, {b}, 5, 9}}; int t = 0; for (int v : fe) {{ if (v == 5) {{ continue; }} if (v == 9) {{ break; }} t += v; }} System.out.println(t); }}"
        ),
    }
}

/// `throw`/`catch`/`finally` — handler selection by class, propagation out of a
/// call, and the `finally` ordering.
fn g_exception(r: &mut Rng) -> String {
    let msg = pick(r, &["boom", "x", "bad input"]);
    match r.below(4) {
        0 => format!(
            "{{ try {{ throw new IllegalStateException(\"{msg}\"); }} catch (RuntimeException e) {{ System.out.println(e.getMessage()); }} }}"
        ),
        1 => format!(
            "{{ try {{ throw new NumberFormatException(\"{msg}\"); }} catch (IllegalStateException e) {{ System.out.println(\"wrong\"); }} catch (IllegalArgumentException e) {{ System.out.println(e); }} finally {{ System.out.println(\"fin\"); }} }}"
        ),
        2 => format!(
            "{{ int t = 0; for (int i = 0; i < 4; i++) {{ try {{ if (i % 2 == 0) {{ throw new RuntimeException(\"{msg}\"); }} t += i; }} catch (RuntimeException e) {{ t += 10; }} }} System.out.println(t); }}"
        ),
        _ => format!(
            "{{ try {{ try {{ throw new IllegalArgumentException(\"{msg}\"); }} finally {{ System.out.println(\"inner\"); }} }} catch (Exception e) {{ System.out.println(\"outer \" + e.getMessage()); }} }}"
        ),
    }
}

/// Catchable *runtime* faults — the ones javars raises itself rather than a
/// `throw`: an out-of-range array index, `Integer.parseInt` on junk, integral
/// `/ 0` and `% 0`, a negative array size, and the `String` index/argument
/// faults. Each probe both catches the fault and prints its `getMessage()`/
/// `toString()`, so a wrong exception *type* and a wrong detail message are
/// both divergences.
///
/// Divisors and indices come from computed values (`arr.length - 3`), never
/// literals, so the compile-time constant-divisor path cannot mask the check.
/// `NullPointerException` messages are deliberately not printed: Java's helpful
/// NPE names the javac local slot (`because "<local3>" is null`), which javars
/// cannot reproduce (see BUGS.md).
fn g_fault(r: &mut Rng) -> String {
    let catch_as = pick(
        r,
        &[
            "RuntimeException",
            "Exception",
            "Throwable",
            "IndexOutOfBoundsException",
        ],
    );
    match r.below(9) {
        0 => format!(
            "{{ int[] fa = {{1, 2, 3}}; int fi = fa.length + {}; try {{ System.out.println(fa[fi]); }} catch (ArrayIndexOutOfBoundsException e) {{ System.out.println(e.getMessage()); }} }}",
            pick(r, &["0", "2", "-7", "-4"])
        ),
        1 => format!(
            "{{ int[] fa = new int[2]; int fi = {}; try {{ fa[fi] = 1; }} catch (ArrayIndexOutOfBoundsException e) {{ System.out.println(e); }} System.out.println(fa[0]); }}",
            pick(r, &["2", "-1", "9"])
        ),
        2 => format!(
            "{{ try {{ System.out.println(Integer.parseInt(\"{}\")); }} catch (NumberFormatException e) {{ System.out.println(e.getMessage()); }} }}",
            pick(r, &["abc", "", " 7 ", "1x", "99999999999", "-", "+"])
        ),
        3 => format!(
            "{{ int fz = {} - {}; try {{ System.out.println(7 {} fz); }} catch (ArithmeticException e) {{ System.out.println(e); }} }}",
            pick(r, &["1", "3", "5"]),
            pick(r, &["1", "3", "5"]),
            if r.below(2) == 0 { "/" } else { "%" }
        ),
        4 => format!(
            "{{ int fn = {}; try {{ int[] fq = new int[fn]; System.out.println(fq.length); }} catch (NegativeArraySizeException e) {{ System.out.println(e.getMessage()); }} }}",
            pick(r, &["-1", "-9", "2"])
        ),
        5 => format!(
            "{{ String fs = \"abcd\"; int fi = {}; try {{ System.out.println(fs.charAt(fi)); }} catch (StringIndexOutOfBoundsException e) {{ System.out.println(e.getMessage()); }} }}",
            pick(r, &["1", "4", "-1", "9"])
        ),
        6 => format!(
            "{{ String fs = \"abcd\"; int fi = {}; try {{ System.out.println(fs.substring(fi, 3)); }} catch (StringIndexOutOfBoundsException e) {{ System.out.println(e.getMessage()); }} }}",
            pick(r, &["0", "5", "-2"])
        ),
        7 => format!(
            "{{ int fc = {}; try {{ System.out.println(\"ab\".repeat(fc)); }} catch (IllegalArgumentException e) {{ System.out.println(e.getMessage()); }} }}",
            pick(r, &["-1", "-3", "2"])
        ),
        // The fault must propagate out of a call frame and be catchable by any
        // supertype of its class.
        _ => format!(
            "{{ int[] fa = {{4, 5}}; try {{ System.out.println(fa[fa.length + 1] + Integer.parseInt(\"z\")); }} catch ({catch_as} e) {{ System.out.println(\"c:\" + e); }} }}"
        ),
    }
}

/// `finally` interaction with the jumps that leave it — `return`, `break`,
/// `continue`, labeled forms, nesting, and an exception raised inside a `catch`
/// arm (which must still run the cleanup on its way out).
fn g_finally(r: &mut Rng) -> String {
    match r.below(6) {
        0 => format!(
            "{{ int fv = {}; System.out.println(fv); }}",
            pick(
                r,
                &[
                    "fin1()",
                    "fin2()",
                    "fin3()"
                ]
            )
        ),
        1 => "{ int t = 0; for (int i = 0; i < 4; i++) { try { if (i == 2) { break; } t += i; } finally { System.out.println(\"b\" + i); } } System.out.println(t); }"
            .to_string(),
        2 => "{ int t = 0; for (int i = 0; i < 4; i++) { try { if (i % 2 == 0) { continue; } t += i; } finally { System.out.println(\"c\" + i); } } System.out.println(t); }"
            .to_string(),
        3 => "{ int t = 0; outer: for (int i = 0; i < 3; i++) { for (int j = 0; j < 3; j++) { try { if (j == 1) { continue outer; } if (i == 2) { break outer; } t++; } finally { System.out.println(\"l\" + i + j); } } } System.out.println(t); }"
            .to_string(),
        4 => format!(
            "{{ try {{ try {{ throw new IllegalStateException(\"a\"); }} catch (IllegalStateException e) {{ throw new RuntimeException(\"{}\"); }} finally {{ System.out.println(\"fin\"); }} }} catch (RuntimeException e) {{ System.out.println(\"out \" + e.getMessage()); }} }}",
            pick(r, &["b", "c"])
        ),
        _ => "{ try { try { int[] z = new int[1]; System.out.println(z[3]); } catch (NumberFormatException e) { System.out.println(\"no\"); } finally { System.out.println(\"fin2\"); } } catch (RuntimeException e) { System.out.println(\"out2 \" + e.getMessage()); } }"
            .to_string(),
    }
}

/// Try-with-resources: close order (reverse of declaration), closing before the
/// outer `catch`/`finally` runs, the exceptional path, a `return` out of the
/// block, and the Java 9 bare-name form.
fn g_resource(r: &mut Rng) -> String {
    match r.below(5) {
        0 => "{ try (Res a = new Res(\"a\")) { System.out.println(\"body\"); } }".to_string(),
        1 => "{ try (Res a = new Res(\"a\"); Res b = new Res(\"b\")) { System.out.println(\"body\"); } }"
            .to_string(),
        2 => format!(
            "{{ try (Res a = new Res(\"a\"); Res b = new Res(\"b\")) {{ throw new IllegalStateException(\"{}\"); }} catch (IllegalStateException e) {{ System.out.println(\"c \" + e.getMessage()); }} finally {{ System.out.println(\"fin\"); }} }}",
            pick(r, &["x", "y"])
        ),
        3 => "{ try (Res a = new Res(\"a\")) { int[] q = {1}; System.out.println(q[4]); } catch (ArrayIndexOutOfBoundsException e) { System.out.println(e.getMessage()); } }"
            .to_string(),
        _ => "{ Res h = new Res(\"h\"); try (h) { System.out.println(\"body\"); } System.out.println(resret()); }"
            .to_string(),
    }
}

/// `enum` types: constant identity, `name()`/`ordinal()`/`toString()`,
/// `values()` order, `valueOf` (including its `IllegalArgumentException`),
/// `switch` on an unqualified constant, an enum-typed parameter/return, and an
/// enum with a body of its own.
fn g_enum(r: &mut Rng) -> String {
    let c = pick(r, &["RED", "GREEN", "BLUE"]);
    match r.below(10) {
        0 => format!(
            "{{ Color e = Color.{c}; System.out.println(e + \",\" + e.name() + \",\" + e.ordinal()); }}"
        ),
        1 => format!(
            "{{ Color e = Color.{c}; System.out.println((e == Color.{}) + \",\" + e.equals(Color.{})); }}",
            pick(r, &["RED", "BLUE"]),
            pick(r, &["GREEN", "RED"])
        ),
        2 => "{ for (Color e : Color.values()) { System.out.print(e + \":\" + e.ordinal() + \" \"); } System.out.println(Color.values().length); }"
            .to_string(),
        3 => format!(
            "{{ try {{ System.out.println(Color.valueOf(\"{}\")); }} catch (IllegalArgumentException x) {{ System.out.println(x.getMessage()); }} }}",
            pick(r, &["RED", "BLUE", "PINK", "red", ""])
        ),
        4 => format!(
            "{{ Color e = Color.{c}; switch (e) {{ case RED: System.out.println(\"r\"); break; case GREEN: System.out.println(\"g\"); break; default: System.out.println(\"d\"); }} }}"
        ),
        5 => format!("{{ System.out.println(shout(Color.{c}) + \" \" + next(Color.{c})); }}"),
        6 => format!(
            "{{ Op o = Op.{}; System.out.println(o.apply({}, {}) + \" \" + o.isMul() + \" \" + o); }}",
            pick(r, &["ADD", "SUB", "MUL"]),
            pick(r, INTS),
            pick(r, DIVS)
        ),
        // A constant carrying constructor arguments — per-constant state.
        7 => format!(
            "{{ Planet pl = Planet.{}; System.out.println(pl + \",\" + pl.mass() + \",\" + pl.heavy() + \",\" + pl.ordinal()); }}",
            pick(r, &["MERCURY", "EARTH", "JUPITER"])
        ),
        8 => "{ for (Planet pl : Planet.values()) { System.out.print(pl.mass() + \" \"); } System.out.println(); }"
            .to_string(),
        // A constant with a *body* — an anonymous subclass whose override the
        // enum's own abstract method dispatches to.
        _ => format!(
            "{{ Ops o = Ops.{}; System.out.println(o.apply({}, {}) + \",\" + o.label() + \",\" + o); }}",
            pick(r, &["PLUS", "MINUS", "TIMES"]),
            pick(r, INTS),
            pick(r, DIVS)
        ),
    }
}

/// `static` fields: class-level storage that outlives every instance, reached
/// both qualified (`St.n`) and through an inheriting class (`Sub2.n`), plus the
/// `static { … }` block and static-field initializers that seed it before
/// `main` runs. Every probe assigns before it reads, so a probe's output does
/// not depend on which other probes ran first.
fn g_static(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    match r.below(6) {
        0 => format!("{{ St.n = {a}; System.out.println(St.n + \",\" + St.get()); }}"),
        1 => format!("{{ St.n = {a}; St.bump(); St.n += 3; System.out.println(St.n); }}"),
        2 => "{ System.out.println(St.LABEL + St.SIZE + \",\" + St.INIT); }".to_string(),
        3 => format!("{{ St.n = {a}; System.out.println(St.n / 2 + \",\" + St.n % 3); }}"),
        4 => format!(
            "{{ St.arr[1] = {a}; System.out.println(St.arr[0] + \",\" + St.arr[1] + \",\" + St.arr.length); }}"
        ),
        _ => format!("{{ St.n = {a}; St.n++; System.out.println(St.n + \",\" + Sub2.n); }}"),
    }
}

/// `record` types: the derived accessors, `toString`, component-wise `equals`,
/// a compact constructor's validation, and a user-declared extra method.
fn g_record(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    match r.below(6) {
        0 => format!("{{ System.out.println(new Pt({a}, {b})); }}"),
        1 => format!(
            "{{ Pt p = new Pt({a}, {b}); System.out.println(p.x() + \",\" + p.y() + \",\" + p.sum()); }}"
        ),
        2 => format!(
            "{{ System.out.println(new Pt({a}, {b}).equals(new Pt({a}, {b})) + \",\" + new Pt({a}, {b}).equals(new Pt({b}, {a}))); }}"
        ),
        3 => format!(
            "{{ Tag t = new Tag({}, {}); System.out.println(t + \" \" + t.tag().length()); }}",
            pick(r, STRS),
            pick(r, DBLS)
        ),
        4 => format!(
            "{{ Pt[] ps = {{ new Pt({a}, {b}), new Pt(0, 0) }}; for (Pt q : ps) {{ System.out.print(q + \";\"); }} System.out.println(); }}"
        ),
        _ => format!(
            "{{ try {{ System.out.println(new Ord({a}, {b})); }} catch (IllegalArgumentException e) {{ System.out.println(\"bad \" + e.getMessage()); }} }}"
        ),
    }
}

/// Lambdas and functional interfaces: an expression and a block body, a
/// captured effectively-final local, a lambda passed as an argument, one
/// returned from a method (so it outlives the frame that built it), the target
/// interface's declared `int` parameters driving `/` truncation and the 32-bit
/// wrap inside the body, and the three method-reference shapes. A lambda's own
/// `toString` is a JVM identity hash, so no probe ever prints one.
fn g_lambda(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let d = pick(r, DIVS);
    let s = pick(r, STRS);
    let op = pick(r, AOPS);
    match r.below(11) {
        0 => format!("{{ Calc c = (x, y) -> x {op} y; System.out.println(c.of({a}, {b})); }}"),
        1 => format!("{{ Calc c = (x, y) -> {{ return x {op} y; }}; System.out.println(c.of({a}, {b})); }}"),
        2 => format!("{{ Calc c = (x, y) -> x / y; System.out.println(c.of({a}, {d})); }}"),
        3 => format!("{{ System.out.println(useCalc((x, y) -> x {op} y, {a}, {b})); }}"),
        4 => format!(
            "{{ int cap = {a}; Calc c = (x, y) -> x + y + cap; System.out.println(c.of({b}, {d})); }}"
        ),
        5 => format!("{{ System.out.println(mkAdder({a}).of({b}, {d})); }}"),
        6 => format!(
            "{{ Str1 f = t -> t.toUpperCase() + t.length(); System.out.println(f.of({s})); }}"
        ),
        7 => format!(
            "{{ Pred1 pr = x -> x > {a}; System.out.println(pr.of({b}) + \",\" + pr.of({d})); }}"
        ),
        8 => format!("{{ Sup0 g = () -> {a} * {b}; System.out.println(g.of()); }}"),
        9 => format!("{{ Str1 f = String::toUpperCase; System.out.println(f.of({s})); }}"),
        _ => format!(
            "{{ Calc c = (x, y) -> {{ if (y == 0) {{ return -1; }} return x % y; }}; System.out.println(c.of({a}, {d})); }}"
        ),
    }
}

/// `java.util` collections: `List`/`ArrayList`, the three `Map`s and three
/// `Set`s, `Arrays.asList`, `Collections.sort`/`reverse`, the enhanced `for`
/// over a collection, and the views (`keySet`, `values`). The point of the mode
/// is **iteration order**: a `HashMap`/`HashSet` prints in Java's bucket order,
/// which is a pure function of the keys and the size, so it is a parity signal
/// rather than nondeterminism. Elements stay `String`/`int`/`double`/`boolean`,
/// because a user object inside a collection renders through its `toString()`
/// in Java and through the default form in javars (a documented gap).
fn g_collection(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let c = pick(r, INTS);
    let s1 = pick(r, STRS);
    let s2 = pick(r, STRS);
    match r.below(14) {
        0 => format!(
            "{{ List<Integer> l = new ArrayList<>(); l.add({a}); l.add({b}); l.add({c}); System.out.println(l + \" \" + l.size() + \" \" + l.get(1)); }}"
        ),
        1 => format!(
            "{{ List<String> l = new ArrayList<>(); l.add({s1}); l.add({s2}); System.out.println(l.contains({s1}) + \",\" + l.indexOf({s2}) + \",\" + l.isEmpty()); }}"
        ),
        2 => format!(
            "{{ List<Integer> l = new ArrayList<>(Arrays.asList({a}, {b}, {c})); Collections.sort(l); System.out.println(l); }}"
        ),
        3 => format!(
            "{{ List<Integer> l = new ArrayList<>(Arrays.asList({a}, {b}, {c})); Collections.reverse(l); System.out.println(l); }}"
        ),
        4 => format!(
            "{{ List<Integer> l = new ArrayList<>(Arrays.asList({a}, {b}, {c})); l.sort((p, q) -> p - q); System.out.println(l); }}"
        ),
        5 => format!(
            "{{ Map<String, Integer> m = new HashMap<>(); m.put({s1}, {a}); m.put({s2}, {b}); m.put(\"zz\", {c}); System.out.println(m); }}"
        ),
        6 => format!(
            "{{ Map<String, Integer> m = new LinkedHashMap<>(); m.put({s1}, {a}); m.put({s2}, {b}); System.out.println(m + \" \" + m.keySet() + \" \" + m.values()); }}"
        ),
        7 => format!(
            "{{ Map<String, Integer> m = new TreeMap<>(); m.put({s1}, {a}); m.put({s2}, {b}); m.put(\"m\", {c}); System.out.println(m); }}"
        ),
        8 => format!(
            "{{ Set<Integer> t = new HashSet<>(); t.add({a}); t.add({b}); t.add({c}); System.out.println(t + \" \" + t.size() + \" \" + t.contains({a})); }}"
        ),
        9 => format!(
            "{{ Set<String> t = new LinkedHashSet<>(Arrays.asList({s1}, {s2}, {s1})); System.out.println(t); }}"
        ),
        10 => format!(
            "{{ Set<Integer> t = new TreeSet<>(Arrays.asList({a}, {b}, {c})); System.out.println(t); }}"
        ),
        11 => format!(
            "{{ int tot = 0; for (int v : Arrays.asList({a}, {b}, {c})) {{ tot += v; }} System.out.println(tot); }}"
        ),
        12 => format!(
            "{{ Map<String, Integer> m = new HashMap<>(); for (int i = 0; i < 15; i++) {{ m.put(\"k\" + i, i * {a}); }} System.out.println(m); }}"
        ),
        _ => format!(
            "{{ Map<String, Integer> m = new LinkedHashMap<>(); m.put({s1}, {a}); System.out.println(m.get({s1}) + \",\" + m.getOrDefault(\"absent\", -1) + \",\" + m.containsKey({s2})); }}"
        ),
    }
}

/// Arrow `switch` expressions and statements: multi-label arms, a `default`
/// (including one written before a matching arm, which must not shadow it),
/// block arms with `yield`, an `enum` discriminant with unqualified labels, a
/// `String` discriminant, a `throw` arm, and the result feeding `/` typing —
/// which only truncates if the expression's static type survives the switch.
fn g_switchexpr(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let s = pick(r, STRS);
    match r.below(10) {
        0 => format!(
            "{{ int v = {a}; System.out.println(switch (v) {{ case 0 -> \"zero\"; case 1, 2 -> \"low\"; default -> \"other\"; }}); }}"
        ),
        1 => format!(
            "{{ int v = {a}; System.out.println(switch (v) {{ case 0 -> 100; default -> {{ int t = v * 2; yield t + 1; }} }}); }}"
        ),
        2 => format!(
            "{{ int v = {a}; switch (v) {{ case 0 -> System.out.println(\"z\"); case 1 -> System.out.println(\"o\"); default -> System.out.println(\"d\"); }} }}"
        ),
        3 => format!(
            "{{ Color c = {}; System.out.println(switch (c) {{ case RED -> \"r\"; case GREEN -> \"g\"; default -> \"b\"; }}); }}",
            pick(r, &["Color.RED", "Color.GREEN", "Color.BLUE", "next(Color.BLUE)"])
        ),
        4 => format!(
            "{{ String t = {s}; System.out.println(switch (t) {{ case \"\" -> \"empty\"; case \"a\", \"abc\" -> \"hit\"; default -> \"miss\"; }}); }}"
        ),
        5 => format!(
            "{{ int v = {a}; System.out.println(switch (v) {{ default -> \"d\"; case 0 -> \"zero\"; }}); }}"
        ),
        6 => format!(
            "{{ int v = {a}; System.out.println(switch (v) {{ case 0 -> 8; default -> 7; }} / 2); }}"
        ),
        7 => format!(
            "{{ int v = {a}; System.out.println({b} + switch (v) {{ case 0 -> 20; default -> 3; }} + 300); }}"
        ),
        8 => format!(
            "{{ int v = {a}; try {{ System.out.println(switch (v) {{ case 0 -> throw new IllegalStateException(\"z\"); default -> \"ok\"; }}); }} catch (IllegalStateException e) {{ System.out.println(\"c \" + e.getMessage()); }} }}"
        ),
        _ => format!(
            "{{ Op o = {}; System.out.println(switch (o) {{ case ADD -> o.apply({a}, {b}); case SUB -> o.apply({b}, {a}); default -> 0; }}); }}",
            pick(r, &["Op.ADD", "Op.SUB", "Op.MUL"])
        ),
    }
}

/// `var` inference and `long` literals. A `var` local has to carry the *type*
/// of its initializer, not just its value, or `/` stops truncating and `int`
/// arithmetic stops wrapping — so every probe reads the binding back through an
/// operation that only a correct static type gets right. The `long` half is the
/// other side of the same coin: an `L`-suffixed literal must NOT wrap at 32
/// bits, which is only visible past `Integer.MAX_VALUE`.
fn g_varinfer(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let d = pick(r, DIVS);
    let f = pick(r, DBLS);
    let s = pick(r, STRS);
    let big = pick(
        r,
        &["3000000000L", "2147483648L", "1000000000L", "5000000000L"],
    );
    match r.below(11) {
        0 => format!("{{ var v = {a}; System.out.println(v / {d} + \",\" + v % {d}); }}"),
        1 => format!("{{ var v = {f}; System.out.println(v / {d}); }}"),
        2 => format!("{{ var v = {s}; System.out.println(v.length() + v.toUpperCase()); }}"),
        3 => format!(
            "{{ var arr = new int[]{{{a}, {d}, 8}}; for (var e : arr) {{ System.out.print(e / 2 + \",\"); }} System.out.println(); }}"
        ),
        4 => format!(
            "{{ var g = new int[][]{{{{{a}, 2}}, {{3, {d}}}}}; for (var row : g) {{ for (var c : row) {{ System.out.print(c / 2); }} }} System.out.println(); }}"
        ),
        5 => format!(
            "{{ var t = 0; for (var i = 0; i < 5; i++) {{ t += i * {a}; }} System.out.println(t / 2); }}"
        ),
        6 => format!("{{ var v = {big}; System.out.println(v + v); }}"),
        7 => format!("{{ System.out.println({big} + {big}); }}"),
        8 => format!("{{ var v = {big}; System.out.println(v * 3 + \",\" + v / 7); }}"),
        9 => format!(
            "{{ var ps = new String[]{{{s}, \"zz\"}}; for (var p : ps) {{ System.out.print(p.length()); }} System.out.println(); }}"
        ),
        _ => format!(
            "{{ var p = new Pt({a}, {d}); System.out.println(p + \",\" + p.x() / 2 + \",\" + p.sum()); }}"
        ),
    }
}

/// `&`, `|`, `^`, `~` — bitwise on integral operands, non-short-circuiting
/// logical on booleans (where the result must print `true`/`false`, not 0/1).
fn g_bitwise(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let op = pick(r, &["&", "|", "^"]);
    match r.below(5) {
        0 => p(format!("{a} {op} {b}")),
        1 => p(format!("~{a}")),
        2 => p(format!("{} {op} {}", pick(r, BOOLS), pick(r, BOOLS))),
        3 => p(format!("({a} {op} {b}) + 1")),
        _ => format!("{{ long v = {a}L; v {op}= {b}; System.out.println(v); }}"),
    }
}

/// `<<`, `>>`, `>>>` — the distance is masked to the *left* operand's width
/// (5 bits for `int`, 6 for `long`), and only `int` narrows afterwards.
fn g_shift(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    // Distances deliberately past the width, so the mask is exercised.
    let n = pick(r, &["0", "1", "3", "16", "31", "32", "33", "63", "64"]);
    let op = pick(r, &["<<", ">>", ">>>"]);
    match r.below(4) {
        0 => p(format!("{a} {op} {n}")),
        1 => p(format!("{a}L {op} {n}")),
        2 => format!("{{ int v = {a}; v {op}= {n}; System.out.println(v); }}"),
        _ => format!("{{ long v = {a}L; v {op}= {n}; System.out.println(v); }}"),
    }
}

/// Narrowing and widening primitive casts — the saturating float→integral rule
/// and the two's-complement integral narrowings.
fn g_cast(r: &mut Rng) -> String {
    let d = pick(r, DBLS);
    let i = pick(r, INTS);
    // The boxed values a *reference* cast is applied to, and the target types
    // it is applied with. Every failing pair here is between `java.lang` types,
    // whose `ClassCastException` message javars reproduces exactly; the
    // user-class pairs print the exception's class instead, because Java's
    // message names its class loader by identity hash.
    // A boxed `char` is deliberately absent: javars models a `Character` as the
    // one-character String it prints as, so a cast that fails on one names
    // `java.lang.String` in its message. That is a documented simplification,
    // not a finding.
    const BOXED: &[&str] = &["\"hi\"", "42", "3.5", "true"];
    const TARGETS: &[&str] = &[
        "String",
        "Integer",
        "Double",
        "Boolean",
        "Number",
        "CharSequence",
        "Object",
    ];
    match r.below(12) {
        0 => p(format!("(int) {d}")),
        1 => p(format!("(long) {d}")),
        // Parenthesised because `DBLS` holds negative literals: `-{d}` spliced
        // onto `-1.5` is `--1.5`, which Java lexes as the decrement operator and
        // rejects with "unexpected type / required: variable".
        2 => p(format!("(int) -({d})")),
        3 => p(format!("(byte) ({i} * 37)")),
        4 => p(format!("(short) ({i} * 5000)")),
        5 => p(format!("(double) {i} / 2")),
        6 => p(format!("(char) (65 + {})", r.below(20))),
        7 => p(format!("(int) 1e18 + {i}")),
        // A reference cast between JDK types: succeeds, or throws with the
        // message Java prints.
        8 | 9 => {
            let val = pick(r, BOXED);
            let target = pick(r, TARGETS);
            format!(
                "{{ Object o = {val}; try {{ System.out.println(({target}) o); }} catch (ClassCastException e) {{ System.out.println(e.getMessage()); }} }}"
            )
        }
        // A downcast in a user hierarchy: the exception's *class*, since its
        // message names the launcher's class loader.
        10 => {
            let src = pick(r, &["new Left()", "new Right()", "new Base()"]);
            let target = pick(r, &["Left", "Right", "Base", "Marker", "Object"]);
            format!(
                "{{ Base b = {src}; try {{ System.out.println(({target}) b); }} catch (ClassCastException e) {{ System.out.println(\"CCE \" + e.getClass().getSimpleName()); }} }}"
            )
        }
        // A `null` casts to anything, and a cast does not erase what the
        // operand is — `println((Object) left)` still finds `Left.toString`.
        _ => {
            let src = pick(r, &["new Left()", "new Right()"]);
            format!(
                "{{ Base n = null; System.out.println((Base) n); System.out.println((Object) {src}); }}"
            )
        }
    }
}

/// `i++` / `++i` / `i--` / `--i` in *value* position, where the pre- and
/// post-forms differ in what the expression evaluates to.
fn g_incdec(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    match r.below(5) {
        0 => format!("{{ int v = {a}; System.out.println(v++ + \",\" + v); }}"),
        1 => format!("{{ int v = {a}; System.out.println(++v + \",\" + v); }}"),
        2 => format!("{{ int v = {a}; System.out.println(v-- + \",\" + v); }}"),
        3 => format!("{{ int v = {a}; System.out.println(--v + \",\" + v); }}"),
        _ => "{ int t = 0; for (int x = 0, y = 5; x < y; x++, y--) { t += x + y; } System.out.println(t); }"
            .to_string(),
    }
}

/// Hex, binary, octal, and `_`-separated integer literals — including the
/// bit-pattern reading that makes `0xFFFFFFFF` the `int` -1.
fn g_literal(r: &mut Rng) -> String {
    p(pick(
        r,
        &[
            "0x1F",
            "0xFF",
            "0xFFFFFFFF",
            "0xFFFFFFFFFFFFFFFFL",
            "0X7fffffff",
            "0b1010",
            "0b1111_0000",
            "0B1",
            "017",
            "0777",
            "1_000_000",
            "1_2_3",
            "0x7FL",
        ],
    )
    .to_string())
}

/// `System.out.printf` / `String.format` beyond the plain conversions: the
/// grouping and parenthesis flags, argument indexes, and `%e`/`%g`/`%h`.
fn g_printf(r: &mut Rng) -> String {
    let i = pick(r, INTS);
    let d = pick(r, DBLS);
    match r.below(9) {
        0 => format!("System.out.printf(\"%d|%s%n\", {i}, {});", pick(r, STRS)),
        1 => p(format!("String.format(\"%,d\", {i} * 100000)")),
        2 => p(format!("String.format(\"%(d\", {i})")),
        3 => p("String.format(\"%2$s-%1$s\", \"a\", \"b\")".to_string()),
        4 => p(format!("String.format(\"%e\", {d})")),
        5 => p(format!("String.format(\"%E\", {d})")),
        6 => p(format!("String.format(\"%.3f\", {d})")),
        7 => p(format!("String.format(\"%08.2f\", {d})")),
        _ => p(format!("String.format(\"%,.2f\", {d} * 1000)")),
    }
}

/// Labeled `break`/`continue` out of nested loops, including from inside a
/// `try` whose `finally` must still run on the way out.
fn g_labelflow(r: &mut Rng) -> String {
    match r.below(4) {
        0 => "{ outer: for (int i = 0; i < 3; i++) { for (int j = 0; j < 3; j++) { if (j == 1) continue outer; System.out.print(i + \":\" + j + \" \"); } } System.out.println(); }".to_string(),
        1 => "{ up: for (int i = 0; i < 4; i++) { for (int j = 0; j < 4; j++) { if (i * j > 2) break up; System.out.print(i * j); } } System.out.println(); }".to_string(),
        2 => "{ lp: for (int i = 0; i < 3; i++) { for (int j = 0; j < 3; j++) { try { if (j == 1) continue lp; System.out.print(\"b\" + i + j); } finally { System.out.print(\"f\" + i + j); } } } System.out.println(); }".to_string(),
        _ => "{ int k = 0; ex: while (true) { k++; try { if (k > 2) break ex; } finally { System.out.print(\"w\" + k); } } System.out.println(k); }".to_string(),
    }
}

/// `char` — the boundary between Java's *integral* `char` and its string
/// conversion. Arithmetic on a `char` promotes to `int` (`'a' + 1` is 98), while
/// a `char` reaching a String, a `println`, or a collection element renders as
/// the character. Both sides have to be right at once, which is what makes this
/// worth generating: an implementation that models a `char` as a one-character
/// string passes every rendering probe and fails every arithmetic one.
fn g_char(r: &mut Rng) -> String {
    let c = pick(r, CHARS);
    let d = pick(r, CHARS);
    // A third char guaranteed distinct from `c`: `switch` rejects duplicate
    // case labels at compile time, so the two arms must not collide.
    let e = CHARS[(CHARS.iter().position(|x| x == c).unwrap_or(0) + 1) % CHARS.len()];
    let s = pick(r, STRS);
    let n = pick(r, &["0", "1", "2", "7", "32", "-1"]);
    match r.below(20) {
        // Arithmetic and promotion.
        0 => p(format!("'{c}' + {n}")),
        1 => p(format!("'{c}' - '{d}'")),
        2 => p(format!("'{c}' * 2 + '{d}'")),
        3 => p(format!("(char) ('{c}' + {n})")),
        4 => p(format!("'{c}' & 0x0F | '{d}' ^ 3")),
        // String conversion.
        5 => p(format!("\"[\" + '{c}' + \"]\" + {n}")),
        6 => p(format!("'{c}' + \"\" + '{d}'")),
        7 => p(format!("String.valueOf('{c}') + String.format(\"%c%s\", '{d}', '{c}')")),
        // Casts both ways.
        8 => p(format!("(int) '{c}' + (int) '{d}'")),
        9 => p(format!(
            "(char) ({} + {n})",
            c.chars().next().unwrap_or('a') as u32
        )),
        10 => p(format!("(long) '{c}' + (double) '{d}'")),
        // `charAt` and the code-point idioms built on it.
        11 => format!(
            "{{ String s = {s}; int t = 0; for (int i = 0; i < s.length(); i++) t += s.charAt(i) - 'a'; System.out.println(t); }}"
        ),
        12 => format!("{{ String s = {s}; System.out.println(s.isEmpty() ? '?' : s.charAt(0)); }}"),
        13 => format!(
            "{{ String s = {s}; String o = \"\"; for (char x : s.toCharArray()) o += (char) (x + {n}); System.out.println(o); }}"
        ),
        14 => format!("System.out.println(Arrays.toString({s}.toCharArray()));"),
        // Mutation: `++`, compound assign, and the 16-bit `char` width.
        15 => format!(
            "{{ char v = '{c}'; v++; v += {n}; System.out.println(v + \"|\" + (int) v); }}"
        ),
        // Comparison and `switch`.
        16 => p(format!("('{c}' < '{d}') + \",\" + ('{c}' == '{d}')")),
        17 => format!(
            "{{ char v = '{d}'; switch (v) {{ case '{e}': System.out.println(\"e\"); break; case '{c}': System.out.println(\"c\"); break; default: System.out.println(\"?\"); }} }}"
        ),
        // `Character` statics, whose results are `char` again.
        18 => p(format!(
            "Character.toUpperCase('{c}') + \"/\" + Character.isDigit('{c}') + \"/\" + Character.toLowerCase('{d}')"
        )),
        // A boxed `Character` in a collection.
        _ => format!(
            "{{ List<Character> l = new ArrayList<>(); l.add('{c}'); l.add('{d}'); System.out.println(l + \"|\" + l.get(0) + \"|\" + l.contains('{c}')); }}"
        ),
    }
}

/// `float` operands, chosen so most are *not* exactly representable in 32 bits
/// — that is where a `double` standing in for a `float` gives itself away.
const FLTS: &[&str] = &[
    "0.1f",
    "0.2f",
    "1.0f",
    "3.0f",
    "2.5f",
    "-1.5f",
    "1.1f",
    "100.0f",
    "1e3f",
    "0.0f",
    "7.0f",
    "1e-4f",
    "1e8f",
    "16777217.0f",
];

/// `float` — a 32-bit type javars models as a `double` kept at 32-bit
/// precision. Both halves have to hold: every operation rounds to `f32` (Java
/// rounds per operation, so `a * b + c` rounds twice), and the value prints with
/// `Float.toString`'s shortest-round-trip *at 32 bits* (`1.0f / 3.0f` is
/// `0.33333334`, where the `double` answer is `0.3333333333333333`).
fn g_float(r: &mut Rng) -> String {
    let a = pick(r, FLTS);
    let b = pick(r, FLTS);
    let c = pick(r, FLTS);
    let d = pick(r, DBLS);
    let i = pick(r, INTS);
    match r.below(17) {
        // The `float` subnormal floor, where Java's two-digit widening decides
        // the last digit (`Float.MIN_VALUE` is `1.4E-45`, not `1.0E-45`).
        14 => p(format!("Float.MIN_VALUE * {}", r.below(64) + 1)),
        15 => p("Float.MIN_VALUE".to_string()),
        16 => p("Float.MIN_NORMAL".to_string()),
        0 => p(format!("{a} / {b}")),
        1 => p(format!("{a} + {b}")),
        2 => p(format!("{a} * {b}")),
        3 => p(format!("{a} - {b}")),
        4 => p(format!("{a} % {b}")),
        // Rounding happens per operation, so the grouping changes the answer.
        5 => p(format!("{a} * {b} + {c}")),
        6 => p(format!("{a} * ({b} + {c})")),
        // A `double` operand promotes the whole operation; an `int` does not.
        7 => p(format!("{a} / {d}")),
        8 => p(format!("{a} / {i}")),
        9 => p(format!("(float) {d} + {a}")),
        10 => p(format!("(double) {a}")),
        // Accumulation, where 32-bit rounding compounds.
        11 => format!(
            "{{ float v = {a}; for (int k = 0; k < 8; k++) v += {b}; System.out.println(v); }}"
        ),
        12 => format!("{{ float v = {a}; v *= {b}; v /= {c}; System.out.println(v + \"|\" + (v < {a})); }}"),
        // Rendering paths: concatenation, `String.valueOf`, an array.
        _ => format!(
            "{{ float[] fa = {{{a}, {b}}}; System.out.println(java.util.Arrays.toString(fa) + \"|\" + String.valueOf(fa[0] + fa[1]) + \"|\" + ({a} / {c})); }}"
        ),
    }
}

/// The `java.util.regex` patterns the `regex` mode draws from. Every one is a
/// construct javars translates rather than refuses, and the pool deliberately
/// mixes the places where Java's *defaults* differ from the engine's — ASCII
/// `\d`/`\w`/`\s`/`\b`, ASCII `(?i)` folding, `.` against a `\r`, and a `$` that
/// sits before a final line terminator.
const PATTERNS: &[&str] = &[
    ",",
    "\\\\.",
    "\\\\d",
    "\\\\d+",
    "\\\\D",
    "\\\\w",
    "\\\\W+",
    "\\\\s",
    "\\\\s+",
    "\\\\S",
    "[abc]",
    "[^abc]",
    "[a-z]",
    "[0-9]+",
    "[-.]",
    "[\\\\w.]",
    "a|b",
    "ab?",
    "a*",
    "a+?",
    "(a)(b)",
    "(?:ab)+",
    "(?<g>a)b",
    "a.c",
    ".",
    "^a",
    "a$",
    "$",
    "\\\\ba",
    "a\\\\b",
    "\\\\B",
    "(?i)a",
    "(?i)[a-c]",
    "(?i:AB)",
    "(?s)a.b",
    "\\\\Qa.c\\\\E",
    "\\\\p{Upper}",
    "\\\\p{Digit}+",
    "\\\\h",
    "\\\\R",
    "a{2}",
    "a{1,2}",
    "(a)\\\\1",
    "a(?=b)",
    "(?<=a)b",
    "x(?!y)",
    "",
    "\\\\u0061",
    // Group-bearing patterns. The pool above is almost entirely group-free,
    // which left the `$n` replacements with nothing legal to pair with; these
    // give the numbered and named back-references a subject, and cover nesting
    // (where the group *number* is the order of the opening paren), an optional
    // group (which can go unmatched, so `$2` expands to nothing rather than
    // throwing), and a named group that a `${name}` replacement can reach.
    "(a)",
    "([a-z])",
    "(\\\\d)",
    "(a|b)",
    "(a)(b)?",
    "(a(b))",
    "(\\\\w)(\\\\w)",
    "(\\\\w)(\\\\w)(\\\\w)",
    "(?<g>a)",
    "(?<g>[a-z])(\\\\d)",
];

/// The subjects the `regex` mode matches against — ASCII, non-ASCII (where the
/// ASCII-only defaults show), embedded line terminators (where `$` and `.` do),
/// and the empty string.
const SUBJECTS: &[&str] = &[
    "\"\"",
    "\"a\"",
    "\"abc\"",
    "\"aabbcc\"",
    "\"a,b,,c\"",
    "\"a1b22c\"",
    "\"a b\\tc\"",
    "\"Hello World\"",
    "\"AbC\"",
    "\"a.b.c\"",
    "\"2024-01-31\"",
    "\"naïve café\"",
    "\"abc\\n\"",
    "\"a\\nb\"",
    "\"a\\rb\"",
    "\"x-y_z\"",
    "\"aaa\"",
];

/// The replacement strings, covering Java's `$n` / `${name}` grammar and its
/// backslash escaping (which is not the `regex` crate's).
///
/// Entries carry their own enclosing quotes, because a replacement is passed to
/// `replaceAll` as an expression rather than interpolated into one.
///
/// A replacement here may reference groups; it is [`pairable`] that keeps it
/// from being handed a pattern that does not have them.
const REPLACEMENTS: &[&str] = &[
    "\"-\"",
    "\"\"",
    "\"[$0]\"",
    "\"<$1>\"",
    "\"$1$1\"",
    "\"\\\\$\"",
    "\"x\"",
    "\"\\\\\\\\\"",
    "\"$2$1\"",
    "\"$0|$1\"",
    "\"${g}\"",
    "\"[${g}]$0\"",
];

/// One Java string *literal body* with the compiler's escapes resolved — the
/// value the running program actually receives. Both pools are written as
/// literal bodies, so a regex `\d` appears here as `\\d` and has to be collapsed
/// before the regex or replacement grammar can be read off it.
///
/// A `backslash-u` escape resolves too, because javac processes those in the
/// lexer rather than passing them through: the pool's hex escape for U+0061
/// reaches `Pattern.compile` as the single character `a`, not as an escape the
/// regex engine ever sees.
fn java_unescape(body: &str) -> String {
    let cs: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < cs.len() {
        if cs[i] != '\\' {
            out.push(cs[i]);
            i += 1;
            continue;
        }
        match cs.get(i + 1) {
            Some('u') => {
                let hex: String = cs.iter().skip(i + 2).take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(c) => {
                        out.push(c);
                        i += 6;
                    }
                    None => {
                        out.push('\\');
                        i += 1;
                    }
                }
            }
            Some('n') => {
                out.push('\n');
                i += 2;
            }
            Some('r') => {
                out.push('\r');
                i += 2;
            }
            Some('t') => {
                out.push('\t');
                i += 2;
            }
            Some(&c) => {
                out.push(c);
                i += 2;
            }
            None => {
                out.push('\\');
                i += 1;
            }
        }
    }
    out
}

/// The number of capturing groups a pattern has, and the names of its named
/// groups — the two facts that decide which replacements are legal against it.
///
/// A `(` opens a capturing group unless it is followed by `?`, with `(?<name>`
/// the exception: that one IS capturing, while the lookbehinds `(?<=` and `(?<!`
/// are not — the distinguishing character is whether an identifier follows the
/// `<`. A `(` inside a character class, or escaped, is literal.
fn pattern_groups(pat: &str) -> (usize, Vec<String>) {
    let cs: Vec<char> = java_unescape(pat).chars().collect();
    let (mut count, mut names) = (0usize, Vec::new());
    let (mut i, mut in_class) = (0usize, false);
    while i < cs.len() {
        match cs[i] {
            '\\' => {
                i += 2;
                continue;
            }
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' if !in_class => {
                if cs.get(i + 1) != Some(&'?') {
                    count += 1;
                } else if cs.get(i + 2) == Some(&'<')
                    && cs.get(i + 3).is_some_and(|c| c.is_ascii_alphabetic())
                {
                    count += 1;
                    names.push(cs[i + 3..].iter().take_while(|c| **c != '>').collect());
                }
            }
            _ => {}
        }
        i += 1;
    }
    (count, names)
}

/// The highest numbered back-reference, and every `${name}` reference, a
/// replacement uses. A backslash-escaped `$` is a literal dollar and references
/// nothing. The entry carries enclosing quotes (see [`REPLACEMENTS`]), which are
/// stripped before the grammar is read.
///
/// Java resolves `$nn` greedily but only as far as a group that exists, so
/// `$12` against a one-group pattern is group 1 followed by a literal `2`.
/// Taking every digit instead is deliberately *conservative*: it can only refuse
/// a pairing Java would have allowed, never allow one Java would abort on. For
/// this pool, whose references are all single-digit, the two agree exactly.
fn replacement_refs(rep: &str) -> (usize, Vec<String>) {
    let body = rep
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(rep);
    let cs: Vec<char> = java_unescape(body).chars().collect();
    let (mut max_num, mut names) = (0usize, Vec::new());
    let mut i = 0;
    while i < cs.len() {
        match cs[i] {
            '\\' => {
                i += 2;
                continue;
            }
            '$' if cs.get(i + 1) == Some(&'{') => {
                let name: String = cs[i + 2..].iter().take_while(|c| **c != '}').collect();
                i += name.len() + 2;
                names.push(name);
            }
            '$' => {
                let digits: String = cs[i + 1..]
                    .iter()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(n) = digits.parse::<usize>() {
                    max_num = max_num.max(n);
                }
                i += digits.len();
            }
            _ => {}
        }
        i += 1;
    }
    (max_num, names)
}

/// Whether `rep` references only groups `pat` actually has — the invariant that
/// keeps the generator from emitting a program the reference JDK aborts on.
fn pairable(pat: &str, rep: &str) -> bool {
    let (count, names) = pattern_groups(pat);
    let (max_num, refs) = replacement_refs(rep);
    max_num <= count && refs.iter().all(|n| names.contains(n))
}

/// A replacement drawn uniformly from those legal against `pat`.
///
/// This bounds the pairing without narrowing the replacement grammar under
/// test: every entry in [`REPLACEMENTS`] stays reachable, because the
/// group-bearing patterns admit all of them. Constraining the *pairing* rather
/// than shrinking the pool is what keeps the fix from trading skips for lost
/// coverage.
fn pick_replacement(r: &mut Rng, pat: &str) -> &'static str {
    let legal: Vec<&'static str> = REPLACEMENTS
        .iter()
        .copied()
        .filter(|rep| pairable(pat, rep))
        .collect();
    assert!(
        !legal.is_empty(),
        "no replacement is legal against pattern `{pat}`: REPLACEMENTS must keep \
         at least one entry that references no group"
    );
    legal[r.below(legal.len())]
}

/// `java.util.regex` through `String.split`/`replaceAll`/`replaceFirst`/
/// `matches` — the pattern *translation* as much as the engine, since Java's
/// defaults and fancy-regex's disagree on `\d`, `\b`, `(?i)`, `.`, and `$`.
fn g_regex(r: &mut Rng) -> String {
    let pat = *pick(r, PATTERNS);
    let s = pick(r, SUBJECTS);
    // The replacement is drawn from the subset legal for *this* pattern, not
    // from the pool at large. Pairing them independently emitted programs like
    // `"aabbcc".replaceAll("[a-z]", "$1$1")`, which the reference JDK aborts
    // with `IndexOutOfBoundsException: No group 1` — so the whole packed program
    // produced no comparable output and was counted a skip. A skip is
    // indistinguishable from coverage in the summary, so the corpus reported
    // agreement it had never actually tested.
    let rep = pick_replacement(r, pat);
    match r.below(7) {
        0 => format!("System.out.println(Arrays.toString({s}.split(\"{pat}\")) + \"/\" + {s}.split(\"{pat}\").length);"),
        1 => format!(
            "System.out.println(Arrays.toString({s}.split(\"{pat}\", {})));",
            r.below(4) as i64 - 1
        ),
        2 => p(format!("{s}.replaceAll(\"{pat}\", {rep})")),
        3 => p(format!("{s}.replaceFirst(\"{pat}\", {rep})")),
        4 => p(format!("{s}.matches(\"{pat}\")")),
        5 => p(format!("{s}.matches(\"{pat}\") + \"|\" + {s}.replaceAll(\"{pat}\", \"#\")")),
        _ => format!(
            "try {{ System.out.println({s}.replaceAll(\"{pat}\", {rep})); }} catch (RuntimeException e) {{ System.out.println(e.getClass().getSimpleName()); }}"
        ),
    }
}

/// Declaration statements with more than one declarator, and the C-style array
/// suffix that binds to the *declarator* rather than the type — so
/// `int a[], b;` declares an `int[]` and an `int`. Both spellings appear in
/// locals, in `for` init clauses, and in the initializer that reads the
/// declarator before it.
fn g_decl(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let d = pick(r, DIVS);
    let f = pick(r, DBLS);
    let s = pick(r, STRS);
    let c = pick(r, CHARS);
    match r.below(14) {
        0 => format!("{{ int x = {a}, y = {b}; System.out.println(x + \",\" + y + \",\" + (x + y)); }}"),
        1 => format!("{{ int x = {a}, y = x + {b}, z = y * 2; System.out.println(x + \",\" + y + \",\" + z); }}"),
        2 => format!("{{ int x, y = {b}, z; x = {a}; z = x + y; System.out.println(x + \",\" + y + \",\" + z); }}"),
        3 => format!("{{ final int x = {a}, y = {b}; System.out.println(x / {d} + \",\" + y % {d}); }}"),
        4 => format!("{{ int p[] = {{{a}, {b}}}, q = {d}; System.out.println(p.length + \",\" + p[1] + \",\" + q); }}"),
        5 => format!("{{ int p[] = {{{a}}}, q[][] = {{{{{b}}}, {{{d}, 9}}}}; System.out.println(p[0] + \",\" + q[1][1] + \",\" + q.length); }}"),
        6 => format!("{{ int[] u = {{{a}}}, v = {{{b}, {d}}}; System.out.println(u[0] + v.length); }}"),
        7 => format!("{{ double x = {f}, y = x * 2; System.out.println(x + \",\" + y); }}"),
        8 => format!("{{ String x = {s}, y = x + \"z\"; System.out.println(y + \",\" + y.length()); }}"),
        9 => format!("{{ long m = {a}, n = {b}; System.out.println(m * n + \",\" + (m - n)); }}"),
        10 => format!("{{ char c1 = '{c}', c2 = (char) (c1 + 1); System.out.println(\"\" + c1 + c2 + \",\" + (c1 + 0)); }}"),
        11 => "{ boolean t = true, u = false; System.out.println(t + \",\" + u + \",\" + (t & u)); }".to_string(),
        12 => format!("{{ int t = 0; for (int i = {a}, n = i + 3; i < n; i++) {{ t += i; }} System.out.println(t); }}"),
        _ => format!("{{ int t = 0; for (int i = 0, j = 4; i < j; i++, j--) {{ t += i * {d} + j; }} System.out.println(t); }}"),
    }
}

/// `new Object()` and the methods every class inherits from it. The identity
/// hash itself is never printed — Java's is a JVM value that differs run to run
/// — so the probes assert the properties a program can depend on: reference
/// identity, the shape of the default `toString`, the class name, and the way an
/// `Object` behaves as a key, an element, and a monitor.
fn g_object(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    match r.below(12) {
        0 => "{ Object o = new Object(), p = new Object(); System.out.println((o == p) + \",\" + (o == o) + \",\" + o.equals(o) + \",\" + o.equals(p)); }".to_string(),
        1 => "{ Object o = new Object(); System.out.println(o.getClass().getName() + \",\" + o.getClass().getSimpleName()); }".to_string(),
        2 => "{ Object o = new Object(); System.out.println(o.toString().startsWith(\"java.lang.Object@\")); }".to_string(),
        3 => "{ Object o = new Object(); System.out.println((o.hashCode() == o.hashCode()) + \",\" + (o.hashCode() == new Object().hashCode())); }".to_string(),
        4 => "{ Object o = new Object(); System.out.println(String.valueOf(o).startsWith(\"java.lang.Object@\")); }".to_string(),
        5 => "{ Object o = new Object(), p = new Object(); List<Object> l = new ArrayList<>(); l.add(o); l.add(p); System.out.println(l.size() + \",\" + l.indexOf(p) + \",\" + l.contains(o)); }".to_string(),
        6 => "{ Object o = new Object(), p = new Object(); Set<Object> s = new HashSet<>(); s.add(o); s.add(o); s.add(p); System.out.println(s.size()); }".to_string(),
        7 => "{ Object o = new Object(), p = new Object(); Map<Object, String> m = new HashMap<>(); m.put(o, \"A\"); m.put(p, \"B\"); System.out.println(m.get(o) + m.get(p) + m.size()); }".to_string(),
        8 => format!("{{ Object lock = new Object(); int t = 0; synchronized (lock) {{ t += {a}; }} System.out.println(t); }}"),
        9 => format!("{{ Bare b1 = new Bare({a}), b2 = new Bare({a}); System.out.println(b1.equals(b1) + \",\" + b1.equals(b2) + \",\" + b1.toString().startsWith(\"Bare@\")); }}"),
        10 => format!("{{ Object[] xs = new Object[2]; xs[0] = new Object(); xs[1] = xs[0]; System.out.println((xs[0] == xs[1]) + \",\" + xs.length + \",\" + {a}); }}"),
        _ => format!("{{ Object miss = new Object(); Map<String, Object> m = new HashMap<>(); m.put(\"k\", miss); System.out.println((m.get(\"k\") == miss) + \",\" + (m.getOrDefault(\"z\", miss) == miss) + \",\" + {a}); }}"),
    }
}

/// Helper declarations the `finally`/`resource` probes call. Emitted into every
/// generated program (they are inert when unused).
/// A **qualified** static call, `C.m(args)`, where more than one class in the
/// program declares `m`.
///
/// Java resolves a qualified static in the receiver class's own declarations
/// first and then up its superclass chain; it never reaches an unrelated class.
/// The pre-existing modes could not observe that rule, because every probe they
/// emitted lived in a program whose static names were unique — with one name per
/// method a by-name-only resolver is indistinguishable from a correct one. These
/// probes put a same-named static in a *second* class and then ask which body
/// ran, which is the only shape that separates the two.
fn g_staticref(r: &mut Rng) -> String {
    let n = pick(r, &["0", "1", "2", "7", "-3"]);
    let s = pick(r, &["\"x\"", "\"\"", "\"ab\""]);
    match r.below(10) {
        // Same name, same signature, different class: the receiver alone decides.
        0 => p(format!("Qa.f({n})")),
        1 => p(format!("Qb.f({n})")),
        2 => p(format!("Qa.f({n}) + \",\" + Qb.f({n})")),
        // Same name, *different* parameter types: a resolver that scores
        // signatures across the whole program picks the closer one and crosses
        // classes; Java scores only inside the named class.
        3 => p(format!("Qa.g({s})")),
        4 => p(format!("Qb.g({s})")),
        5 => p(format!("Qa.g({s}) + Qb.g({s})")),
        // Zero-arg statics whose signatures are identical in every way but owner.
        6 => p("Qa.h() + Qb.h()".to_string()),
        // A subclass hides an inherited static; the name it does not redeclare
        // still resolves up the chain.
        7 => p("Qbase.kind() + \"/\" + Qderiv.kind()".to_string()),
        8 => p("Qderiv.only() + Qbase.only()".to_string()),
        // The chosen body's result feeding an expression, so a wrong target is
        // visible as a wrong number rather than only as a wrong label.
        _ => format!(
            "{{ int q = Qb.f({n}) * 2 + Qa.f({n}); System.out.println(q + Qbase.kind()); }}"
        ),
    }
}

/// `while` and `do`/`while` with `continue` and `break`.
///
/// `continue` lowers differently in each of Java's three loops — a `for` runs its
/// update clause first, a `while` jumps straight back to the condition, and a
/// `do`/`while` jumps *forward* to the trailing condition — so they are three
/// separate code paths in the compiler. Every loop probe that existed emitted a
/// `for` (the sole `while` was a `while (true)` with a `break`), which left two
/// of the three lowerings and the whole `do`/`while` statement ungenerated. A
/// sibling frontend shipped a `continue` that compiled to a jump-to-zero — an
/// infinite loop — under exactly this blind spot.
fn g_loopkind(r: &mut Rng) -> String {
    let n = 3 + r.below(4);
    let skip = r.below(3);
    match r.below(12) {
        // `continue` in a `while`: the condition is re-tested, nothing is stepped.
        0 => format!(
            "{{ int i = 0, acc = 0; while (i < {n}) {{ i++; if (i == {skip}) continue; acc += i; }} System.out.println(acc); }}"
        ),
        // `continue` in a `do`/`while`: jumps to the *trailing* condition.
        1 => format!(
            "{{ int i = 0, acc = 0; do {{ i++; if (i == {skip}) continue; acc += i; }} while (i < {n}); System.out.println(acc); }}"
        ),
        // `break` out of a `do`/`while`.
        2 => format!(
            "{{ int i = 0, acc = 0; do {{ i++; if (i == {skip}) break; acc += i; }} while (i < {n}); System.out.println(acc); }}"
        ),
        // The body of a `do`/`while` always runs once, however false the test.
        3 => "{ int k = 0; do { k++; } while (false); System.out.println(k); }".to_string(),
        // A labelled `continue` stepping an outer `while` from an inner one.
        4 => format!(
            "{{ int i = 0; String o = \"\"; L: while (i < {n}) {{ i++; int j = 0; while (j < 3) {{ j++; if (j == 2) continue L; o += i + \"\" + j + \" \"; }} }} System.out.println(o); }}"
        ),
        // A labelled `continue` on a `do`/`while`.
        5 => format!(
            "{{ int i = 0; String o = \"\"; D: do {{ i++; if (i % 2 == 0) continue D; o += i; }} while (i < {n}); System.out.println(o); }}"
        ),
        // A labelled `break` out of a `do`/`while` nest.
        6 => format!(
            "{{ int t = 0; U: do {{ int j = 0; while (j < {n}) {{ j++; if (j == 2) break U; t += j; }} }} while (t < 100); System.out.println(t); }}"
        ),
        // `continue` inside a `switch` inside a loop — the `switch` owns `break`
        // but not `continue`, so the jump has to pass through it.
        7 => format!(
            "{{ int acc = 0; for (int i = 0; i < {n}; i++) {{ switch (i) {{ case 1: continue; case 2: acc += 100; break; default: acc += i; }} }} System.out.println(acc); }}"
        ),
        8 => format!(
            "{{ int i = 0, acc = 0; while (i < {n}) {{ i++; switch (i) {{ case 2: continue; default: acc += i; }} }} System.out.println(acc); }}"
        ),
        // `continue` in a `while` whose body carries a `finally` — the cleanup
        // has to run on the way out of the iteration.
        9 => format!(
            "{{ int i = 0, acc = 0; while (i < {n}) {{ i++; try {{ if (i == {skip}) continue; acc += i; }} finally {{ System.out.print(\"f\" + i); }} }} System.out.println(\" \" + acc); }}"
        ),
        10 => format!(
            "{{ int i = 0, acc = 0; do {{ i++; try {{ if (i == {skip}) continue; acc += i; }} finally {{ System.out.print(\"g\" + i); }} }} while (i < {n}); System.out.println(\" \" + acc); }}"
        ),
        // Two unlabelled `continue`s nested two deep: each steps its own loop.
        _ => format!(
            "{{ String o = \"\"; int i = 0; while (i < {n}) {{ i++; if (i == 1) continue; int j = 0; while (j < 3) {{ j++; if (j == 2) continue; o += i + \"\" + j; }} }} System.out.println(o); }}"
        ),
    }
}

/// `byte` and `short` — the two integral widths narrower than `int`.
///
/// Every fusevm number is a 64-bit value with no width tag, so each of Java's
/// widths is a per-site narrowing the compiler has to emit. The `overflow` mode
/// covers `int` and the `char` mode covers the unsigned 16-bit one, but no probe
/// declared a `byte` or a `short` at all — and their signed 8/16-bit wraps are
/// separate emit sites. Compound assignment is the sharp edge: Java inserts an
/// *implicit* narrowing cast into `b += 100`, so it wraps where the equivalent
/// `b = b + 100` would not even compile.
fn g_narrow(r: &mut Rng) -> String {
    let k = pick(r, &["1", "2", "100", "127", "-128", "1000", "10000"]);
    match r.below(11) {
        // Compound assignment carries an implicit narrowing cast.
        0 => format!("{{ byte b = 100; b += {k}; System.out.println(b); }}"),
        1 => format!("{{ short s = 30000; s += {k}; System.out.println(s); }}"),
        2 => format!("{{ byte b = 100; b *= {k}; System.out.println(b); }}"),
        3 => format!("{{ short s = 1000; s *= {k}; System.out.println(s); }}"),
        // `++`/`--` at the width's boundary.
        4 => "{ byte b = 127; b++; System.out.println(b); }".to_string(),
        5 => "{ short s = 32767; s++; System.out.println(s); }".to_string(),
        6 => "{ byte b = -128; b--; System.out.println(b); }".to_string(),
        // An explicit cast past the width.
        7 => format!("System.out.println((byte) ({k} * 3) + \",\" + (short) ({k} * 700));"),
        // A `byte` promotes to `int` in arithmetic, so the *expression* does not
        // wrap even though the variable would.
        8 => format!("{{ byte b = 100; int w = b * 3 + {k}; System.out.println(w); }}"),
        // The sign-extension idiom, and the shifts a signed narrow type feeds.
        9 => "{ byte b = -1; System.out.println((b & 0xFF) + \",\" + (b >> 1) + \",\" + (b >>> 24)); }"
            .to_string(),
        // An array element is a storage site of the same width.
        _ => format!(
            "{{ byte[] ba = {{100}}; short[] sa = {{30000}}; ba[0] += {k}; sa[0] += {k}; System.out.println(ba[0] + \",\" + sa[0]); }}"
        ),
    }
}

/// Operations *on* a constructed collection, rather than its construction.
///
/// The `collection` mode builds lists and maps and prints them; what it does not
/// do is apply the methods whose meaning depends on the argument's static type.
/// `List` declares both `remove(int)` and `remove(Object)` and Java chooses
/// between them at compile time, so the same call text removes an *index* or a
/// *value* depending on how the argument was declared — a difference no
/// construction probe can see. Every probe here has a deterministic order
/// (`ArrayList`, `TreeMap`, `TreeSet`), never a `HashMap` iteration.
fn g_listop(r: &mut Rng) -> String {
    let i = r.below(3);
    let v = pick(r, &["10", "20", "30", "99"]);
    let s = pick(r, &["\"a\"", "\"b\"", "\"z\""]);
    match r.below(12) {
        // `remove(int)` — an `int`-typed argument is an index.
        0 => format!(
            "{{ List<Integer> l = new ArrayList<>(Arrays.asList(10, 20, 30)); l.remove({i}); System.out.println(l); }}"
        ),
        1 => format!(
            "{{ List<Integer> l = new ArrayList<>(Arrays.asList(10, 20, 30)); int ix = {i}; l.remove(ix); System.out.println(l); }}"
        ),
        // `remove(Object)` — a boxed or reference argument is a *value*, and the
        // call answers whether one was found.
        2 => format!(
            "{{ List<Integer> l = new ArrayList<>(Arrays.asList(10, 20, 30)); boolean hit = l.remove(Integer.valueOf({v})); System.out.println(hit + \" \" + l); }}"
        ),
        3 => format!(
            "{{ List<Integer> l = new ArrayList<>(Arrays.asList(10, 20, 30)); Integer k = {v}; System.out.println(l.remove(k) + \" \" + l); }}"
        ),
        4 => format!(
            "{{ List<String> l = new ArrayList<>(Arrays.asList(\"a\", \"b\", \"c\")); System.out.println(l.remove({s}) + \" \" + l); }}"
        ),
        // `String.join` over an *Iterable*, Java's second overload — the one that
        // joins elements rather than rendering the collection.
        5 => format!(
            "{{ List<String> l = new ArrayList<>(Arrays.asList(\"a\", \"b\", \"c\")); l.remove({s}); System.out.println(String.join(\"-\", l)); }}"
        ),
        6 => "{ Set<String> st = new TreeSet<>(Arrays.asList(\"q\", \"p\", \"q\")); System.out.println(String.join(\",\", st)); }"
            .to_string(),
        // Ordering, searching and in-place mutation of a built list.
        7 => format!(
            "{{ List<Integer> l = new ArrayList<>(Arrays.asList(30, 10, 20)); Collections.sort(l); l.set({i}, 5); System.out.println(l + \" \" + l.indexOf(5)); }}"
        ),
        8 => "{ List<Integer> l = new ArrayList<>(Arrays.asList(3, 1, 2)); Collections.reverse(l); System.out.println(l + \" \" + Collections.max(l) + Collections.min(l)); }"
            .to_string(),
        9 => format!(
            "{{ List<Integer> l = new ArrayList<>(Arrays.asList(1, 2)); l.add({i}, 9); System.out.println(l + \" \" + l.contains(9) + l.lastIndexOf(9)); }}"
        ),
        // A `TreeMap` keeps key order, so iterating it is deterministic.
        10 => format!(
            "{{ Map<String, Integer> m = new TreeMap<>(); m.put(\"b\", 2); m.put(\"a\", 1); m.remove({s}); int t = 0; for (String k : m.keySet()) t += m.get(k); System.out.println(m + \" \" + t); }}"
        ),
        // Removing while the value is absent must leave the list untouched.
        _ => format!(
            "{{ List<Integer> l = new ArrayList<>(Arrays.asList(10, 20)); System.out.println(l.remove(Integer.valueOf({v})) + \" \" + l.size() + \" \" + l); }}"
        ),
    }
}

/// Calls to **user-declared variable-arity** methods, constructors and instance
/// methods — a shape no other mode emits, because no other mode writes a `...`
/// parameter at all.
///
/// The interesting part is not that `sum(1, 2, 3)` runs; it is *which* of
/// several declarations a call selects, which Java answers with three ordered
/// resolution phases. A generator that only ever declared one variadic method
/// could not tell a correct resolver from one that ignores the ordering, so
/// every probe here calls into a set where at least two declarations compete:
/// a fixed-arity overload that must win outright, an array argument that must
/// pass through unpacked, and element types (`String…`/`Object…`, `int…`/
/// `double…`) whose specificity decides the winner.
fn g_varargs(r: &mut Rng) -> String {
    let n = pick(r, &["1", "2", "7", "-3"]);
    let s = pick(r, &["\"a\"", "\"b\"", "\"zz\""]);
    match r.below(16) {
        // Arity: none, one, several — the same declaration has to take all of
        // them, and `xs.length` proves what actually arrived.
        0 => format!("System.out.println(vsum({n}, {n}, {n}));"),
        1 => format!("System.out.println(vsum({n}));"),
        2 => "System.out.println(vsum());".to_string(),
        // An *array* argument matches the declared `int[]` in the fixed-arity
        // phase, so it is passed straight through rather than wrapped in a
        // one-element array.
        3 => format!("{{ int[] a = {{{n}, 4}}; System.out.println(vsum(a)); }}"),
        4 => format!("System.out.println(vsum(new int[]{{{n}, 5, 6}}));"),
        // A fixed-arity overload must beat the variadic one for the arity it
        // declares, and lose for every other.
        5 => format!("System.out.println(vpick({n}, 2));"),
        6 => format!("System.out.println(vpick({n}));"),
        7 => format!("System.out.println(vpick({n}, 2, 3));"),
        8 => "System.out.println(vpick());".to_string(),
        // Element-type specificity between two variadic candidates.
        9 => format!("System.out.println(vkind({s}, {s}));"),
        10 => format!("System.out.println(vkind({n}, {n}));"),
        11 => "System.out.println(vkind());".to_string(),
        // A fixed prefix ahead of the variadic tail, and the zero-length tail.
        12 => format!("System.out.println(vtag({s}, {n}, {n}));"),
        13 => format!("System.out.println(vtag({s}));"),
        // A constructor and an instance method, both variadic, plus the
        // `(Object[]) null` cast Java passes as the whole array.
        14 => format!(
            "{{ Bag b = new Bag({n}, {n}); System.out.println(b.total() + \" \" + b.plus({n})); }}"
        ),
        _ => "System.out.println(vnull((Object[]) null) + \" \" + vnull(1, 2));".to_string(),
    }
}

/// The `List.of`/`Set.of`/`Map.of` contract, which is three answers and not one.
///
/// The `listop` and `listview` modes build collections and read them back; both
/// stay on the *accepting* side of the factories, so the whole refusing side was
/// unreachable and three separate divergences hid behind clean sweeps:
///
///   * An out-of-range index does not name one exception. `ArrayList` raises
///     `IndexOutOfBoundsException`, `Arrays.asList` and a three-or-more-element
///     `List.of` let a backing array raise `ArrayIndexOutOfBoundsException`, and
///     a one-or-two-element `List.of` raises `IndexOutOfBoundsException` with a
///     different wording again (`Index: 5 Size: 2`). Only a probe that prints
///     `getClass().getName()` alongside `getMessage()` can see the difference —
///     printing the message alone agrees on two of the four.
///   * `List.of(1, null)` throws before the list exists.
///   * `List.of(1, 2).contains(null)` throws rather than answering `false`, and
///     so do `indexOf`/`lastIndexOf`, `Set.of.contains`, and `Map.of`'s
///     `get`/`containsKey`/`containsValue`.
///
/// Every probe reports through `getClass().getName()`, because the three list
/// shapes differ in the exception *class* well before they differ in its text,
/// and a `RuntimeException` catch that printed only the message would call two
/// of them equal. The receiver sizes are drawn to straddle the JDK's own
/// `List12`/`ListN` and `Set12`/`SetN` and `Map1`/`MapN` boundaries, since that
/// arity is what selects the reporting path.
fn g_immutable(r: &mut Rng) -> String {
    let idx = pick(r, &["3", "5", "-1", "-2", "9"]);
    let report =
        "catch (RuntimeException e) { System.out.println(e.getClass().getName() + \" \" + e.getMessage()); }";
    match r.below(16) {
        // bounds, one receiver shape per arm
        0 => format!("try {{ new ArrayList<>(List.of(1, 2)).get({idx}); }} {report}"),
        1 => format!("try {{ Arrays.asList(1, 2).get({idx}); }} {report}"),
        2 => format!("try {{ List.of(1).get({idx}); }} {report}"),
        3 => format!("try {{ List.of(1, 2).get({idx}); }} {report}"),
        4 => format!("try {{ List.of(1, 2, 3).get({idx}); }} {report}"),
        5 => format!("try {{ List.of(1, 2, 3, 4, 5).get({idx}); }} {report}"),
        6 => format!("try {{ List.of().get({idx}); }} {report}"),
        7 => format!("try {{ Arrays.asList(1, 2, 3).set({idx}, 0); }} {report}"),
        // a structural refusal outranks a bad index on an immutable receiver
        8 => format!("try {{ List.of(1, 2).set({idx}, 0); }} {report}"),
        9 => format!("try {{ List.of(1, 2).remove({idx}); }} {report}"),
        // a null the factory is built from
        10 => "try { List.of(1, null); } catch (NullPointerException e) { System.out.println(\"npe \" + e.getMessage()); }".to_string(),
        11 => "try { List.of(1, 2, null, 4); } catch (NullPointerException e) { System.out.println(\"npe \" + e.getMessage()); }".to_string(),
        // a null the collection is asked about — the JDK's message here is its
        // helpful-NPE text for a `Set12`/`Map1` receiver, which BUGS.md records
        // javars cannot reproduce, so those arities stay out of the pool and
        // the ones whose message is `requireNonNull`'s own stay in.
        12 => format!("try {{ List.of(1, 2).contains(null); }} {report}"),
        13 => format!("try {{ List.of(1, 2).indexOf(null); }} {report}"),
        14 => format!("try {{ Set.of(1, 2, 3).contains(null); }} {report}"),
        _ => format!("try {{ Map.of(\"a\", 1, \"b\", 2).containsKey(null); }} {report}"),
    }
}

/// `List.subList` **views**, whose defining property is that they alias their
/// backing list rather than copy it.
///
/// A copy answers every read correctly, so read-only probes cannot distinguish
/// one from a view — which is exactly why this mode writes through both ends.
/// Each probe either mutates the parent and reads the view, mutates the view
/// and reads the parent, or structurally modifies the parent and then touches
/// the view, where Java raises `ConcurrentModificationException`.
fn g_listview(r: &mut Rng) -> String {
    let v = pick(r, &["7", "8", "99"]);
    let init = "List<Integer> l = new ArrayList<>(Arrays.asList(10, 20, 30, 40, 50));";
    match r.below(14) {
        0 => format!("{{ {init} System.out.println(l.subList(1, 4)); }}"),
        1 => format!("{{ {init} System.out.println(l.subList(0, 0) + \" \" + l.subList(2, 2).isEmpty()); }}"),
        2 => format!("{{ {init} System.out.println(l.subList(1, 4).size() + \" \" + l.subList(1, 4).get(1)); }}"),
        // parent write → visible through the view
        3 => format!("{{ {init} List<Integer> s = l.subList(1, 4); l.set(2, {v}); System.out.println(s); }}"),
        4 => format!("{{ {init} List<Integer> s = l.subList(0, 3); l.set(0, {v}); System.out.println(s.get(0) + \" \" + s.contains({v})); }}"),
        // view write → visible in the parent
        5 => format!("{{ {init} List<Integer> s = l.subList(1, 3); s.set(0, {v}); System.out.println(l); }}"),
        6 => format!("{{ {init} List<Integer> s = l.subList(2, 5); s.add({v}); System.out.println(l + \" \" + s); }}"),
        7 => format!("{{ {init} List<Integer> s = l.subList(1, 4); s.remove(0); System.out.println(l + \" \" + s); }}"),
        8 => format!("{{ {init} List<Integer> s = l.subList(1, 3); s.clear(); System.out.println(l + \" \" + s.size()); }}"),
        // a view of a view still reaches the same backing list
        9 => format!("{{ {init} List<Integer> s = l.subList(0, 4).subList(1, 3); s.set(0, {v}); System.out.println(l + \" \" + s); }}"),
        // reads that must agree with the equivalent plain list
        10 => format!("{{ {init} List<Integer> s = l.subList(1, 4); System.out.println(s.indexOf(30) + \" \" + s.equals(Arrays.asList(20, 30, 40))); }}"),
        11 => format!("{{ {init} List<Integer> s = l.subList(1, 4); int t = 0; for (int x : s) t += x; System.out.println(t); }}"),
        // a structural change to the parent invalidates the view
        12 => format!("{{ {init} List<Integer> s = l.subList(1, 3); l.add({v}); try {{ System.out.println(s.get(0)); }} catch (RuntimeException e) {{ System.out.println(e); }} }}"),
        _ => format!("{{ {init} List<Integer> s = l.subList(1, 3); l.remove(0); try {{ System.out.println(s.size()); }} catch (RuntimeException e) {{ System.out.println(e); }} }}"),
    }
}

/// `super.member` — the qualified access that names the **superclass's**
/// implementation of something the receiver's own class overrides.
///
/// It is the one call in Java that must NOT dispatch virtually, and a generator
/// that never writes `super.` cannot tell a correct lowering from no lowering at
/// all: every probe here would still be reachable through a plain `this.`, and
/// most would then either recurse forever or answer the override's value. The
/// support chain (`SupA`/`SupB`/`SupC`) is built so each shape has a distinct
/// wrong answer — see its comment in [`SUPPORT_CLASS`].
fn g_super(r: &mut Rng) -> String {
    let n = pick(r, &["0", "1", "2", "7", "-3"]);
    let s = pick(r, &["\"x\"", "\"\"", "\"zz\""]);
    match r.below(16) {
        // The whole chain: `SupC.f` → `SupB.f` → `SupA.f`. A virtual `super`
        // re-enters `SupC.f` and never returns.
        0 => format!("System.out.println(new SupC().f({n}));"),
        // Entering the same chain through the *base* static type — the override
        // is selected virtually, and only then does `super` stop being virtual.
        1 => format!("{{ SupA a = new SupC(); System.out.println(a.f({n})); }}"),
        2 => format!("{{ SupB b = new SupC(); System.out.println(b.f({n}) + \" \" + new SupB().f({n})); }}"),
        // `super.toString()` at two levels, plus `super.tag()` reaching the
        // grandparent's body past a parent that does not declare it.
        3 => "System.out.println(new SupC().toString());".to_string(),
        4 => "System.out.println(new SupB() + \" \" + new SupA());".to_string(),
        // The callee reached by `super` still dispatches its own unqualified
        // calls virtually, so `kind()` sees `SupC`'s `tag()`.
        5 => "System.out.println(new SupC().kind() + \" \" + new SupB().kind());".to_string(),
        // Overload selection happens at the superclass: a `double` argument
        // picks `f(double)`, an `int` picks `f(int)`.
        6 => format!("System.out.println(new SupC().fd() + \" \" + new SupC().f({n}));"),
        // A variadic `super` call, both spellings: an array that passes through
        // unpacked and a loose argument list that packs.
        7 => format!("System.out.println(new SupC().vs({n}, 2));"),
        8 => format!("{{ int[] a = {{{n}, 4}}; System.out.println(new SupC().vs(a)); }}"),
        9 => "System.out.println(new SupC().vs());".to_string(),
        // `super.field`, read / compound-assigned / incremented.
        10 => "System.out.println(new SupC().bump());".to_string(),
        11 => format!("{{ SupC c = new SupC(); c.bump(); System.out.println(c.f({n}) + \" \" + c); }}"),
        // `super` inside a lambda body (which outlives the frame it captured)
        // and behind a method reference.
        12 => format!("System.out.println(new SupC().lam({n}).of());"),
        13 => format!("System.out.println(new SupC().mref().of({s}) + new SupC().echo({s}));"),
        // `super.equals` where the superclass is the implicit `java.lang.Object`
        // — reference identity, which is deterministic (its `toString` and
        // `hashCode` are not, and are left ungenerated).
        14 => format!("{{ SupObj o = new SupObj({n}); System.out.println(o.sameAs(o) + \" \" + o.sameAs(new SupObj({n}))); }}"),
        _ => format!("{{ SupA[] xs = {{new SupA(), new SupB(), new SupC()}}; String t = \"\"; for (SupA a : xs) t += a.f({n}) + \",\"; System.out.println(t); }}"),
    }
}

/// `instanceof` — the runtime type test, asked of every shape javars's value
/// model can name.
///
/// No probe in this generator wrote `instanceof` before this mode, so the whole
/// form was unreachable and every clean sweep above was silent about it.
///
/// Every probe prints a *true* and a *false* answer drawn from one target
/// family, so neither a blanket `true` nor a blanket `false` satisfies one.
/// That shape is the point: the test a single-answer implementation passes is
/// the one that only ever asks about a type the value is not, and asking both
/// halves of the same question is what separates a real supertype walk from a
/// constant.
///
/// Deliberately absent, because javars's value model cannot decide them and a
/// probe would only re-report a documented BUGS.md simplification rather than
/// find anything:
///   * `new LinkedList<>()` — modeled as the same mutable list an `ArrayList`
///     is, so no test can separate the two.
///   * `Set.of(…)` — modeled as a hash-ordered `Set`, indistinguishable from
///     `new HashSet<>()`, where Java's `ImmutableCollections.SetN` is not one.
///   * a lambda against its functional interface — the closure records the body
///     and its captures, not the interface it was assigned to.
///   * `Long`/`Float`/`Character` — `int` and `long` are one `Value::Int`, a
///     `double` and a `float` one `Value::Float`, and a boxed `char` is the
///     one-character `String` javars models it as.
fn g_instanceof(r: &mut Rng) -> String {
    let i = pick(r, INTS);
    let d = pick(r, DBLS);
    let s = pick(r, STRS);
    let b = pick(r, BOOLS);
    match r.below(16) {
        // Each boxed primitive against its own wrapper, the wrapper it is *not*,
        // and the `java.lang` interfaces the JLS gives it.
        0 => format!(
            "{{ Object v = {i}; System.out.println((v instanceof Integer) + \",\" + (v instanceof Double) + \",\" + (v instanceof Boolean) + \",\" + (v instanceof Number) + \",\" + (v instanceof Comparable) + \",\" + (v instanceof String) + \",\" + (v instanceof Object)); }}"
        ),
        1 => format!(
            "{{ Object v = {d}; System.out.println((v instanceof Double) + \",\" + (v instanceof Integer) + \",\" + (v instanceof Number) + \",\" + (v instanceof Comparable) + \",\" + (v instanceof CharSequence) + \",\" + (v instanceof Object)); }}"
        ),
        2 => format!(
            "{{ Object v = {b}; System.out.println((v instanceof Boolean) + \",\" + (v instanceof Number) + \",\" + (v instanceof Comparable) + \",\" + (v instanceof String) + \",\" + (v instanceof Object)); }}"
        ),
        3 => format!(
            "{{ Object v = {s}; System.out.println((v instanceof String) + \",\" + (v instanceof CharSequence) + \",\" + (v instanceof Comparable) + \",\" + (v instanceof Number) + \",\" + (v instanceof List) + \",\" + (v instanceof Object)); }}"
        ),
        // `instanceof Object` over every shape at once. Java answers it `true`
        // for every non-null reference and `false` for `null`, so one blanket
        // answer fails on the other end whichever way it is chosen.
        4 => "{ Object[] vs = { 1, 2.5, true, \"s\", new int[]{1}, new ArrayList<Integer>(), new HashMap<String, Integer>(), new HashSet<Integer>(), new Left(), Color.RED, new Pt(1, 2), new IllegalStateException(\"e\"), null }; String out = \"\"; for (Object v : vs) out += (v instanceof Object) ? \"T\" : \"F\"; System.out.println(out); }".to_string(),
        // A declared subclass chain plus an interface only one sibling carries.
        5 => {
            let v = pick(r, &["new Left()", "new Right()", "new Base()", "new Bare(1)"]);
            format!(
                "{{ Object v = {v}; System.out.println((v instanceof Base) + \",\" + (v instanceof Left) + \",\" + (v instanceof Right) + \",\" + (v instanceof Marker) + \",\" + (v instanceof Bare) + \",\" + (v instanceof Object)); }}"
            )
        }
        // An enum constant is an instance of its own enum, of `java.lang.Enum`,
        // and of `Comparable` — and never of a *different* enum in the program.
        6 => {
            let c = pick(r, &["Color.RED", "Color.BLUE", "Op.ADD", "Ops.TIMES"]);
            format!(
                "{{ Object v = {c}; System.out.println((v instanceof Color) + \",\" + (v instanceof Op) + \",\" + (v instanceof Enum) + \",\" + (v instanceof Comparable) + \",\" + (v instanceof Object)); }}"
            )
        }
        // A record is an instance of `java.lang.Record`, and of no other record.
        7 => {
            let v = pick(r, &["new Pt(1, 2)", "new Tag(\"t\", 1.5)"]);
            format!(
                "{{ Object v = {v}; System.out.println((v instanceof Pt) + \",\" + (v instanceof Tag) + \",\" + (v instanceof Record) + \",\" + (v instanceof Comparable) + \",\" + (v instanceof Object)); }}"
            )
        }
        // The throwable chain, asked as an expression rather than through
        // `catch` — the same builtin decides both, and only `catch` was reachable.
        8 => {
            let v = pick(
                r,
                &[
                    "new IllegalArgumentException(\"e\")",
                    "new IllegalStateException(\"e\")",
                    "new ArithmeticException(\"e\")",
                    "new NumberFormatException(\"e\")",
                ],
            );
            format!(
                "{{ Object v = {v}; System.out.println((v instanceof IllegalArgumentException) + \",\" + (v instanceof RuntimeException) + \",\" + (v instanceof Exception) + \",\" + (v instanceof Throwable) + \",\" + (v instanceof Error) + \",\" + (v instanceof Object)); }}"
            )
        }
        // A mutable list, against both the interfaces it implements and the
        // collection kinds it is not.
        9 => "{ Object v = new ArrayList<Integer>(); System.out.println((v instanceof List) + \",\" + (v instanceof Collection) + \",\" + (v instanceof Iterable) + \",\" + (v instanceof ArrayList) + \",\" + (v instanceof Map) + \",\" + (v instanceof Set) + \",\" + (v instanceof Object)); }".to_string(),
        // The list *views*: `List.of` and `Arrays.asList` are `List`s that are
        // not `ArrayList`s, which is the half a name-only answer gets wrong.
        10 => {
            let v = pick(
                r,
                &[
                    "List.of(1, 2)",
                    "Arrays.asList(1, 2)",
                    "new ArrayList<Integer>(List.of(1, 2, 3)).subList(0, 2)",
                ],
            );
            format!(
                "{{ Object v = {v}; System.out.println((v instanceof List) + \",\" + (v instanceof Collection) + \",\" + (v instanceof ArrayList) + \",\" + (v instanceof Set) + \",\" + (v instanceof Object)); }}"
            )
        }
        // The three map kinds. `LinkedHashMap` extends `HashMap` and `TreeMap`
        // does not, so the pair only agrees with Java if the *declared* graph is
        // walked rather than the name matched.
        11 => {
            let v = pick(
                r,
                &[
                    "new HashMap<String, Integer>()",
                    "new LinkedHashMap<String, Integer>()",
                    "new TreeMap<String, Integer>()",
                ],
            );
            format!(
                "{{ Object v = {v}; System.out.println((v instanceof Map) + \",\" + (v instanceof HashMap) + \",\" + (v instanceof LinkedHashMap) + \",\" + (v instanceof TreeMap) + \",\" + (v instanceof SortedMap) + \",\" + (v instanceof Collection) + \",\" + (v instanceof Object)); }}"
            )
        }
        // The three set kinds, the same way: `LinkedHashSet` extends `HashSet`,
        // `TreeSet` does not, and neither is a `List`.
        12 => {
            let v = pick(
                r,
                &[
                    "new HashSet<Integer>()",
                    "new LinkedHashSet<Integer>()",
                    "new TreeSet<Integer>()",
                ],
            );
            format!(
                "{{ Object v = {v}; System.out.println((v instanceof Set) + \",\" + (v instanceof HashSet) + \",\" + (v instanceof LinkedHashSet) + \",\" + (v instanceof TreeSet) + \",\" + (v instanceof SortedSet) + \",\" + (v instanceof List) + \",\" + (v instanceof Object)); }}"
            )
        }
        // An array is an `Object` and a `Cloneable`, and is not a collection —
        // the element type it erases decides none of those.
        13 => {
            let v = pick(
                r,
                &[
                    "new int[]{1, 2}",
                    "new String[]{\"a\"}",
                    "new Object[]{1, \"a\"}",
                    "new Base[]{new Left()}",
                ],
            );
            format!(
                "{{ Object v = {v}; System.out.println((v instanceof Object) + \",\" + (v instanceof Cloneable) + \",\" + (v instanceof List) + \",\" + (v instanceof String) + \",\" + (v instanceof Base)); }}"
            )
        }
        // `instanceof` driving control flow, where the answer picks a branch
        // rather than being printed — and `null`, which is an instance of
        // nothing at all, including `Object`.
        14 => {
            let v = pick(r, &["new Left()", "new Right()", "new Base()"]);
            format!(
                "{{ Object v = {v}; String out; if (v instanceof Left) {{ out = \"L\"; }} else if (v instanceof Right) {{ out = \"R\"; }} else if (v instanceof Base) {{ out = \"B\"; }} else {{ out = \"?\"; }} Object n = null; System.out.println(out + \",\" + (n instanceof Object) + \",\" + (n instanceof Base) + \",\" + (!(v instanceof Marker)) + \",\" + ((v instanceof Base) && !(v instanceof Bare))); }}"
            )
        }
        // The test inside a loop over a mixed array, so one call site sees every
        // shape in turn rather than a single value per program.
        _ => format!(
            "{{ Object[] vs = {{ {i}, {d}, {s}, {b}, new Left(), Color.GREEN, new Pt(1, 2), new ArrayList<Integer>(), new HashMap<String, Integer>(), new int[]{{1}} }}; String out = \"\"; for (Object v : vs) {{ out += (v instanceof Number) ? \"N\" : (v instanceof CharSequence) ? \"C\" : (v instanceof Base) ? \"B\" : (v instanceof Enum) ? \"E\" : (v instanceof Record) ? \"R\" : (v instanceof Collection) ? \"L\" : (v instanceof Map) ? \"M\" : (v instanceof Object) ? \"O\" : \"?\"; }} System.out.println(out); }}"
        ),
    }
}

/// `toString()` overrides reached through a receiver the compiler cannot type.
///
/// The compiler resolves an override statically whenever the operand's declared
/// type names the class, so every probe here deliberately loses that type — the
/// value goes into an `Object`, into a collection, or into a `Map` — which is
/// the only way to reach the host's runtime lookup. Each surface is asked twice
/// over, once by concatenation and once by a rendering call, because those are
/// two different code paths (fusevm's `Op::Add` versus a builtin) and the
/// failure this mode exists to catch is them disagreeing for the same object.
///
/// `Bare` is in the pool as the negative case: it declares no override, so a
/// lookup that answered *some* body for every class would print `B`/`L`/`R`
/// where Java prints `Bare@<hash>` — and the probes keep that one to the
/// `endsWith`/`startsWith` shape, since the hash is not reproducible.
fn g_render(r: &mut Rng) -> String {
    let v = pick(
        r,
        &[
            "new Base()",
            "new Left()",
            "new Right()",
            "new SupC()",
            "new Pt(1, 2)",
            "new Tag(\"t\", 1.5)",
            "Color.RED",
            "Ops.TIMES",
        ],
    );
    match r.below(10) {
        // An `Object`-typed local: concatenation, `println`, `String.valueOf`,
        // `%s`, and an explicit `toString()` must all answer the same text.
        0 => format!(
            "{{ Object o = {v}; System.out.println(\"\" + o); System.out.println(o); System.out.println(String.valueOf(o)); System.out.println(String.format(\"%s\", o)); System.out.println(o.toString()); }}"
        ),
        // An element of a list, which renders through the collection's own
        // `toString` — the element's static type is erased by `get`.
        1 => format!(
            "{{ List<Object> l = new ArrayList<>(); l.add({v}); System.out.println(l); System.out.println(\"\" + l); System.out.println(l.toString()); System.out.println(l.get(0).toString()); }}"
        ),
        // Depth: a list inside a list, and a list inside a map value.
        2 => format!(
            "{{ List<Object> in = new ArrayList<>(); in.add({v}); List<Object> out = new ArrayList<>(); out.add(in); System.out.println(out); System.out.println(\"\" + out); }}"
        ),
        3 => format!(
            "{{ Map<String, Object> m = new LinkedHashMap<>(); m.put(\"k\", {v}); System.out.println(m); System.out.println(\"\" + m); }}"
        ),
        // A `Set` element and a map *key*, which render on the other side of the
        // `=` from a value.
        4 => format!(
            "{{ Set<Object> s = new LinkedHashSet<>(); s.add({v}); System.out.println(s); System.out.println(\"\" + s); }}"
        ),
        5 => format!(
            "{{ Map<Object, String> m = new LinkedHashMap<>(); m.put({v}, \"x\"); System.out.println(m); System.out.println(\"\" + m); }}"
        ),
        // A reference array, through both `Arrays` renderings.
        6 => format!(
            "{{ Object[] a = {{ {v}, {v} }}; System.out.println(Arrays.toString(a)); System.out.println(Arrays.deepToString(new Object[][] {{ a }})); System.out.println(\"\" + a[0]); }}"
        ),
        // The `Formatter` conversions that call `toString`, including the
        // uppercasing one and the receiver-as-format-string spelling.
        7 => format!(
            "{{ Object o = {v}; System.out.println(String.format(\"%s|%S|%10s|%.1s\", o, o, o, o)); System.out.println(\"%s\".formatted(o)); }}"
        ),
        // The negative case: a class with no override still renders Java's
        // default form, and the two surfaces still agree about it.
        8 => "{ Object o = new Bare(1); String a = \"\" + o; String b = String.valueOf(o); System.out.println(a.equals(b)); System.out.println(a.startsWith(\"Bare@\")); }".to_string(),
        // A `null` reference is the text \"null\" at every surface, never a call.
        _ => "{ Object o = null; System.out.println(\"\" + o); System.out.println(String.valueOf(o)); System.out.println(String.format(\"%s\", o)); }".to_string(),
    }
}

/// A collection's membership tests, over elements whose `equals` is their own.
///
/// Every other collection mode draws its elements from `Integer` and `String`,
/// which javars models as scalars and compares structurally by value — so no
/// existing probe can reach the comparison a collection performs on a *heap*
/// element, and the whole class of divergence where `contains` answers identity
/// instead of `equals` was invisible to the harness.
///
/// The pool is three shapes, because Java's answer differs across them:
///
///   * `Eqh` and `Pt`/`Tag` (a `record`) declare `equals` *and* a consistent
///     hash, so every container finds an equal element.
///   * `Eqn` declares `equals` and no `hashCode`, so it keeps `Object`'s
///     identity hash: an `ArrayList` still finds it (no hashing), and a
///     `HashSet`/`HashMap` does not — two equal instances land in different
///     buckets and never meet. A membership test that consulted `equals`
///     unconditionally would report `true` where Java reports `false`.
///   * `Eqx` declares neither, so identity is the whole answer everywhere.
///
/// `Eqc` counts its own invocations, which pins the *number* of comparisons a
/// list scan performs: `indexOf` stops at the first hit, and an implementation
/// that resolved every element's verdict up front would run the body too many
/// times. Only list probes read the counter — a hash container's comparison
/// count follows bucket layout, which javars does not model.
///
/// Nothing here prints a `HashSet`/`HashMap` holding a user object: their
/// iteration order follows `hashCode`, and javars's identity hash is its heap
/// handle (see BUGS.md). Order-bearing output uses the insertion-ordered
/// `Linked*` forms; the hash forms are asked only for booleans, sizes and
/// looked-up values.
fn g_equals(r: &mut Rng) -> String {
    // (constructor for the element, a second equal instance, an unequal one)
    let (a, b, c) = *pick(
        r,
        &[
            ("new Eqh(1)", "new Eqh(1)", "new Eqh(2)"),
            ("new Eqn(1)", "new Eqn(1)", "new Eqn(2)"),
            ("new Pt(1, 2)", "new Pt(1, 2)", "new Pt(3, 4)"),
            (
                "new Tag(\"t\", 1.5)",
                "new Tag(\"t\", 1.5)",
                "new Tag(\"u\", 2.5)",
            ),
            ("new Eqx(1)", "new Eqx(1)", "new Eqx(2)"),
        ],
    );
    match r.below(14) {
        // `List` never hashes, so `equals` alone decides every one of these.
        0 => format!(
            "{{ List<Object> l = new ArrayList<>(); l.add({a}); l.add({c}); System.out.println(l.contains({b}) + \" \" + l.indexOf({b}) + \" \" + l.lastIndexOf({b}) + \" \" + l.contains({c})); }}"
        ),
        1 => format!(
            "{{ List<Object> l = new ArrayList<>(); l.add({a}); l.add({b}); System.out.println(l.indexOf({b}) + \" \" + l.lastIndexOf({b}) + \" \" + l.size()); }}"
        ),
        2 => format!(
            "{{ List<Object> l = new ArrayList<>(); l.add({a}); l.add({c}); Object q = {b}; System.out.println(l.remove(q) + \" \" + l.size() + \" \" + l.contains(q)); }}"
        ),
        3 => format!(
            "{{ List<Object> x = new ArrayList<>(); x.add({a}); List<Object> y = new ArrayList<>(); y.add({b}); List<Object> z = new ArrayList<>(); z.add({c}); System.out.println(x.equals(y) + \" \" + x.equals(z) + \" \" + x.equals(new ArrayList<>())); }}"
        ),
        4 => format!(
            "{{ List<Object> l = new ArrayList<>(); l.add({c}); l.add({a}); l.add({c}); System.out.println(l.subList(1, 3).contains({b}) + \" \" + l.subList(0, 2).indexOf({b})); }}"
        ),
        // The hash containers: `Eqn` is the one that must *not* be found.
        5 => format!(
            "{{ Set<Object> s = new HashSet<>(); System.out.println(s.add({a}) + \" \" + s.add({b}) + \" \" + s.size() + \" \" + s.contains({b})); }}"
        ),
        6 => format!(
            "{{ Set<Object> s = new HashSet<>(); s.add({a}); System.out.println(s.remove({b}) + \" \" + s.size() + \" \" + s.remove({c})); }}"
        ),
        7 => format!(
            "{{ Map<Object, Integer> m = new HashMap<>(); m.put({a}, 1); System.out.println(m.get({b}) + \" \" + m.containsKey({b}) + \" \" + m.size() + \" \" + m.getOrDefault({c}, -1)); }}"
        ),
        8 => format!(
            "{{ Map<Object, Integer> m = new HashMap<>(); m.put({a}, 1); m.put({b}, 2); System.out.println(m.size() + \" \" + m.get({a}) + \" \" + m.containsValue(2) + \" \" + m.remove({b})); }}"
        ),
        9 => format!(
            "{{ Map<Object, Integer> m = new HashMap<>(); m.put({a}, 1); System.out.println(m.putIfAbsent({b}, 9) + \" \" + m.size() + \" \" + m.putIfAbsent({c}, 9) + \" \" + m.size()); }}"
        ),
        // Insertion-ordered forms, where the contents can be printed.
        10 => format!(
            "{{ Set<Object> s = new LinkedHashSet<>(); s.add({a}); s.add({b}); s.add({c}); System.out.println(s.size() + \" \" + s); }}"
        ),
        11 => format!(
            "{{ Set<Object> s = new LinkedHashSet<>(); s.addAll(new ArrayList<>(Arrays.asList({a}, {b}, {c}))); System.out.println(s.size() + \" \" + s); }}"
        ),
        // Building a set from a sequence de-duplicates by the same rule.
        12 => format!(
            "{{ System.out.println(new HashSet<>(Arrays.asList({a}, {b}, {c})).size()); }}"
        ),
        // The comparison *count* a list scan performs, which only a
        // short-circuiting scan gets right.
        _ => {
            "{ Eqc.calls = 0; List<Object> l = new ArrayList<>(); l.add(new Eqc(1)); l.add(new Eqc(2)); l.add(new Eqc(3)); boolean h = l.contains(new Eqc(1)); int n = Eqc.calls; Eqc.calls = 0; int i = l.indexOf(new Eqc(3)); System.out.println(h + \" \" + n + \" \" + i + \" \" + Eqc.calls); }"
                .to_string()
        }
    }
}

const SUPPORT: &str = concat!(
    "    static int fin1() { try { return 1; } finally { System.out.println(\"f1\"); } }\n",
    "    static int fin2() { int x = 5; try { return x; } finally { x = 99; } }\n",
    "    static int fin3() { try { return 6; } finally { return 66; } }\n",
    "    static int resret() { try (Res a = new Res(\"r\")) { return 7; } }\n",
    "    static String shout(Color c) { return c.name() + \"!\"; }\n",
    "    static Color next(Color c) { return c == Color.BLUE ? Color.RED : Color.values()[c.ordinal() + 1]; }\n",
    "    static int useCalc(Calc c, int a, int b) { return c.of(a, b); }\n",
    "    static Calc mkAdder(int n) { return (x, y) -> x + y + n; }\n",
    // Variable-arity declarations for the `varargs` mode. They come in
    // competing sets on purpose: `vpick` pairs a fixed-arity overload with a
    // variadic one (the fixed one must win at its own arity and only there),
    // and `vkind` pairs two variadic ones whose element types decide which is
    // more specific. A single variadic declaration would be satisfied by a
    // resolver that ignores the phase ordering entirely.
    "    static int vsum(int... xs) { int t = 0; for (int v : xs) t += v; return t; }\n",
    "    static String vpick(int a, int b) { return \"fixed2\"; }\n",
    "    static String vpick(int... xs) { return \"var\" + xs.length; }\n",
    "    static String vkind(String... xs) { return \"str\" + xs.length; }\n",
    "    static String vkind(Object... xs) { return \"obj\" + xs.length; }\n",
    "    static String vtag(String tag, int... xs) { return tag + \":\" + xs.length + \":\" + Arrays.toString(xs); }\n",
    "    static String vnull(Object... xs) { return xs == null ? \"nullarray\" : \"len\" + xs.length; }\n",
);

/// The types the probes construct: the `AutoCloseable` the resource probes open
/// and close, the enums (bare, stateful, and body-carrying), the class holding
/// the `static` fields, and the records.
const SUPPORT_CLASS: &str = concat!(
    "enum Color { RED, GREEN, BLUE }\n",
    "enum Op {\n",
    "    ADD, SUB, MUL;\n",
    "    int apply(int a, int b) { switch (this) { case ADD: return a + b; case SUB: return a - b; default: return a * b; } }\n",
    "    boolean isMul() { return this == MUL; }\n",
    "}\n",
    // An enum whose constants carry constructor arguments (per-constant state).
    "enum Planet {\n",
    "    MERCURY(3.3), EARTH(5.97), JUPITER(1898.0);\n",
    "    private final double mass;\n",
    "    Planet(double m) { this.mass = m; }\n",
    "    double mass() { return mass; }\n",
    "    boolean heavy() { return mass > 100.0; }\n",
    "}\n",
    // An enum whose constants carry bodies — anonymous subclasses overriding an
    // abstract method (and, for one of them, a concrete one).
    "enum Ops {\n",
    "    PLUS { int apply(int a, int b) { return a + b; } },\n",
    "    MINUS { int apply(int a, int b) { return a - b; } },\n",
    "    TIMES { int apply(int a, int b) { return a * b; } String label() { return \"x\"; } };\n",
    "    abstract int apply(int a, int b);\n",
    "    String label() { return name().toLowerCase(); }\n",
    "}\n",
    "class St {\n",
    "    static int n = 1;\n",
    "    static final String LABEL = \"L\";\n",
    "    static int SIZE = 2 + 3;\n",
    "    static int INIT;\n",
    "    static int[] arr = {7, 8};\n",
    "    static { INIT = SIZE * 2; }\n",
    "    static int get() { return n; }\n",
    "    static void bump() { n = n * 2; }\n",
    "}\n",
    "class Sub2 extends St { }\n",
    "record Pt(int x, int y) { int sum() { return x + y; } }\n",
    "record Tag(String tag, double weight) { }\n",
    "record Ord(int lo, int hi) {\n",
    "    Ord { if (lo > hi) { throw new IllegalArgumentException(lo + \">\" + hi); } }\n",
    "}\n",
    // The functional interfaces the lambda probes target. A user-declared
    // single-abstract-method interface is exactly what the JDK's own
    // `java.util.function` types are, so these exercise the same path.
    // A hierarchy for the reference-cast probes: a base, two siblings, and an
    // interface only one of them implements, so a downcast can succeed or throw.
    // A class that declares neither `equals` nor `toString`, so both come from
    // `Object`: reference identity and `Bare@<hash>`.
    "class Bare { int v; Bare(int v) { this.v = v; } }\n",
    "class Base { public String toString() { return \"B\"; } }\n",
    "class Left extends Base { public String toString() { return \"L\"; } }\n",
    "class Right extends Base implements Marker { public String toString() { return \"R\"; } }\n",
    "interface Marker { }\n",
    "interface Calc { int of(int a, int b); }\n",
    "interface Str1 { String of(String s); }\n",
    "interface Pred1 { boolean of(int a); }\n",
    "interface Sup0 { int of(); }\n",
    "class Res implements AutoCloseable {\n",
    "    String n;\n",
    "    Res(String n) { this.n = n; System.out.println(\"open \" + n); }\n",
    "    public void close() { System.out.println(\"close \" + n); }\n",
    "}\n",
    // A variable-arity *constructor* and a variable-arity *instance* method —
    // the two call sites besides a `static` that Java packs arguments at, and
    // the reason a statics-only implementation would make the same declaration
    // callable or not depending on where it sits.
    "class Bag {\n",
    "    int n;\n",
    "    Bag(int... xs) { for (int v : xs) n += v; }\n",
    "    int total() { return n; }\n",
    "    int plus(int... xs) { int t = n; for (int v : xs) t += v; return t; }\n",
    "}\n",
    // Two *unrelated* classes declaring statics of the same name — `Qa` and `Qb`
    // both spell `f`, `g` and `h`. A qualified `Qb.f(1)` must reach `Qb`'s body
    // and nothing else, which is only observable when a same-named static exists
    // somewhere else in the compilation unit. `g`'s two overloads differ in
    // parameter type, so a by-name-only resolver picks the *signature* that fits
    // best and silently crosses classes.
    "class Qa {\n",
    "    static int f(int x) { return x + 1; }\n",
    "    static String g(String s) { return \"a:\" + s; }\n",
    "    static int h() { return 1; }\n",
    "}\n",
    "class Qb {\n",
    "    static int f(int x) { return x + 100; }\n",
    "    static String g(Object o) { return \"b:\" + o; }\n",
    "    static int h() { return 2; }\n",
    "}\n",
    // A chain, so a subclass's static *hides* the one it inherits while the
    // names it does not redeclare still resolve up the chain.
    "class Qbase {\n",
    "    static String kind() { return \"base\"; }\n",
    "    static int only() { return 7; }\n",
    "}\n",
    "class Qderiv extends Qbase {\n",
    "    static String kind() { return \"deriv\"; }\n",
    "}\n",
    // A three-level chain for the `super` mode. Every member here exists to make
    // one wrong lowering of `super.m(...)` observable:
    //   * `f` is declared at all three levels, so dispatching `super.f` on the
    //     receiver's runtime class re-enters `SupC.f` and never terminates.
    //   * `tag` is declared on `SupA` and `SupC` but NOT on `SupB`, so
    //     `super.tag()` from `SupC` has to walk past the parent to the
    //     grandparent's body.
    //   * `kind`'s body calls an unqualified `tag()`, which must still dispatch
    //     *virtually* back down to `SupC` — `super` de-virtualizes exactly one
    //     call, not the whole callee.
    //   * `f(int)`/`f(double)` are an overload pair, `vs` is variadic, and `n`
    //     is a field, so `super.` is exercised at each of the forms that resolve
    //     differently from a plain `this.`.
    // No field is re-declared down the chain: field *hiding* is a documented
    // BUGS.md simplification, and generating it would only reproduce it.
    "class SupA {\n",
    "    int n = 3;\n",
    "    String tag() { return \"A\"; }\n",
    "    String kind() { return \"k:\" + tag(); }\n",
    "    String echo(String s) { return \"A\" + s; }\n",
    "    int f(int x) { return x + 1; }\n",
    "    int f(double x) { return 100; }\n",
    "    int vs(int... xs) { int t = 0; for (int v : xs) t += v; return t; }\n",
    "    public String toString() { return \"A<\" + n + \">\"; }\n",
    "}\n",
    "class SupB extends SupA {\n",
    "    int f(int x) { return super.f(x) * 10; }\n",
    "    public String toString() { return \"B[\" + super.toString() + \"]\"; }\n",
    "}\n",
    "class SupC extends SupB {\n",
    "    String tag() { return \"C\"; }\n",
    "    String kind() { return \"C/\" + super.kind(); }\n",
    "    String echo(String s) { return \"C\" + s; }\n",
    "    int f(int x) { return super.f(x) + 2; }\n",
    "    int vs(int... xs) { return super.vs(xs) + super.vs(1, 2); }\n",
    "    public String toString() { return \"C{\" + super.toString() + \",\" + super.tag() + \"}\"; }\n",
    "    int fd() { return super.f(2.5); }\n",
    "    int bump() { super.n += 4; super.n++; return super.n + this.n; }\n",
    "    Sup0 lam(int k) { return () -> super.f(k) + 7; }\n",
    "    Str1 mref() { return super::echo; }\n",
    "}\n",
    // A class whose superclass is the implicit `java.lang.Object`, so
    // `super.equals` is Object's reference identity. Its `toString`/`hashCode`
    // are deliberately not reachable here: both render an identity hash, which
    // differs between runs of the same JVM and so has no deterministic answer.
    "class SupObj {\n",
    "    int v;\n",
    "    SupObj(int v) { this.v = v; }\n",
    "    boolean sameAs(Object o) { return super.equals(o); }\n",
    "}\n",
    // The `equals` mode's element pool. `Eqh` is the correctly written pair;
    // `Eqn` deliberately omits `hashCode`, which is what makes a hash container
    // miss an element an `ArrayList` finds; `Eqc` counts its own invocations so
    // a probe can pin how many comparisons a scan performs.
    "class Eqh {\n",
    "    int v;\n",
    "    Eqh(int v) { this.v = v; }\n",
    "    public boolean equals(Object o) { return o instanceof Eqh && ((Eqh) o).v == v; }\n",
    "    public int hashCode() { return v; }\n",
    "    public String toString() { return \"Eqh\" + v; }\n",
    "}\n",
    "class Eqn {\n",
    "    int v;\n",
    "    Eqn(int v) { this.v = v; }\n",
    "    public boolean equals(Object o) { return o instanceof Eqn && ((Eqn) o).v == v; }\n",
    "    public String toString() { return \"Eqn\" + v; }\n",
    "}\n",
    // The negative case: neither `equals` nor `hashCode`, so identity is the
    // whole answer in every container. It carries a `toString` because the
    // probes print set contents, and `Object`'s default renders an identity hash
    // that differs between runs of the same JVM.
    "class Eqx {\n",
    "    int v;\n",
    "    Eqx(int v) { this.v = v; }\n",
    "    public String toString() { return \"Eqx\" + v; }\n",
    "}\n",
    "class Eqc {\n",
    "    static int calls;\n",
    "    int v;\n",
    "    Eqc(int v) { this.v = v; }\n",
    "    public boolean equals(Object o) { calls++; return o instanceof Eqc && ((Eqc) o).v == v; }\n",
    "    public int hashCode() { return v; }\n",
    "    public String toString() { return \"Eqc\" + v; }\n",
    "}\n",
);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    All,
    Arith,
    IntDiv,
    DoubleArith,
    MixedDiv,
    DoubleFmt,
    Concat,
    Compare,
    Bool,
    Ternary,
    StrMethod,
    Math,
    Format,
    Integer,
    Loop,
    Switch,
    Array,
    Overflow,
    ForEach,
    Exception,
    Fault,
    Finally,
    Resource,
    Enum,
    Static,
    Record,
    Lambda,
    Collection,
    SwitchExpr,
    VarInfer,
    Bitwise,
    Shift,
    Cast,
    IncDec,
    Literal,
    Printf,
    LabelFlow,
    Char,
    Regex,
    Float,
    Decl,
    Object,
    StaticRef,
    LoopKind,
    Narrow,
    ListOp,
    Varargs,
    ListView,
    Immutable,
    Super,
    InstanceOf,
    Render,
    Equals,
}

const CONCRETE: &[Mode] = &[
    Mode::Arith,
    Mode::IntDiv,
    Mode::DoubleArith,
    Mode::MixedDiv,
    Mode::DoubleFmt,
    Mode::Concat,
    Mode::Compare,
    Mode::Bool,
    Mode::Ternary,
    Mode::StrMethod,
    Mode::Math,
    Mode::Format,
    Mode::Integer,
    Mode::Loop,
    Mode::Switch,
    Mode::Array,
    Mode::Overflow,
    Mode::ForEach,
    Mode::Exception,
    Mode::Fault,
    Mode::Finally,
    Mode::Resource,
    Mode::Enum,
    Mode::Static,
    Mode::Record,
    Mode::Lambda,
    Mode::Collection,
    Mode::SwitchExpr,
    Mode::VarInfer,
    Mode::Bitwise,
    Mode::Shift,
    Mode::Cast,
    Mode::IncDec,
    Mode::Literal,
    Mode::Printf,
    Mode::LabelFlow,
    Mode::Char,
    Mode::Regex,
    Mode::Float,
    Mode::Decl,
    Mode::Object,
    Mode::StaticRef,
    Mode::LoopKind,
    Mode::Narrow,
    Mode::ListOp,
    Mode::Varargs,
    Mode::ListView,
    Mode::Immutable,
    Mode::Super,
    Mode::InstanceOf,
    Mode::Render,
    Mode::Equals,
];

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::All => "all",
        Mode::Arith => "arith",
        Mode::IntDiv => "intdiv",
        Mode::DoubleArith => "doublearith",
        Mode::MixedDiv => "mixeddiv",
        Mode::DoubleFmt => "doublefmt",
        Mode::Concat => "concat",
        Mode::Compare => "compare",
        Mode::Bool => "bool",
        Mode::Ternary => "ternary",
        Mode::StrMethod => "strmethod",
        Mode::Math => "math",
        Mode::Format => "format",
        Mode::Integer => "integer",
        Mode::Loop => "loop",
        Mode::Switch => "switch",
        Mode::Array => "array",
        Mode::Overflow => "overflow",
        Mode::ForEach => "foreach",
        Mode::Exception => "exception",
        Mode::Fault => "fault",
        Mode::Finally => "finally",
        Mode::Resource => "resource",
        Mode::Enum => "enum",
        Mode::Static => "static",
        Mode::Record => "record",
        Mode::Lambda => "lambda",
        Mode::Collection => "collection",
        Mode::SwitchExpr => "switchexpr",
        Mode::VarInfer => "varinfer",
        Mode::Bitwise => "bitwise",
        Mode::Shift => "shift",
        Mode::Cast => "cast",
        Mode::IncDec => "incdec",
        Mode::Literal => "literal",
        Mode::Printf => "printf",
        Mode::LabelFlow => "labelflow",
        Mode::Char => "char",
        Mode::Regex => "regex",
        Mode::Float => "float",
        Mode::Decl => "decl",
        Mode::Object => "object",
        Mode::StaticRef => "staticref",
        Mode::LoopKind => "loopkind",
        Mode::Narrow => "narrow",
        Mode::ListOp => "listop",
        Mode::Varargs => "varargs",
        Mode::ListView => "listview",
        Mode::Immutable => "immutable",
        Mode::Super => "super",
        Mode::InstanceOf => "instanceof",
        Mode::Render => "render",
        Mode::Equals => "equals",
    }
}

fn parse_mode(s: &str) -> Option<Mode> {
    if s == "all" {
        return Some(Mode::All);
    }
    CONCRETE.iter().copied().find(|m| mode_name(*m) == s)
}

fn gen_probe(r: &mut Rng, mode: Mode) -> String {
    let m = if mode == Mode::All {
        *pick(r, CONCRETE)
    } else {
        mode
    };
    match m {
        Mode::Arith => g_arith(r),
        Mode::IntDiv => g_intdiv(r),
        Mode::DoubleArith => g_doublearith(r),
        Mode::MixedDiv => g_mixeddiv(r),
        Mode::DoubleFmt => g_doublefmt(r),
        Mode::Concat => g_concat(r),
        Mode::Compare => g_compare(r),
        Mode::Bool => g_bool(r),
        Mode::Ternary => g_ternary(r),
        Mode::StrMethod => g_strmethod(r),
        Mode::Math => g_math(r),
        Mode::Format => g_format(r),
        Mode::Integer => g_integer(r),
        Mode::Loop => g_loop(r),
        Mode::Switch => g_switch(r),
        Mode::Array => g_array(r),
        Mode::Overflow => g_overflow(r),
        Mode::ForEach => g_foreach(r),
        Mode::Exception => g_exception(r),
        Mode::Fault => g_fault(r),
        Mode::Finally => g_finally(r),
        Mode::Resource => g_resource(r),
        Mode::Enum => g_enum(r),
        Mode::Static => g_static(r),
        Mode::Record => g_record(r),
        Mode::Lambda => g_lambda(r),
        Mode::Collection => g_collection(r),
        Mode::SwitchExpr => g_switchexpr(r),
        Mode::VarInfer => g_varinfer(r),
        Mode::Bitwise => g_bitwise(r),
        Mode::Shift => g_shift(r),
        Mode::Cast => g_cast(r),
        Mode::IncDec => g_incdec(r),
        Mode::Literal => g_literal(r),
        Mode::Printf => g_printf(r),
        Mode::LabelFlow => g_labelflow(r),
        Mode::Char => g_char(r),
        Mode::Regex => g_regex(r),
        Mode::Float => g_float(r),
        Mode::Decl => g_decl(r),
        Mode::Object => g_object(r),
        Mode::StaticRef => g_staticref(r),
        Mode::LoopKind => g_loopkind(r),
        Mode::Narrow => g_narrow(r),
        Mode::ListOp => g_listop(r),
        Mode::Varargs => g_varargs(r),
        Mode::ListView => g_listview(r),
        Mode::Immutable => g_immutable(r),
        Mode::Super => g_super(r),
        Mode::InstanceOf => g_instanceof(r),
        Mode::Render => g_render(r),
        Mode::Equals => g_equals(r),
        Mode::All => unreachable!("resolved above"),
    }
}

fn gen_probes(seed: u64, mode: Mode, n: usize) -> Vec<String> {
    let mut r = Rng::new(seed);
    (0..n).map(|_| gen_probe(&mut r, mode)).collect()
}

/// Wrap probes in the single-class shell both sides accept. The class must be
/// named for the file, so callers write it to `T.java`.
fn build_program(probes: &[String]) -> String {
    let mut s = String::from(
        "import java.util.*;\npublic class T {\n    public static void main(String[] args) {\n",
    );
    for probe in probes {
        s.push_str("        ");
        s.push_str(probe);
        s.push('\n');
    }
    s.push_str("    }\n");
    s.push_str(SUPPORT);
    s.push_str("}\n");
    s.push_str(SUPPORT_CLASS);
    s
}

struct RunOut {
    stdout: Vec<u8>,
    ok: bool,
}

static TMP_CTR: AtomicU64 = AtomicU64::new(0);

/// Environment variables that change what a `java` launcher does, cleared from
/// every JVM this harness spawns -- the probes AND the measured runs alike.
///
/// `JAVA_HOME` is not read by a real `java` binary, but the first `java` on a
/// developer `PATH` is routinely a version-manager shim that *is* a script and
/// does read it. On this machine it names a JDK 17, whose `Double.toString`
/// predates JDK 19's shortest-round-trip rewrite, so an inherited `JAVA_HOME`
/// silently selects a JVM that answers `1.0e23` with `9.999999999999999E22`.
/// [`require_modern_doubles`] catches that, but only because the probe runs
/// under the same environment as the measured programs; clearing the variable
/// removes the divergence at the source instead of detecting it after the fact.
///
/// The three `*OPTIONS` variables are read by the launcher itself and prepend
/// arbitrary flags to every invocation. `JDK_JAVA_OPTIONS` is the dangerous one
/// for a differential harness: it is honoured only by the *source-file*
/// launcher -- exactly the entry point this harness uses and the frozen corpus
/// replays -- so an ambient value alters the reference and nothing else, and
/// javars, which reads no environment variable at all, cannot follow it.
const JVM_ENV_TO_CLEAR: &[&str] = &[
    "JAVA_HOME",
    "JDK_JAVA_OPTIONS",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
];

/// A `Command` for one side of the comparison, with [`JVM_ENV_TO_CLEAR`] removed.
///
/// Both sides are sanitized, not just the oracle: javars reads no environment
/// variable, so clearing them costs nothing there and keeps the two processes
/// running under one environment rather than two that differ in ways the report
/// would not name.
fn jvm_command(prog: &Path) -> Command {
    let mut c = Command::new(prog);
    for k in JVM_ENV_TO_CLEAR {
        c.env_remove(k);
    }
    c
}

/// The options the reference `java` is launched with, and javars is not.
///
/// An empty `user.language`/`user.country` is `Locale.ROOT` — the locale javars
/// formats in unconditionally, because it has no locale model and accepts no
/// `-D` to give it one (see BUGS.md). Without the pin the oracle formats in
/// whatever locale the *machine* is set to, so `%,d` and `%.2f` would be
/// compared against `1.234.567` / `3,50` on a German desktop and every format
/// probe would report a divergence that says nothing about javars. This does not
/// hide the locale gap: a javars program cannot select a locale at all, so there
/// is no javars behaviour on the other side of the pin to measure.
const ORACLE_OPTS: &[&str] = &["-Duser.language=", "-Duser.country="];

/// javars takes no options before the source file.
const OURS_OPTS: &[&str] = &[];

/// Run one program through `prog`. The file must be named `T.java` for the JDK's
/// single-file source launcher, so each run gets its own directory.
fn run_prog(prog: &Path, opts: &[&str], src: &str, timeout: Duration) -> RunOut {
    let n = TMP_CTR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("javars_parity_{}_{n}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("T.java");
    if std::fs::write(&path, src).is_err() {
        return RunOut {
            stdout: Vec::new(),
            ok: false,
        };
    }

    let mut child = match jvm_command(prog)
        .args(opts)
        .arg(&path)
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("parity-fuzz: cannot spawn {}: {e}", prog.display());
            std::process::exit(2);
        }
    };

    // Poll for the timeout rather than blocking forever on a hung frontend.
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break None,
        }
    };

    let out = child.wait_with_output().ok();
    let _ = std::fs::remove_dir_all(&dir);
    match (status, out) {
        (Some(st), Some(o)) => RunOut {
            stdout: o.stdout,
            ok: st.success(),
        },
        (_, Some(o)) => RunOut {
            stdout: o.stdout,
            ok: false,
        },
        _ => RunOut {
            stdout: Vec::new(),
            ok: false,
        },
    }
}

fn differs(oracle: &RunOut, ours: &RunOut) -> bool {
    oracle.stdout != ours.stdout || oracle.ok != ours.ok
}

fn render(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\n', "\\n")
}

fn diverges(probes: &[String], ours: &Path, oracle: &Path, timeout: Duration) -> bool {
    let src = build_program(probes);
    let a = run_prog(oracle, ORACLE_OPTS, &src, timeout);
    let b = run_prog(ours, OURS_OPTS, &src, timeout);
    differs(&a, &b)
}

/// The literal spans of a probe, as `(start, end)` byte ranges of its source.
///
/// A run of digits that is not glued to an identifier, and the inside of a
/// double-quoted string. Both are found by a single left-to-right walk that
/// tracks whether it is inside a string or a character literal, so a `"5"` in a
/// message and an escaped quote do not read as code.
fn literal_spans(probe: &str) -> Vec<(usize, usize)> {
    let b = probe.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i + 1;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                if i <= b.len() {
                    spans.push((start, i.min(b.len())));
                }
                i += 1;
            }
            // A character literal is skipped whole: its content is one code
            // point and replacing it with `0` or `""` never compiles.
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            c if c.is_ascii_digit() => {
                let start = i;
                // Glued to an identifier on the left (`arg0`, `List12`) it is
                // part of a name, not a number.
                let name =
                    start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_');
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                    i += 1;
                }
                // A suffix (`5L`, `1.0f`) or a trailing identifier character
                // makes the span something other than a bare decimal.
                let suffixed = i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'_');
                if !name && !suffixed {
                    spans.push((start, i));
                }
            }
            _ => i += 1,
        }
    }
    spans
}

/// Reduce one probe's *literals* while it keeps diverging.
///
/// [`minimize`] deletes whole probes, which is the only reduction a packed
/// program allows — but it stops at one probe, and that probe still carries the
/// operands the generator happened to draw. A report that reads
/// `List.of(47, 12, 99).get(-2)` says the same thing as `List.of(1, 1, 1).get(0)`
/// and takes longer to read: the reader has to establish for himself that 47 and
/// 99 are not load-bearing before he can see that the *arity* is. So each
/// literal is tried against a smaller stand-in and kept only when the divergence
/// survives, which turns the surviving operands into evidence — a `3` that
/// resists shrinking to `0` and `1` is a `3` the divergence depends on.
///
/// Every candidate is checked against the same predicate the search uses, and
/// the oracle's own success is part of it: a reduction that stops the reference
/// from compiling makes both sides fail, and while that is not a *divergence* it
/// would be one the moment javars accepted what javac refused. Requiring the
/// oracle to still run keeps a syntactically broken reduction from being
/// mistaken for a smaller reproducer.
fn shrink_literals(probe: &str, ours: &Path, oracle: &Path, timeout: Duration) -> String {
    let oracle_ran = run_prog(
        oracle,
        ORACLE_OPTS,
        &build_program(&[probe.to_string()]),
        timeout,
    )
    .ok;
    let still_diverges = |cand: &str| -> bool {
        let src = build_program(&[cand.to_string()]);
        let a = run_prog(oracle, ORACLE_OPTS, &src, timeout);
        if oracle_ran && !a.ok {
            return false;
        }
        differs(&a, &run_prog(ours, OURS_OPTS, &src, timeout))
    };
    let mut cur = probe.to_string();
    // Right to left, so an accepted replacement cannot shift the spans still to
    // be tried.
    let mut spans = literal_spans(&cur);
    spans.reverse();
    for (start, end) in spans {
        if end > cur.len() || start > end {
            continue;
        }
        let original = cur[start..end].to_string();
        for smaller in ["", "0", "1"] {
            if smaller == original {
                break;
            }
            let mut cand = cur.clone();
            cand.replace_range(start..end, smaller);
            if still_diverges(&cand) {
                cur = cand;
                break;
            }
        }
    }
    cur
}

/// Bisect a diverging probe list down to a minimal still-diverging subset.
fn minimize(probes: &[String], ours: &Path, oracle: &Path, timeout: Duration) -> Vec<String> {
    let mut cur = probes.to_vec();
    let mut chunk = cur.len() / 2;
    while chunk >= 1 {
        let mut i = 0;
        while i < cur.len() {
            let mut trial = cur.clone();
            let end = (i + chunk).min(trial.len());
            trial.drain(i..end);
            if !trial.is_empty() && diverges(&trial, ours, oracle, timeout) {
                cur = trial;
            } else {
                i += chunk;
            }
        }
        if chunk == 1 {
            break;
        }
        chunk /= 2;
    }
    cur
}

fn ours_bin() -> PathBuf {
    if let Ok(p) = std::env::var("JAVARS_BIN") {
        return PathBuf::from(p);
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for profile in ["debug", "release"] {
        let c = root.join("target").join(profile).join("java");
        if c.exists() {
            return c;
        }
    }
    root.join("target/debug/java")
}

/// Locate a real JDK `java` that is not our own binary.
///
/// Every candidate is *run* before it is accepted, twice over, because two
/// different wrong oracles live on a developer machine and neither announces
/// itself:
///
///   * A `java` on `PATH` is routinely a version-manager shim rather than a
///     launcher, and a broken shim exits non-zero without printing a banner —
///     which this harness would read as "the reference produced no output",
///     reporting a divergence on every single probe. Naming the executable is
///     not evidence that it launches a JVM; [`jdk_banner`] getting a version
///     out of it is.
///   * A launcher that *does* work may still be too old to be the reference.
///     `Double.toString` was reimplemented in JDK 19 to emit the shortest
///     round-tripping decimal, so a JDK 17 oracle answers `1.0e23` with
///     `9.999999999999999E22` where 19+ answers `1.0E23`. That is a *silent*
///     corruptor: the harness runs green and reports divergences on every
///     double-valued probe, or worse, blesses the old rendering into the frozen
///     corpus. [`renders_modern_doubles`] is the probe that catches it.
fn resolve_oracle(ours: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("JAVA_ORACLE") {
        let p = PathBuf::from(p);
        match jdk_banner(&p) {
            Some(v) => announce_oracle(&p, &v),
            None => {
                eprintln!(
                    "parity-fuzz: JAVA_ORACLE={} does not run — `--version` failed",
                    p.display()
                );
                std::process::exit(2);
            }
        }
        return p;
    }
    let ours_canon = ours.canonicalize().ok();
    let mut rejected = Vec::new();
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let cand = Path::new(dir).join("java");
            if !cand.exists() || cand.canonicalize().ok() == ours_canon {
                continue;
            }
            match jdk_banner(&cand) {
                Some(v) => {
                    announce_oracle(&cand, &v);
                    return cand;
                }
                None => rejected.push(cand.display().to_string()),
            }
        }
    }
    eprintln!("parity-fuzz: no working reference `java` on PATH (set JAVA_ORACLE=/path/to/java)");
    for r in &rejected {
        eprintln!("parity-fuzz:   rejected {r} — `--version` failed");
    }
    std::process::exit(2);
}

/// Accept a candidate as the reference, after measuring it, and say so.
///
/// The report is unconditional rather than reserved for failures. Which JVM
/// answered is the single fact that decides what every double-valued
/// comparison in the run means, and a harness that prints it only when it
/// rejects one leaves a *passing* sweep with no record of what it passed
/// against — the log then cannot distinguish a clean run against JDK 21 from a
/// clean run against a JVM whose renderings differ. The ambient `JAVA_HOME` is
/// named for the same reason: it is the variable that misdirects a shim, and
/// [`JVM_ENV_TO_CLEAR`] strips it, so the line records both what was present
/// and that it did not reach the JVM.
fn announce_oracle(prog: &Path, banner: &str) {
    require_modern_doubles(prog, banner);
    require_root_locale(prog, banner);
    eprintln!("parity-fuzz: oracle {} ({banner})", prog.display());
    eprintln!(
        "parity-fuzz: oracle verified — 1.0e23 renders as 1.0E23 (JDK 19+ Double.toString), \
         String.format is the root locale"
    );
    eprintln!(
        "parity-fuzz: ambient JAVA_HOME={} (cleared from every spawn)",
        std::env::var("JAVA_HOME").unwrap_or_else(|_| "<unset>".into())
    );
}

/// The first line `prog --version` prints, if it exits 0 and prints one.
/// `None` for anything that is not a working launcher.
fn jdk_banner(prog: &Path) -> Option<String> {
    let out = jvm_command(prog).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// What the candidate answers for `1.0e23`. JDK 19 replaced `Double.toString`
/// with the shortest round-tripping decimal (`1.0E23`); every release before it
/// answers `9.999999999999999E22`.
fn renders_modern_doubles(prog: &Path) -> Option<bool> {
    let dir = std::env::temp_dir().join(format!("javars_oracle_probe_{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("D.java");
    std::fs::write(
        &path,
        "public class D { public static void main(String[] a) { System.out.println(1.0e23); } }\n",
    )
    .ok()?;
    let out = jvm_command(prog).arg(&path).current_dir(&dir).output().ok();
    let _ = std::fs::remove_dir_all(&dir);
    let out = out?;
    Some(String::from_utf8_lossy(&out.stdout).trim() == "1.0E23")
}

/// Whether the candidate, launched with [`ORACLE_OPTS`], formats in the root
/// locale — the one javars formats in unconditionally.
///
/// The pin is two `-D` options, and a launcher is free to ignore an option it
/// does not recognise, so the answer is *measured* rather than assumed: a JVM
/// that kept a German default would render `1.234.567` / `3,50` and turn every
/// `format`/`printf` probe into a divergence that says nothing about javars.
fn formats_in_root_locale(prog: &Path) -> Option<bool> {
    let dir = std::env::temp_dir().join(format!("javars_locale_probe_{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("L.java");
    std::fs::write(
        &path,
        "public class L { public static void main(String[] a) { System.out.print(String.format(\"%,d|%.2f\", 1234567, 3.5)); } }\n",
    )
    .ok()?;
    let out = jvm_command(prog)
        .args(ORACLE_OPTS)
        .arg(&path)
        .current_dir(&dir)
        .output()
        .ok();
    let _ = std::fs::remove_dir_all(&dir);
    Some(String::from_utf8_lossy(&out?.stdout).trim() == "1,234,567|3.50")
}

/// Exit rather than measure `String.format` against another locale's separators.
fn require_root_locale(prog: &Path, banner: &str) {
    if formats_in_root_locale(prog) == Some(true) {
        return;
    }
    eprintln!(
        "parity-fuzz: {} ({banner}) does not honour {ORACLE_OPTS:?} — its `String.format` is not the root locale",
        prog.display()
    );
    eprintln!("parity-fuzz: every `format`/`printf` probe would report a locale difference as a javars divergence");
    std::process::exit(2);
}

/// Exit rather than measure against a pre-JDK-19 `Double.toString`.
fn require_modern_doubles(prog: &Path, banner: &str) {
    if renders_modern_doubles(prog) == Some(true) {
        return;
    }
    eprintln!(
        "parity-fuzz: {} ({banner}) renders 1.0e23 the pre-JDK-19 way — too old to be the reference",
        prog.display()
    );
    eprintln!(
        "parity-fuzz: JAVA_HOME={}",
        std::env::var("JAVA_HOME").unwrap_or_else(|_| "<unset>".into())
    );
    eprintln!("parity-fuzz: set JAVA_ORACLE to a JDK 19 or newer `java`");
    std::process::exit(2);
}

struct Args {
    iters: usize,
    probes: usize,
    seed: Option<u64>,
    once: bool,
    mode: Mode,
    timeout: Duration,
    verbose: bool,
    jobs: usize,
}

fn parse_args() -> Args {
    let mut a = Args {
        iters: 50,
        probes: 40,
        seed: None,
        once: false,
        mode: Mode::All,
        timeout: Duration::from_secs(60),
        verbose: false,
        // One seed per core by default. A sweep is not compute-bound — nearly
        // all of its wall time is two process launches per program waiting on
        // each other — so the serial loop left the machine idle: 24 programs
        // took 2:35.82 at 28% CPU on this one. The floor is 1, not 0, since
        // `--jobs 0` would otherwise spawn no worker and report a clean sweep
        // over zero programs.
        jobs: std::thread::available_parallelism().map_or(1, |n| n.get()),
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let next = |i: &mut usize| -> String {
            *i += 1;
            argv.get(*i).cloned().unwrap_or_default()
        };
        match argv[i].as_str() {
            "--iters" => a.iters = next(&mut i).parse().unwrap_or(a.iters),
            "--probes" => a.probes = next(&mut i).parse().unwrap_or(a.probes),
            "--seed" => a.seed = next(&mut i).parse().ok(),
            "--once" => a.once = true,
            "--verbose" | "-v" => a.verbose = true,
            "--timeout" => a.timeout = Duration::from_secs(next(&mut i).parse().unwrap_or(60)),
            "--jobs" | "-j" => a.jobs = next(&mut i).parse().unwrap_or(a.jobs).max(1),
            "--mode" => {
                let m = next(&mut i);
                match parse_mode(&m) {
                    Some(m) => a.mode = m,
                    None => {
                        eprintln!("parity-fuzz: unknown mode `{m}`");
                        std::process::exit(2);
                    }
                }
            }
            "--help" | "-h" => {
                println!("parity-fuzz [--iters N] [--probes N] [--seed N] [--once] [--mode M] [--timeout SECS] [--jobs N] [-v]");
                println!(
                    "modes: all {}",
                    CONCRETE
                        .iter()
                        .map(|m| mode_name(*m))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("parity-fuzz: unknown option `{other}`");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    a
}

fn main() {
    let args = parse_args();
    let ours = ours_bin();
    if !ours.exists() {
        eprintln!("parity-fuzz: {} not built (cargo build)", ours.display());
        std::process::exit(2);
    }
    let oracle = resolve_oracle(&ours);
    eprintln!(
        "parity-fuzz: oracle={} ours={} mode={} probes={}",
        oracle.display(),
        ours.display(),
        mode_name(args.mode),
        args.probes
    );

    let iters = if args.once { 1 } else { args.iters };
    let base = args.seed.unwrap_or(0x5EED);

    // One program per worker at a time, handed out from a shared cursor rather
    // than sliced up front: a program that diverges pays for minimization and
    // literal shrinking on top of its two launches, so a fixed split would
    // leave every other worker idle behind the one that found something.
    let next_index = AtomicUsize::new(0);
    let failures = AtomicUsize::new(0);
    let probes_run = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);
    // Divergence reports are several lines each and interleaving two of them
    // would make both unreadable, so a report is assembled whole and printed
    // under the lock.
    let report = Mutex::new(());
    let jobs = args.jobs.min(iters.max(1));
    eprintln!("parity-fuzz: {jobs} job(s)");

    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| loop {
                let k = next_index.fetch_add(1, Ordering::Relaxed);
                if k >= iters {
                    return;
                }
                let seed = if args.once {
                    base
                } else {
                    base.wrapping_add(k as u64)
                };
                let probes = gen_probes(seed, args.mode, args.probes);
                let src = build_program(&probes);
                let a = run_prog(&oracle, ORACLE_OPTS, &src, args.timeout);
                // A program the reference toolchain itself did not run is no
                // evidence about javars. Comparing anyway would let a probe the
                // JDK rejects be counted as agreement (both sides "fail"), which
                // silently inflates a clean sweep — so it is skipped and
                // reported under its own count.
                if !a.ok || a.stdout.is_empty() {
                    skipped.fetch_add(1, Ordering::Relaxed);
                    let _g = report.lock().unwrap_or_else(|e| e.into_inner());
                    eprintln!("seed {seed}: SKIPPED — reference `java` did not produce output");
                    // A skip is a claim about the *generator*, not about javars,
                    // so the program has to be inspectable — otherwise a mode
                    // that emits Java the JDK rejects would quietly shrink the
                    // verified count instead of being fixed.
                    if args.verbose {
                        eprintln!("{src}");
                    }
                    continue;
                }
                probes_run.fetch_add(probes.len(), Ordering::Relaxed);
                let b = run_prog(&ours, OURS_OPTS, &src, args.timeout);
                if !differs(&a, &b) {
                    if args.verbose {
                        let _g = report.lock().unwrap_or_else(|e| e.into_inner());
                        eprintln!("seed {seed}: ok ({} probes)", probes.len());
                    }
                    continue;
                }
                failures.fetch_add(1, Ordering::Relaxed);
                let mut minimal = minimize(&probes, &ours, &oracle, args.timeout);
                // Literal reduction only pays once the probe list is down to
                // one: with several probes left the divergence could move
                // between them and each candidate costs two JDK launches.
                if minimal.len() == 1 {
                    minimal[0] = shrink_literals(&minimal[0], &ours, &oracle, args.timeout);
                }
                let src = build_program(&minimal);
                let a = run_prog(&oracle, ORACLE_OPTS, &src, args.timeout);
                let b = run_prog(&ours, OURS_OPTS, &src, args.timeout);
                let mut out =
                    format!("=== DIVERGENCE seed {seed} (replay: --seed {seed} --once) ===\n");
                for probe in &minimal {
                    out.push_str(&format!("  {probe}\n"));
                }
                out.push_str(&format!(
                    "  oracle: ok={} out={}\n",
                    a.ok,
                    render(&a.stdout)
                ));
                out.push_str(&format!("  ours  : ok={} out={}", b.ok, render(&b.stdout)));
                let _g = report.lock().unwrap_or_else(|e| e.into_inner());
                println!("{out}");
            });
        }
    });

    let failures = failures.into_inner();
    let probes_run = probes_run.into_inner();
    let skipped = skipped.into_inner();

    eprintln!(
        "parity-fuzz: {} program(s) ({skipped} skipped), {probes_run} verified probe(s), {failures} divergence(s)",
        iters
    );
    if failures > 0 {
        std::process::exit(1);
    }
}

/// The regex pools' pairing invariant, checked against the JDK's own answer.
///
/// The generator used to draw a pattern and a replacement from independent
/// pools, so it emitted programs like `"aabbcc".replaceAll("[a-z]", "$1$1")`
/// that the reference `java` aborts with `IndexOutOfBoundsException: No group
/// 1`. The whole packed program then produced no comparable output and was
/// counted a *skip* — which reads in the summary exactly like coverage that
/// passed. These tests pin the group accounting that replaced it, so the pools
/// cannot drift back into generating a program the oracle refuses to run.
/// [`literal_spans`] finds the operands a report should shrink, and nothing else.
///
/// The walk decides what a run of digits *is* from its neighbours, and every
/// wrong answer costs a JDK launch on a candidate that cannot compile — or worse,
/// silently rewrites a name and reports a reproducer that does not reproduce.
/// These pin the four judgements it makes.
#[cfg(test)]
mod literal_spans_tests {
    use super::literal_spans;

    fn found(probe: &str) -> Vec<&str> {
        literal_spans(probe)
            .into_iter()
            .map(|(a, b)| &probe[a..b])
            .collect()
    }

    #[test]
    fn a_bare_decimal_is_a_literal_and_a_digit_inside_a_name_is_not() {
        // `List12` and `arg0` are names. Rewriting the `12` in `List12` to `0`
        // produces `List0`, which does not compile and burns two JDK launches
        // per candidate to discover it.
        assert_eq!(found("List.of(1, 2).get(5);"), ["1", "2", "5"]);
        assert_eq!(found("x = List12 + arg0;"), Vec::<&str>::new());
        assert_eq!(found("a1b2c3"), Vec::<&str>::new());
    }

    #[test]
    fn a_suffixed_or_fractional_literal_is_left_whole_or_left_alone() {
        // `5L` and `1.0f` carry a type in their suffix; replacing the digits
        // alone would leave a bare `L`/`f`. A `1.0` with no suffix is one span,
        // not two — shrinking `1` and `0` independently would emit `.0` and `1.`.
        assert_eq!(found("long v = 5L; float f = 1.0f;"), Vec::<&str>::new());
        assert_eq!(found("double d = 1.0;"), ["1.0"]);
    }

    #[test]
    fn digits_inside_a_string_are_the_strings_content_not_the_programs() {
        // The span is the *inside* of the quotes, so the replacement stays a
        // valid string literal. The digits within it are not separate spans;
        // reporting them too would rewrite `"a5"` to `"a"5""`.
        assert_eq!(found("System.out.println(\"idx 5\" + 7);"), ["idx 5", "7"]);
        assert_eq!(found("s = \"\";"), [""]);
    }

    #[test]
    fn a_char_literal_and_an_escaped_quote_do_not_open_a_string() {
        // `'\"'` and `\"\\\"\"` each contain a quote that does not start one. A
        // walk that took either as an opener would treat the rest of the probe
        // as string content and find no spans at all in it.
        assert_eq!(found("c = '\"'; n = 4;"), ["4"]);
        assert_eq!(found("s = \"a\\\"b\"; n = 4;"), ["a\\\"b", "4"]);
        assert_eq!(found("c = '5'; n = 4;"), ["4"]);
    }
}

#[cfg(test)]
mod regex_pairing {
    use super::*;

    /// `Pattern.compile(p).matcher("").groupCount()` for every entry of
    /// [`PATTERNS`], in order, captured from a real JDK and re-verified against
    /// `openjdk 21.0.12` and `openjdk 26.0.2` (Homebrew), which agree on every
    /// entry. Frozen rather than recomputed so the invariant holds in CI without
    /// a JDK installed — the same reason `tests/data/parity_expected.txt` is
    /// frozen. Re-verify by feeding each pattern to
    /// `Pattern.compile(p).matcher("").groupCount()`; the values are the
    /// authority, and the JDK build that first produced them is not recorded
    /// because no build has ever disagreed.
    const JDK_GROUP_COUNTS: &[usize] = &[
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 3, 1, 2,
    ];

    /// The counter agrees with the JDK on every pattern in the pool — including
    /// the three shapes a naive `(`-count gets wrong: `(?:ab)+` and `(?i)a` are
    /// not capturing, `(?<=a)b` is a lookbehind rather than a named group, and
    /// `(?<g>a)b` is a named group rather than a lookbehind.
    #[test]
    fn group_counts_match_the_jdk() {
        assert_eq!(
            PATTERNS.len(),
            JDK_GROUP_COUNTS.len(),
            "PATTERNS changed without recapturing JDK_GROUP_COUNTS from a real JDK"
        );
        let mut wrong = Vec::new();
        for (p, want) in PATTERNS.iter().zip(JDK_GROUP_COUNTS) {
            let got = pattern_groups(p).0;
            if got != *want {
                wrong.push(format!("`{p}`: jdk={want} ours={got}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "group count diverged:\n  {}",
            wrong.join("\n  ")
        );
    }

    /// The lookaround/named-group boundary, stated directly rather than only
    /// via the pool, so a rewrite that collapses them is caught by name.
    #[test]
    fn lookbehind_is_not_a_named_group() {
        assert_eq!(pattern_groups("(?<=a)b"), (0, vec![]));
        assert_eq!(pattern_groups("(?<!a)b"), (0, vec![]));
        assert_eq!(pattern_groups("(?<g>a)b"), (1, vec!["g".to_string()]));
        assert_eq!(pattern_groups("(?:ab)+"), (0, vec![]));
        assert_eq!(pattern_groups("(?i)a"), (0, vec![]));
        // An escaped paren and a paren inside a character class are literal.
        assert_eq!(pattern_groups("\\\\(a\\\\)").0, 0);
        assert_eq!(pattern_groups("[()]").0, 0);
    }

    /// A `\`-escaped `$` is a literal dollar, not a reference — the pool entry
    /// that would otherwise look like a bare `$` and be rejected everywhere.
    #[test]
    fn replacement_references_are_read_through_both_escape_layers() {
        assert_eq!(replacement_refs("\"\\\\$\""), (0, vec![]));
        assert_eq!(replacement_refs("\"\\\\\\\\\""), (0, vec![]));
        assert_eq!(replacement_refs("\"[$0]\""), (0, vec![]));
        assert_eq!(replacement_refs("\"$1$1\""), (1, vec![]));
        assert_eq!(replacement_refs("\"$2$1\""), (2, vec![]));
        assert_eq!(replacement_refs("\"${g}\""), (0, vec!["g".to_string()]));
        assert_eq!(replacement_refs("\"[${g}]$0\""), (0, vec!["g".to_string()]));
    }

    /// Every replacement the generator can hand a pattern is legal against it,
    /// over the entire cross-product — the invariant the skips violated.
    #[test]
    fn every_reachable_pairing_is_legal() {
        let mut r = Rng::new(0xC0FFEE);
        for p in PATTERNS {
            let (count, names) = pattern_groups(p);
            // Sample enough draws that each legal replacement is hit; the check
            // is on what `pick_replacement` can return, not on the pool.
            for _ in 0..REPLACEMENTS.len() * 20 {
                let rep = pick_replacement(&mut r, p);
                let (max_num, refs) = replacement_refs(rep);
                assert!(
                    max_num <= count,
                    "`{p}` has {count} group(s) but was paired with `{rep}`"
                );
                for n in &refs {
                    assert!(
                        names.contains(n),
                        "`{p}` has no group named `{n}` but was paired with `{rep}`"
                    );
                }
            }
        }
    }

    /// The fix constrains the *pairing*, so it must not have cost coverage:
    /// every replacement in the pool is still reachable from some pattern, and
    /// every pattern still admits some replacement.
    #[test]
    fn no_replacement_and_no_pattern_became_unreachable() {
        for rep in REPLACEMENTS {
            assert!(
                PATTERNS.iter().any(|p| pairable(p, rep)),
                "replacement `{rep}` is no longer reachable from any pattern — \
                 the pairing fix would be trading skips for lost coverage"
            );
        }
        for p in PATTERNS {
            assert!(
                REPLACEMENTS.iter().any(|rep| pairable(p, rep)),
                "pattern `{p}` admits no replacement"
            );
        }
    }

    /// The pairing rule is what rejects the exact program that produced the
    /// skips, and it rejects it for the reason the JDK does.
    #[test]
    fn the_program_that_caused_the_skips_is_now_unpairable() {
        assert!(!pairable("[a-z]", "\"$1$1\""));
        assert!(!pairable("[a-z]", "\"<$1>\""));
        assert!(!pairable("(a)", "\"$2$1\""));
        assert!(!pairable("(a)", "\"${g}\""), "no group named g");
        // ...and admits the ones the JDK runs.
        assert!(pairable("[a-z]", "\"[$0]\""));
        assert!(pairable("(a)", "\"$1$1\""));
        assert!(pairable("(\\\\w)(\\\\w)", "\"$2$1\""));
        assert!(pairable("(?<g>a)", "\"${g}\""));
    }
}

/// A generator may not splice an operator onto a literal that already carries a
/// sign.
///
/// `g_cast` built its negated-operand probe as `-{d}` over a `DBLS` pool that
/// holds `-1.5`, producing `(int) --1.5`. Java lexes `--` as the decrement
/// operator, which requires a variable, so `javac` rejected the whole program
/// and the harness counted it a *skip* — 10 of 40 programs in `--mode cast`,
/// reported as "not compared" rather than as invalid Java. It is the same
/// defect class the regex pools had, reached the other way: there the oracle
/// aborted at run time, here it never compiled.
#[cfg(test)]
mod operand_splicing {
    use super::*;

    /// No mode may emit `--` or `++` against a numeric literal.
    ///
    /// Checked over every mode rather than over `cast` alone: the pools are
    /// shared, so any generator that grows a signed-operand probe inherits the
    /// same trap. `--x`/`x++` are ordinary and stay legal — it is only a digit
    /// or a `.` after the doubled operator that cannot be anything but the bug,
    /// since no literal is assignable.
    #[test]
    fn no_generator_prefixes_an_operator_onto_a_signed_literal() {
        let mut bad = Vec::new();
        for mode in CONCRETE {
            for seed in 0..400u64 {
                for probe in gen_probes(seed, *mode, 8) {
                    let cs: Vec<char> = probe.chars().collect();
                    for w in cs.windows(3) {
                        let doubled = (w[0] == '-' || w[0] == '+') && w[1] == w[0];
                        if doubled && (w[2].is_ascii_digit() || w[2] == '.') {
                            bad.push(format!("{}: {probe}", mode_name(*mode)));
                        }
                    }
                }
            }
        }
        bad.sort();
        bad.dedup();
        assert!(
            bad.is_empty(),
            "{} generator(s) spliced an operator onto a signed literal, which \
             `javac` rejects as a decrement of a non-variable:\n  {}",
            bad.len(),
            bad.join("\n  ")
        );
    }
}
