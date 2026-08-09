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
//! (`char`). Pure random bytes only produce
//! mutual parse errors that agree on both sides and teach nothing.
//!
//! Scope + determinism invariants (mirroring the scalars/node-js harnesses):
//!   * Only constructs javars actually implements are emitted — an unsupported
//!     construct would be a known gap, not a parity signal.
//!   * No nondeterministic output (no `Random`, no `currentTimeMillis`, no
//!     identity hashes, no unordered collections). Every probe's output is a pure
//!     function of its source.
//!   * Documented `BUGS.md` simplifications are NOT generated, because they would
//!     only reproduce known entries rather than find anything:
//!       - `NullPointerException` detail messages (Java's helpful NPE names the
//!         javac local slot, which javars cannot reproduce — the `fault` mode
//!         raises every *other* runtime fault and prints its message),
//!       - widening *value* conversion (`double d = 7;` printing `7` not `7.0`),
//!       - `==` identity on non-string objects (including two boxed
//!         `Character`s, which javars compares by value),
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
use std::sync::atomic::{AtomicU64, Ordering};
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
    let d = pick(r, DBLS);
    p((*d).to_string())
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
        2 => p(format!("(int) -{d}")),
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
    match r.below(14) {
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
const REPLACEMENTS: &[&str] = &[
    "\"-\"",
    "\"\"",
    "\"[$0]\"",
    "\"<$1>\"",
    "\"$1$1\"",
    "\"\\\\$\"",
    "\"x\"",
    "\"\\\\\\\\\"",
];

/// `java.util.regex` through `String.split`/`replaceAll`/`replaceFirst`/
/// `matches` — the pattern *translation* as much as the engine, since Java's
/// defaults and fancy-regex's disagree on `\d`, `\b`, `(?i)`, `.`, and `$`.
fn g_regex(r: &mut Rng) -> String {
    let pat = pick(r, PATTERNS);
    let s = pick(r, SUBJECTS);
    let rep = pick(r, REPLACEMENTS);
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

/// Helper declarations the `finally`/`resource` probes call. Emitted into every
/// generated program (they are inert when unused).
const SUPPORT: &str = concat!(
    "    static int fin1() { try { return 1; } finally { System.out.println(\"f1\"); } }\n",
    "    static int fin2() { int x = 5; try { return x; } finally { x = 99; } }\n",
    "    static int fin3() { try { return 6; } finally { return 66; } }\n",
    "    static int resret() { try (Res a = new Res(\"r\")) { return 7; } }\n",
    "    static String shout(Color c) { return c.name() + \"!\"; }\n",
    "    static Color next(Color c) { return c == Color.BLUE ? Color.RED : Color.values()[c.ordinal() + 1]; }\n",
    "    static int useCalc(Calc c, int a, int b) { return c.of(a, b); }\n",
    "    static Calc mkAdder(int n) { return (x, y) -> x + y + n; }\n",
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

/// Run one program through `prog`. The file must be named `T.java` for the JDK's
/// single-file source launcher, so each run gets its own directory.
fn run_prog(prog: &Path, src: &str, timeout: Duration) -> RunOut {
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

    let mut child = match Command::new(prog)
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
    let a = run_prog(oracle, &src, timeout);
    let b = run_prog(ours, &src, timeout);
    differs(&a, &b)
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
fn resolve_oracle(ours: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("JAVA_ORACLE") {
        return PathBuf::from(p);
    }
    let ours_canon = ours.canonicalize().ok();
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let cand = Path::new(dir).join("java");
            if !cand.exists() {
                continue;
            }
            if cand.canonicalize().ok() == ours_canon {
                continue;
            }
            return cand;
        }
    }
    eprintln!("parity-fuzz: no reference `java` on PATH (set JAVA_ORACLE=/path/to/java)");
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
                println!("parity-fuzz [--iters N] [--probes N] [--seed N] [--once] [--mode M] [--timeout SECS] [-v]");
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
    let mut failures = 0usize;
    let mut probes_run = 0usize;

    for k in 0..iters {
        let seed = if args.once {
            base
        } else {
            base.wrapping_add(k as u64)
        };
        let probes = gen_probes(seed, args.mode, args.probes);
        probes_run += probes.len();
        if !diverges(&probes, &ours, &oracle, args.timeout) {
            if args.verbose {
                eprintln!("seed {seed}: ok ({} probes)", probes.len());
            }
            continue;
        }
        failures += 1;
        let minimal = minimize(&probes, &ours, &oracle, args.timeout);
        let src = build_program(&minimal);
        let a = run_prog(&oracle, &src, args.timeout);
        let b = run_prog(&ours, &src, args.timeout);
        println!("=== DIVERGENCE seed {seed} (replay: --seed {seed} --once) ===");
        for probe in &minimal {
            println!("  {probe}");
        }
        println!("  oracle: ok={} out={}", a.ok, render(&a.stdout));
        println!("  ours  : ok={} out={}", b.ok, render(&b.stdout));
    }

    eprintln!(
        "parity-fuzz: {} program(s), {probes_run} probe(s), {failures} divergence(s)",
        iters
    );
    if failures > 0 {
        std::process::exit(1);
    }
}
