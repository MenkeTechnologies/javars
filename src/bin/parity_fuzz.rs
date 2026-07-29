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
//! try-with-resources close ordering (`resource`), and `enum` identity and
//! ordering (`enum`). Pure random bytes only produce mutual parse errors that
//! agree on both sides and teach nothing.
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
//!       - `char` as a one-character string,
//!       - `==` identity on non-string objects,
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
    match r.below(5) {
        0 => p(format!("String.format(\"%d\", {})", pick(r, INTS))),
        1 => p(format!("String.format(\"%s\", {})", pick(r, STRS))),
        2 => p(format!("String.format(\"%5d|\", {})", pick(r, INTS))),
        3 => p(format!("String.format(\"%-5d|\", {})", pick(r, INTS))),
        _ => p(format!(
            "String.format(\"%x\", Math.abs({}))",
            pick(r, INTS)
        )),
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
    match r.below(7) {
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
        _ => format!(
            "{{ Op o = Op.{}; System.out.println(o.apply({}, {}) + \" \" + o.isMul() + \" \" + o); }}",
            pick(r, &["ADD", "SUB", "MUL"]),
            pick(r, INTS),
            pick(r, DIVS)
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
);

/// The `AutoCloseable` the resource probes open and close.
const SUPPORT_CLASS: &str = concat!(
    "enum Color { RED, GREEN, BLUE }\n",
    "enum Op {\n",
    "    ADD, SUB, MUL;\n",
    "    int apply(int a, int b) { switch (this) { case ADD: return a + b; case SUB: return a - b; default: return a * b; } }\n",
    "    boolean isMul() { return this == MUL; }\n",
    "}\n",
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
    let mut s = String::from("public class T {\n    public static void main(String[] args) {\n");
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
