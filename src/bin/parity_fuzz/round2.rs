//! Second-round probe generators for `parity-fuzz`.
//!
//! Each mode here exists because a *shape* the first generation could not emit
//! was the shape a divergence hid behind:
//!
//!   * `clinit` — class initialization order (JLS 12.4.1). Every earlier mode ran
//!     in a program whose classes had no observable static initializer, so
//!     running them all before `main` was indistinguishable from running them on
//!     first use.
//!   * `ternprom` — the numeric-promotion table of the conditional operator
//!     (JLS 15.25), including the `Integer`/`null` pairing that unboxes and so
//!     throws.
//!   * `intcache` — `==` between boxes at the edges of the `valueOf` cache.
//!   * `numprom` — compound assignment, increments and shifts across all seven
//!     numeric types, where each statement narrows back to its target's width.
//!   * `fmtflag` — `String.format` flag/width/precision/conversion products,
//!     including the combinations the JDK rejects.
//!   * `sbuild` — `StringBuilder` mutation sequences with in-range and
//!     out-of-range indices.
//!   * `hashiter` — the iteration order of hash-based collections as they grow.
//!   * `exc` — exception text, causes, suppression and `finally` ordering.
//!   * `overload` — overload resolution across boxing, widening and `Object`.
//!   * `swfall` — `switch` on strings (including hash collisions), ints, chars
//!     and enums, with fall-through, a mid-list `default`, and `yield`.
//!   * `sealed` — sealed hierarchies of records under pattern matching.
//!   * `capture` — what a lambda, an anonymous class and an inner class see.
//!   * `generic` — erased generic classes and bounded generic methods.
//!   * `failfast` — structural modification of a collection during an enhanced
//!     `for`, which Java reports as `ConcurrentModificationException`.

use super::{p, pick, Rng};

/// Members declared inside `T`, appended to the first generation's.
pub const SUPPORT: &str = concat!(
    // ── exc ──────────────────────────────────────────────────────────────
    "    static class ZEx extends Exception {\n",
    "        final int code;\n",
    "        ZEx(String m, int c) { super(m); code = c; }\n",
    "        ZEx(String m, Throwable cause) { super(m, cause); code = -1; }\n",
    "    }\n",
    "    static class ZRt extends RuntimeException {\n",
    "        ZRt(String m) { super(m); }\n",
    "        @Override public String getMessage() { return \"ZRt:\" + super.getMessage(); }\n",
    "    }\n",
    "    static class ZRes implements AutoCloseable {\n",
    "        final String n; final boolean boom;\n",
    "        ZRes(String n, boolean boom) { this.n = n; this.boom = boom; System.out.println(\"open \" + n); }\n",
    "        public void close() throws ZEx { System.out.println(\"close \" + n); if (boom) throw new ZEx(\"close-\" + n, 9); }\n",
    "    }\n",
    "    static int zf(int k) throws ZEx {\n",
    "        if (k == 1) throw new ZEx(\"one\", 1);\n",
    "        if (k == 2) throw new ZRt(\"two\");\n",
    "        if (k == 3) throw new IllegalStateException(\"three\");\n",
    "        if (k == 4) throw new ZEx(\"wrapped\", new ArithmeticException(\"root\"));\n",
    "        return k;\n",
    "    }\n",
    "    @SuppressWarnings(\"finally\")\n",
    "    static int zfin(int k) {\n",
    "        int x = 1;\n",
    "        try {\n",
    "            if (k == 0) return x;\n",
    "            if (k == 1) throw new RuntimeException(\"boom\");\n",
    "            x = 2;\n",
    "        } catch (RuntimeException e) {\n",
    "            x = 3;\n",
    "            return x;\n",
    "        } finally {\n",
    "            x += 10;\n",
    "            if (k == 2) return x;\n",
    "            System.out.println(\"zfin-finally \" + x);\n",
    "        }\n",
    "        return x;\n",
    "    }\n",
    "    static String zloop(int n) {\n",
    "        StringBuilder sb = new StringBuilder();\n",
    "        for (int i = 0; i < n; i++) {\n",
    "            try {\n",
    "                if (i % 3 == 0) continue;\n",
    "                if (i == 7) break;\n",
    "                sb.append('t').append(i);\n",
    "            } finally {\n",
    "                sb.append('f');\n",
    "            }\n",
    "        }\n",
    "        return sb.toString();\n",
    "    }\n",
    // ── overload ─────────────────────────────────────────────────────────
    "    static String oa(int x) { return \"int\"; }\n",
    "    static String oa(long x) { return \"long\"; }\n",
    "    static String oa(double x) { return \"double\"; }\n",
    "    static String oa(Object x) { return \"Object\"; }\n",
    "    static String ob(Integer x) { return \"Integer\"; }\n",
    "    static String ob(Object x) { return \"Object\"; }\n",
    "    static String ob(long x) { return \"long\"; }\n",
    "    static String oc(char x) { return \"char\"; }\n",
    "    static String oc(int x) { return \"int\"; }\n",
    "    static String oc(String x) { return \"String\"; }\n",
    "    static String od(Object x) { return \"Object\"; }\n",
    "    static String od(String x) { return \"String\"; }\n",
    "    static String od(CharSequence x) { return \"CharSequence\"; }\n",
    "    static String oe(double x) { return \"double\"; }\n",
    "    static String oe(float x) { return \"float\"; }\n",
    "    static String oh(long a, int b) { return \"li\"; }\n",
    "    static String oh(int a, long b) { return \"il\"; }\n",
    "    static String oi(short x) { return \"short\"; }\n",
    "    static String oi(int x) { return \"int\"; }\n",
    // ── sealed ───────────────────────────────────────────────────────────
    "    sealed interface Zs permits ZsA, ZsB, ZsC {}\n",
    "    record ZsA(int v) implements Zs {}\n",
    "    record ZsB(String s, Zs inner) implements Zs {}\n",
    "    record ZsC(double d, boolean f) implements Zs {}\n",
    "    static String zdesc(Zs z) {\n",
    "        return switch (z) {\n",
    "            case ZsA a when a.v() > 5 -> \"bigA\" + a.v();\n",
    "            case ZsA a -> \"A\" + a.v();\n",
    "            case ZsB(String s, ZsA(int v)) -> \"BA\" + s + v;\n",
    "            case ZsB(var s, var in) -> \"B\" + s + \"(\" + zdesc(in) + \")\";\n",
    "            case ZsC(double d, boolean f) when f -> \"Cf\" + d;\n",
    "            case ZsC c -> \"C\" + c.d();\n",
    "        };\n",
    "    }\n",
    "    static int zdepth(Object o) {\n",
    "        if (o instanceof ZsB(String s, Zs in)) return 1 + zdepth(in);\n",
    "        if (o instanceof ZsA a && a.v() > 0) return 10;\n",
    "        return 0;\n",
    "    }\n",
    "    static String zobj(Object o) {\n",
    "        return switch (o) {\n",
    "            case null -> \"null\";\n",
    "            case Integer i when i < 0 -> \"neg\" + i;\n",
    "            case Integer i -> \"int\" + i;\n",
    "            case Long l -> \"long\" + l;\n",
    "            case String s when s.length() > 3 -> \"long-string\";\n",
    "            case String s -> \"string\" + s;\n",
    "            case Zs z -> \"zs:\" + zdesc(z);\n",
    "            default -> \"other\";\n",
    "        };\n",
    "    }\n",
    // ── capture ──────────────────────────────────────────────────────────
    "    interface ZSup { int get(); }\n",
    "    static class ZOuter {\n",
    "        int x = 5; int y;\n",
    "        ZOuter(int y) { this.y = y; }\n",
    "        class In {\n",
    "            int x = 7;\n",
    "            int sum() { return x + ZOuter.this.x + y; }\n",
    "            int shadow(int x) { return x + this.x + ZOuter.this.x; }\n",
    "        }\n",
    "        In mk() { return new In(); }\n",
    "        ZSup sup() { return new ZSup() { int calls; public int get() { calls++; return x * 10 + y + calls; } }; }\n",
    "        ZSup lam() { return () -> x + y; }\n",
    "        void bump() { x++; }\n",
    "    }\n",
    // ── generic ──────────────────────────────────────────────────────────
    "    static class ZBox<T> {\n",
    "        T v;\n",
    "        ZBox(T v) { this.v = v; }\n",
    "        T get() { return v; }\n",
    "        <R> ZBox<R> map(java.util.function.Function<T, R> f) { return new ZBox<>(f.apply(v)); }\n",
    "        public String toString() { return \"Box(\" + v + \")\"; }\n",
    "    }\n",
    "    static class ZPair<A, B> {\n",
    "        final A a; final B b;\n",
    "        ZPair(A a, B b) { this.a = a; this.b = b; }\n",
    "        A first() { return a; }\n",
    "        B second() { return b; }\n",
    "        ZPair<B, A> swap() { return new ZPair<>(b, a); }\n",
    "        public String toString() { return \"(\" + a + \", \" + b + \")\"; }\n",
    "    }\n",
    "    static <T extends Comparable<T>> T zmax(T a, T b) { return a.compareTo(b) >= 0 ? a : b; }\n",
    "    static double zsum(List<? extends Number> xs) { double t = 0; for (Number n : xs) t += n.doubleValue(); return t; }\n",
    "    static <T> List<T> zrep(T x, int n) { List<T> l = new ArrayList<>(); for (int i = 0; i < n; i++) l.add(x); return l; }\n",
    "    static <T> int zcount(Collection<T> c, T x) { int n = 0; for (T e : c) if (e.equals(x)) n++; return n; }\n",
);

/// Top-level types appended to the first generation's.
pub const SUPPORT_CLASS: &str = concat!(
    // The log every `clinit` initializer writes to. It is itself lazily
    // initialized, which is part of the point: the first note triggers it.
    "class ZLog {\n",
    "    static StringBuilder sb = new StringBuilder();\n",
    "    static int note(String s) { sb.append(s).append(';'); return sb.length(); }\n",
    "}\n",
    "class ZiA {\n",
    "    static int a = ZLog.note(\"A\");\n",
    "    static { ZLog.note(\"Ablk\"); }\n",
    "    static int m = 3;\n",
    "    static final int K = 7;\n",
    "    static int get() { return a; }\n",
    "}\n",
    "class ZiB extends ZiA {\n",
    "    static int b = ZLog.note(\"B\");\n",
    "    ZiB() { ZLog.note(\"Bctor\"); }\n",
    "    static int bget() { return b; }\n",
    "}\n",
    "class ZiC {\n",
    "    static final ZiC INST = new ZiC();\n",
    "    static int cnt = 5;\n",
    "    int seen;\n",
    "    ZiC() { seen = cnt; cnt++; ZLog.note(\"Cctor\"); }\n",
    "}\n",
    "interface ZiI {\n",
    "    int V = ZLog.note(\"I\");\n",
    "    static int sv() { return 1; }\n",
    "}\n",
    "class ZiD implements ZiI {\n",
    "    static int d = ZLog.note(\"D\");\n",
    "}\n",
    "interface ZiJ {\n",
    "    int W = ZLog.note(\"J\");\n",
    "    default int dm() { return 1; }\n",
    "}\n",
    "class ZiK implements ZiJ {\n",
    "    static int k = ZLog.note(\"K\");\n",
    "}\n",
    "enum ZiE {\n",
    "    P, Q;\n",
    "    ZiE() { ZLog.note(\"Ector\"); }\n",
    "    static { ZLog.note(\"Eblk\"); }\n",
    "}\n",
);

/// Class initialization order. A probe reads a value that forces some class's
/// initialization and prints the log so far, so the *order* the initializers
/// ran in is the output.
pub fn g_clinit(r: &mut Rng) -> String {
    let expr = *pick(
        r,
        &[
            "ZiA.get()",
            "ZiA.K",
            "ZiA.m",
            "ZiB.m",
            "ZiB.bget()",
            "(new ZiB() != null ? 1 : 0)",
            "ZiC.INST.seen",
            "ZiC.cnt",
            "ZiD.d",
            "ZiI.sv()",
            "ZiI.V",
            "(new ZiD() != null ? 1 : 0)",
            "ZiK.k",
            "new ZiK().dm()",
            "ZiE.P.ordinal()",
            "ZiE.values().length",
            "ZiE.valueOf(\"Q\").ordinal()",
        ],
    );
    format!("{{ int u = {expr}; System.out.println(ZLog.sb + \"|\" + u); }}")
}

/// The operands the conditional-operator promotion table is probed with. The
/// declarations a probe needs are emitted ahead of the conditional.
const TERN_OPERANDS: &[&str] = &[
    "1",
    "97",
    "2L",
    "3.5",
    "4.5f",
    "'a'",
    "(byte) 5",
    "(short) 6",
    "vi",
    "vl",
    "vd",
    "vf",
    "vc",
    "vb",
    "vs",
    "wi",
    "wl",
    "wd",
    "wc",
    "ws",
    "wb",
];

/// `flag ? a : b` over mixed primitive and wrapper operands, reporting the
/// value *and* the class of the boxed result — which is where a wrong static
/// type shows even when the printed digits agree (`1` against `1.0`).
pub fn g_ternprom(r: &mut Rng) -> String {
    let a = pick(r, TERN_OPERANDS);
    let b = pick(r, TERN_OPERANDS);
    let flag = if r.below(2) == 0 {
        "args.length == 0"
    } else {
        "args.length != 0"
    };
    format!(
        "{{ int vi = 7; long vl = 8L; double vd = 9.5; float vf = 1.5f; char vc = 'z'; \
         byte vb = 3; short vs = 4; Integer wi = 10; Long wl = 11L; Double wd = 12.5; \
         Character wc = 'q'; Short ws = 13; Byte wb = 14; boolean f = {flag}; \
         Object o = f ? {a} : {b}; \
         System.out.println(o + \":\" + o.getClass().getSimpleName()); }}"
    )
}

/// A `null` `Integer` meeting a primitive in a conditional, a switch, an array
/// index and arithmetic: each unboxes, so each throws, where the same `null`
/// merely passes through a reference position.
pub fn g_nullunbox(r: &mut Rng) -> String {
    let n = pick(r, &["null", "5"]);
    let body = match r.below(10) {
        0 => "int v = f ? n : 0; System.out.println(v);",
        1 => "Integer v = f ? n : 0; System.out.println(v);",
        2 => "Integer v = f ? n : Integer.valueOf(0); System.out.println(v);",
        3 => "Object v = f ? n : \"s\"; System.out.println(v);",
        4 => "switch (n) { case 5: System.out.println(\"five\"); break; default: System.out.println(\"other\"); }",
        5 => "int[] a = new int[8]; a[n] = 3; System.out.println(a[5]);",
        6 => "int v = n + 1; System.out.println(v);",
        7 => "n++; System.out.println(n);",
        8 => "long v = f ? n : 1L; System.out.println(v);",
        _ => "if (n == 5) System.out.println(\"eq\"); else System.out.println(\"ne\");",
    };
    format!(
        "try {{ Integer n = {n}; boolean f = args.length == 0; {body} }} \
         catch (NullPointerException e) {{ System.out.println(\"NPE\"); }}"
    )
}

/// `==` between two boxes across the edges of the `valueOf` cache.
pub fn g_intcache(r: &mut Rng) -> String {
    let v = *pick(r, &["-129", "-128", "-1", "0", "1", "127", "128", "1000"]);
    match r.below(12) {
        0 => format!(
            "{{ Integer a = {v}, b = {v}; System.out.println((a == b) + \",\" + a.equals(b) + \",\" + (a <= b && a >= b)); }}"
        ),
        1 => format!(
            "{{ Long a = {v}L, b = {v}L; System.out.println((a == b) + \",\" + a.equals(b)); }}"
        ),
        2 => format!(
            "{{ Short a = (short) {v}, b = (short) {v}; System.out.println((a == b) + \",\" + a.equals(b)); }}"
        ),
        3 => format!(
            "{{ Byte a = (byte) {v}, b = (byte) {v}; System.out.println((a == b) + \",\" + a.equals(b)); }}"
        ),
        4 => format!(
            "{{ System.out.println(Integer.valueOf({v}) == Integer.valueOf({v})); }}"
        ),
        5 => format!(
            "{{ Integer a = {v}; int p = {v}; System.out.println((a == p) + \",\" + (p == a)); }}"
        ),
        6 => format!(
            "{{ Integer a = {v}; Integer b = a; System.out.println(a == b); }}"
        ),
        7 => format!(
            "{{ Integer a = {v}, b = {v}; System.out.println((a + 0 == b + 0) + \",\" + (a.intValue() == b)); }}"
        ),
        8 => format!(
            "{{ System.out.println(Integer.valueOf({v}).equals(Long.valueOf({v})) + \",\" + Long.valueOf({v}).equals((long) {v})); }}"
        ),
        9 => format!(
            "{{ List<Integer> l = List.of({v}, {v}); System.out.println((l.get(0) == l.get(1)) + \",\" + l.get(0).equals(l.get(1))); }}"
        ),
        10 => format!(
            "{{ Map<Integer, Integer> m = new HashMap<>(); m.put(1, {v}); m.put(2, {v}); System.out.println(m.get(1) == m.get(2)); }}"
        ),
        _ => format!(
            "{{ Integer x = {v}; x += 0; Integer y = {v}; System.out.println(x == y); }}"
        ),
    }
}

/// The seven numeric types, each as a local, and compound statements across
/// them. A statement narrows to its target's width, so the printed sequence is
/// the whole record of which conversions ran.
pub fn g_numprom(r: &mut Rng) -> String {
    const VARS: &[(&str, &str)] = &[
        ("B", "byte"),
        ("S", "short"),
        ("C", "char"),
        ("I", "int"),
        ("L", "long"),
        ("F", "float"),
        ("D", "double"),
    ];
    const ARITH: &[&str] = &["+=", "-=", "*=", "/=", "%="];
    const BITS: &[&str] = &["<<=", ">>=", ">>>=", "&=", "|=", "^="];
    // Operand literals chosen to overflow each narrow width and to fall
    // between integers, without a zero the integral `/=` and `%=` would trap on.
    const LITS: &[&str] = &[
        "1", "3", "7", "100", "200", "-5", "70000", "1.5", "2.75", "1e10", "3L", "0.5f",
    ];
    let mut body = String::from(
        "byte B = 100; short S = 30000; char C = 'x'; int I = 2000000000; long L = 5000000000L; \
         float F = 1.25f; double D = 0.1; ",
    );
    for _ in 0..(2 + r.below(3)) {
        let (target, ty) = *pick(r, VARS);
        let floating = ty == "float" || ty == "double";
        let stmt = match r.below(6) {
            0 | 1 => {
                let op = pick(r, ARITH);
                let rhs = if r.below(2) == 0 {
                    (*pick(r, LITS)).to_string()
                } else {
                    pick(r, VARS).0.to_string()
                };
                // `%=` and `/=` by an integral zero-valued variable would throw
                // identically on both sides; the pool never produces one.
                format!("{target} {op} {rhs};")
            }
            2 if !floating => {
                let op = pick(r, BITS);
                let rhs = pick(r, &["1", "3", "31", "33", "64", "-1", "200", "7L"]);
                format!("{target} {op} {rhs};")
            }
            3 => format!("{target}++;"),
            4 => format!("--{target};"),
            _ => {
                let (src, _) = *pick(r, VARS);
                format!("{target} = ({ty}) ({src} + {});", pick(r, LITS))
            }
        };
        body.push_str(&stmt);
        body.push(' ');
    }
    body.push_str("System.out.println(B + \",\" + S + \",\" + (int) C + \",\" + I + \",\" + L + \",\" + F + \",\" + D);");
    format!("try {{ {body} }} catch (ArithmeticException e) {{ System.out.println(\"AE\"); }}")
}

/// `String.format` over flag subsets, width, precision and conversion, printing
/// either the result or the class the JDK rejects the combination with.
pub fn g_fmtflag(r: &mut Rng) -> String {
    const FLAGS: &[char] = &['-', '#', '+', ' ', '0', ',', '('];
    let mut flags = String::new();
    for f in FLAGS {
        if r.below(4) == 0 {
            flags.push(*f);
        }
    }
    let width = *pick(r, &["", "", "1", "6", "12"]);
    let prec = *pick(r, &["", "", ".0", ".2", ".5"]);
    let (conv, value) = match r.below(12) {
        0 | 1 => (
            'd',
            *pick(
                r,
                &[
                    "0",
                    "-42",
                    "1234567",
                    "Integer.MIN_VALUE",
                    "255L",
                    "(byte) -1",
                ],
            ),
        ),
        2 => ('x', *pick(r, &["255", "-1", "255L", "(byte) -1", "0"])),
        3 => ('X', *pick(r, &["255", "-255", "4096L"])),
        4 => ('o', *pick(r, &["8", "-8", "64L"])),
        5 | 6 => (
            'f',
            *pick(
                r,
                &[
                    "0.0",
                    "-1.5",
                    "1234.5678",
                    "1e10",
                    "0.000123",
                    "2.5",
                    "0.125",
                    "1.1f",
                    "Double.NaN",
                ],
            ),
        ),
        7 => ('e', *pick(r, &["12345.678", "0.0", "-0.00123", "1e100"])),
        8 => ('s', *pick(r, &["\"hello\"", "null", "5", "\"\"", "1.5"])),
        9 => ('c', *pick(r, &["'a'", "97", "'~'"])),
        10 => ('b', *pick(r, &["true", "null", "\"x\"", "false"])),
        _ => ('g', *pick(r, &["0.0001234", "123456789.0", "1.0", "-5.5"])),
    };
    let conv = if matches!(conv, 'd' | 'x' | 'X' | 'o' | 'c') {
        conv
    } else if r.below(5) == 0 && matches!(conv, 'e' | 's' | 'g') {
        conv.to_ascii_uppercase()
    } else {
        conv
    };
    let spec = format!("%{flags}{width}{prec}{conv}");
    format!(
        "try {{ System.out.println(\"[\" + String.format(\"{spec}\", {value}) + \"]\"); }} \
         catch (java.util.IllegalFormatException e) {{ System.out.println(e.getClass().getSimpleName()); }}"
    )
}

/// `StringBuilder` mutation with indices that are sometimes outside the
/// builder, so the exception class and message are part of the output.
pub fn g_sbuild(r: &mut Rng) -> String {
    let mut s = format!(
        "{{ StringBuilder sb = new StringBuilder({}); ",
        pick(r, &["\"\"", "\"abc\"", "\"hello world\"", "\"x\""])
    );
    for _ in 0..(3 + r.below(4)) {
        let i = r.below(9);
        let j = r.below(9);
        let op = match r.below(16) {
            0 => format!(
                "sb.append({});",
                pick(r, &["1", "'c'", "2.5", "true", "\"str\"", "7L", "1.5f"])
            ),
            1 => format!(
                "sb.insert({i}, {});",
                pick(r, &["\"ins\"", "42", "'z'", "false", "3.5"])
            ),
            2 => format!("sb.delete({i}, {j});"),
            3 => format!("sb.deleteCharAt({i});"),
            4 => format!("sb.replace({i}, {j}, \"RR\");"),
            5 => "sb.reverse();".to_string(),
            6 => format!("sb.setCharAt({i}, 'Q');"),
            7 => format!("sb.setLength({i});"),
            8 => format!("System.out.print(sb.charAt({i}));"),
            9 => format!(
                "System.out.print(sb.indexOf(\"{}\"));",
                pick(r, &["a", "l", "o w", "z", ""])
            ),
            10 => format!("System.out.print(sb.substring({i}));"),
            11 => format!("System.out.print(sb.substring({i}, {j}));"),
            12 => format!(
                "System.out.print(sb.lastIndexOf(\"{}\"));",
                pick(r, &["l", "c", "x"])
            ),
            13 => "sb.append(sb);".to_string(),
            14 => format!("sb.insert({i}, sb.length());"),
            _ => format!("sb.append(\"{}\", 0, 1);", pick(r, &["xyz", "q"])),
        };
        s.push_str(&format!(
            "try {{ {op} }} catch (RuntimeException e) {{ System.out.print(\"<\" + e.getClass().getSimpleName() + \":\" + e.getMessage() + \">\"); }} "
        ));
    }
    s.push_str("System.out.println(\"|\" + sb + \"|\" + sb.length()); }");
    s
}

/// Hash-based collections as they grow: insertion order is not iteration order,
/// and the order moves when the table resizes.
pub fn g_hashiter(r: &mut Rng) -> String {
    let n = 3 + r.below(30);
    let step = *pick(r, &["1", "7", "17", "31", "-3", "4099"]);
    let removes = if r.below(2) == 0 {
        ""
    } else {
        "for (int i = 0; i < n; i += 3) m.remove(i * step); "
    };
    match r.below(8) {
        0 => format!(
            "{{ int n = {n}, step = {step}; Map<Integer, String> m = new HashMap<>(); \
             for (int i = 0; i < n; i++) m.put(i * step, \"v\" + i); {removes}System.out.println(m); }}"
        ),
        1 => format!(
            "{{ int n = {n}, step = {step}; Set<Integer> s = new HashSet<>(); \
             for (int i = 0; i < n; i++) s.add(i * step); System.out.println(s); }}"
        ),
        2 => format!(
            "{{ int n = {n}; Set<String> s = new HashSet<>(); \
             for (int i = 0; i < n; i++) s.add(\"k\" + (i * 7 % 23)); System.out.println(s); }}"
        ),
        3 => format!(
            "{{ int n = {n}; Map<Long, Integer> m = new HashMap<>(); \
             for (int i = 0; i < n; i++) m.put((long) i << 31, i); System.out.println(m.keySet()); }}"
        ),
        4 => format!(
            "{{ int n = {n}; Map<Double, Integer> m = new HashMap<>(); \
             for (int i = 0; i < n; i++) m.put(i * 0.37, i); System.out.println(m.values()); }}"
        ),
        5 => format!(
            "{{ int n = {n}; Map<Character, Integer> m = new HashMap<>(); \
             for (int i = 0; i < n; i++) m.put((char) ('a' + i * 5 % 26), i); System.out.println(m); }}"
        ),
        6 => format!(
            "{{ int n = {n}; Map<Integer, Integer> m = new HashMap<>({cap}); \
             for (int i = 0; i < n; i++) m.merge(i * 5 % 11, 1, Integer::sum); System.out.println(m); }}",
            cap = pick(r, &["1", "4", "100", "0"])
        ),
        _ => format!(
            "{{ int n = {n}; Map<String, Integer> m = new LinkedHashMap<>(); \
             for (int i = 0; i < n; i++) m.put(\"k\" + i % 6, i); m.remove(\"k0\"); m.put(\"k0\", -1); System.out.println(m); }}"
        ),
    }
}

/// Exception text, causes, suppression and `finally` ordering.
pub fn g_exc(r: &mut Rng) -> String {
    let k = r.below(5);
    let a = pick(r, &["true", "false"]);
    let b = pick(r, &["true", "false"]);
    match r.below(10) {
        0 => format!(
            "{{ try {{ System.out.println(zf({k})); }} \
             catch (ZEx e) {{ System.out.println(\"ZEx \" + e.getMessage() + \" \" + e.code + \" \" + e.getCause()); }} \
             catch (RuntimeException e) {{ System.out.println(\"RT \" + e); }} \
             finally {{ System.out.println(\"fin\"); }} }}"
        ),
        1 => format!(
            "{{ try (ZRes r1 = new ZRes(\"a\", {a}); ZRes r2 = new ZRes(\"b\", {b})) {{ System.out.println(\"body\"); if ({k} > 2) throw new ZRt(\"in-body\"); }} \
             catch (ZEx e) {{ System.out.println(\"caught \" + e.getMessage() + \" supp=\" + e.getSuppressed().length); }} \
             catch (ZRt e) {{ System.out.println(\"caught \" + e.getMessage() + \" supp=\" + e.getSuppressed().length \
             + (e.getSuppressed().length > 0 ? \" first=\" + e.getSuppressed()[0].getMessage() : \"\")); }} }}"
        ),
        2 => format!("{{ System.out.println(zfin({})); }}", k % 3),
        3 => format!("{{ System.out.println(zloop({})); }}", 4 + k * 2),
        4 => "{ try { try { throw new ZRt(\"a\"); } finally { throw new ZRt(\"b\"); } } \
              catch (ZRt e) { System.out.println(e.getMessage() + \" \" + e.getSuppressed().length); } }"
            .to_string(),
        5 => format!(
            "{{ try {{ try {{ zf({k}); }} catch (ZEx e) {{ throw new RuntimeException(\"wrap\", e); }} }} \
             catch (RuntimeException e) {{ System.out.println(e.getMessage() + \"/\" + (e.getCause() == null ? \"none\" : e.getCause().getMessage())); }} \
             catch (Exception e) {{ System.out.println(\"other \" + e); }} }}"
        ),
        6 => "{ Throwable t = new ZEx(\"m\", 5); System.out.println(t.getClass().getName() + \" \" + t.getLocalizedMessage() + \" \" + t); }"
            .to_string(),
        7 => "{ System.out.println(new ZRt(null).getMessage() + \" \" + new ZRt(null) + \" \" + new RuntimeException((Throwable) null).getMessage()); }"
            .to_string(),
        8 => format!(
            "{{ try {{ zf({k}); System.out.println(\"no throw\"); }} \
             catch (ZEx | IllegalStateException e) {{ System.out.println(\"multi \" + e.getClass().getSimpleName() + \" \" + e.getMessage()); }} \
             catch (RuntimeException e) {{ System.out.println(\"rt \" + e.getMessage()); }} }}"
        ),
        _ => format!(
            "{{ int c = 0; for (int i = 0; i < 4; i++) {{ try {{ zf(i + {k} % 2); c += 1; }} \
             catch (ZEx e) {{ c += 10; }} catch (RuntimeException e) {{ c += 100; }} finally {{ c += 1000; }} }} \
             System.out.println(c); }}"
        ),
    }
}

/// Overload resolution across the families declared in [`SUPPORT`].
pub fn g_overload(r: &mut Rng) -> String {
    let call = match r.below(8) {
        0 | 1 => format!(
            "oa({})",
            pick(
                r,
                &[
                    "1",
                    "1L",
                    "1.5",
                    "1.5f",
                    "'c'",
                    "(byte) 1",
                    "(short) 2",
                    "Integer.valueOf(3)",
                    "\"s\"",
                    "(Object) 5",
                    "Long.valueOf(2)",
                    "true"
                ],
            )
        ),
        2 => format!(
            "ob({})",
            pick(
                r,
                &[
                    "1",
                    "1L",
                    "Integer.valueOf(1)",
                    "(short) 1",
                    "'c'",
                    "\"x\"",
                    "1.5"
                ]
            )
        ),
        3 => format!(
            "oc({})",
            pick(
                r,
                &[
                    "'a'",
                    "97",
                    "\"s\"",
                    "(char) 98",
                    "(short) 1",
                    "(byte) 2",
                    "'a' + 1"
                ]
            )
        ),
        4 => format!(
            "od({})",
            pick(
                r,
                &[
                    "\"s\"",
                    "(Object) \"s\"",
                    "new StringBuilder(\"x\")",
                    "5",
                    "null"
                ]
            )
        ),
        5 => format!("oe({})", pick(r, &["1", "1L", "1.5f", "1.5", "'a'"])),
        6 => (*pick(
            r,
            &["oh(1L, 2)", "oh(1, 2L)", "oh('a', 2L)", "oh((short) 1, 2L)"],
        ))
        .to_string(),
        _ => format!("oi({})", pick(r, &["(byte) 1", "(short) 1", "1", "'a'"])),
    };
    p(call)
}

/// `switch` statements and expressions with fall-through, a `default` that is
/// not last, string labels that collide on `hashCode`, and `yield`.
pub fn g_swfall(r: &mut Rng) -> String {
    match r.below(5) {
        0 => {
            // String switch with colliding keys ("Aa" and "BB" share a hash).
            let subject = pick(
                r,
                &[
                    "\"Aa\"", "\"BB\"", "\"C\"", "\"\"", "\"AaAa\"", "\"BBBB\"", "\"zz\"",
                ],
            );
            let mut arms: Vec<(String, u32)> =
                ["\"Aa\"", "\"BB\"", "\"C\"", "\"\"", "\"AaAa\"", "\"BBBB\""]
                    .iter()
                    .enumerate()
                    .filter(|_| r.below(3) != 0)
                    .map(|(i, l)| (format!("case {l}:"), 1u32 << i))
                    .collect();
            arms.push(("default:".to_string(), 64));
            // A deterministic shuffle: rotate by a drawn amount, then swap a pair.
            let rot = r.below(arms.len());
            arms.rotate_left(rot);
            if arms.len() > 2 {
                let (x, y) = (r.below(arms.len()), r.below(arms.len()));
                arms.swap(x, y);
            }
            let mut body = String::new();
            for (label, bit) in &arms {
                body.push_str(&format!("{label} r += {bit}; "));
                if r.below(2) == 0 {
                    body.push_str("break; ");
                }
            }
            format!("{{ String s = {subject}; int r = 0; switch (s) {{ {body}}} System.out.println(r); }}")
        }
        1 => {
            let subject = pick(r, &["\"Aa\"", "\"BB\"", "\"C\"", "\"other\""]);
            format!(
                "{{ String s = {subject}; int r = switch (s) {{ case \"Aa\", \"BB\" -> 1; case \"C\" -> {{ int t = 5; yield t * 2; }} default -> -1; }}; System.out.println(r); }}"
            )
        }
        2 => {
            let subject = pick(
                r,
                &[
                    "-1",
                    "0",
                    "1",
                    "2",
                    "100",
                    "1000",
                    "2147483647",
                    "-2147483648",
                    "97",
                ],
            );
            let mut body = String::new();
            for (lab, bit) in [
                ("-1", 1),
                ("0", 2),
                ("1", 4),
                ("100", 8),
                ("1000", 16),
                ("2147483647", 32),
                ("-2147483648", 64),
                ("'a'", 128),
            ] {
                if r.below(3) != 0 {
                    body.push_str(&format!("case {lab}: r += {bit}; "));
                    if r.below(2) == 0 {
                        body.push_str("break; ");
                    }
                }
            }
            body.push_str(if r.below(2) == 0 {
                "default: r += 256;"
            } else {
                ""
            });
            format!("{{ int s = {subject}; int r = 0; switch (s) {{ {body} }} System.out.println(r); }}")
        }
        3 => {
            let subject = pick(r, &["'a'", "'b'", "'z'", "'0'", "' '"]);
            format!(
                "{{ char c = {subject}; String r = switch (c) {{ case 'a', 'e' -> \"vowel\"; case 'z' -> {{ String t = \"last\"; yield t + c; }} default -> {{ if (Character.isDigit(c)) yield \"digit\"; yield \"other\"; }} }}; System.out.println(r); }}"
            )
        }
        _ => {
            let i = r.below(3);
            format!(
                "{{ int r = 0; switch (Color.values()[{i}]) {{ case RED: r += 1; case GREEN: r += 10; break; default: r += 100; case BLUE: r += 1000; }} System.out.println(r); }}"
            )
        }
    }
}

/// Sealed hierarchies of records under pattern matching.
pub fn g_sealed(r: &mut Rng) -> String {
    fn value(r: &mut Rng, depth: u32) -> String {
        match r.below(if depth == 0 { 2 } else { 3 }) {
            0 => format!("new ZsA({})", pick(r, &["-3", "0", "2", "9"])),
            1 => format!(
                "new ZsC({}, {})",
                pick(r, &["1.5", "0.0", "-2.25"]),
                pick(r, &["true", "false"])
            ),
            _ => format!(
                "new ZsB(\"{}\", {})",
                pick(r, &["x", "yy"]),
                value(r, depth - 1)
            ),
        }
    }
    let v = value(r, 2);
    match r.below(6) {
        0 | 1 => p(format!("zdesc({v})")),
        2 => p(format!("zdepth({v})")),
        3 => p(format!("zobj({})", match r.below(7) {
            0 => "null".to_string(),
            1 => "5".to_string(),
            2 => "-5".to_string(),
            3 => "7L".to_string(),
            4 => "\"hello\"".to_string(),
            5 => "\"hi\"".to_string(),
            _ => v,
        })),
        4 => format!("{{ Zs z = {v}; System.out.println(z + \" \" + z.equals({v}) + \" \" + (z.hashCode() == z.hashCode())); }}"),
        _ => format!(
            "{{ Object o = {v}; System.out.println((o instanceof ZsA a ? \"A\" + a.v() : o instanceof ZsB b ? \"B\" + b.s() : \"C\")); }}"
        ),
    }
}

/// What a lambda, an anonymous class and an inner class see of their
/// surroundings.
pub fn g_capture(r: &mut Rng) -> String {
    let k = 2 + r.below(4);
    let off = pick(r, &["0", "10", "-1"]);
    match r.below(9) {
        0 => format!(
            "{{ List<ZSup> l = new ArrayList<>(); for (int i = 0; i < {k}; i++) {{ int j = i * i; l.add(() -> j + {off}); }} \
             StringBuilder sb = new StringBuilder(); for (ZSup s : l) sb.append(s.get()).append(' '); System.out.println(sb); }}"
        ),
        1 => "{ ZOuter o = new ZOuter(2); ZSup s = o.lam(); o.bump(); System.out.println(s.get()); }".to_string(),
        2 => "{ ZOuter o = new ZOuter(2); ZSup a = o.sup(); a.get(); System.out.println(a.get() + \",\" + o.sup().get()); }".to_string(),
        3 => format!(
            "{{ ZOuter o = new ZOuter({k}); ZOuter.In i = o.mk(); ZOuter.In j = o.new In(); o.x = 100; System.out.println(i.sum() + \",\" + j.shadow(3)); }}"
        ),
        4 => format!(
            "{{ int base = {k}; class Loc {{ int f(int q) {{ return base + q; }} }} System.out.println(new Loc().f(3)); }}"
        ),
        5 => "{ int[] c = {0}; Runnable r = () -> c[0]++; r.run(); r.run(); System.out.println(c[0]); }".to_string(),
        6 => format!(
            "{{ String[] ws = {{\"a\", \"b\", \"c\"}}; List<java.util.function.Supplier<String>> l = new ArrayList<>(); for (String w : ws) l.add(() -> w + {k}); \
             StringBuilder sb = new StringBuilder(); for (java.util.function.Supplier<String> s : l) sb.append(s.get()); System.out.println(sb); }}"
        ),
        7 => format!(
            "{{ java.util.function.Function<Integer, java.util.function.Function<Integer, Integer>> add = x -> y -> x * {k} + y; System.out.println(add.apply(3).apply(4)); }}"
        ),
        _ => format!(
            "{{ int n = {k}; ZSup s = new ZSup() {{ public int get() {{ return n * 2; }} }}; java.util.function.Supplier<ZSup> sup = () -> s; System.out.println(sup.get().get()); }}"
        ),
    }
}

/// Erased generic classes and bounded generic methods.
pub fn g_generic(r: &mut Rng) -> String {
    match r.below(10) {
        0 => p("new ZBox<>(5).map(x -> x * 2.5)".to_string()),
        1 => p(format!("zmax({}, {})", pick(r, &["3", "-9"]), pick(r, &["9", "2"]))),
        2 => p("zmax(\"pear\", \"apple\")".to_string()),
        3 => p("zsum(List.of(1, 2.5, 3L))".to_string()),
        4 => p(format!("zrep(\"ab\", {})", r.below(4))),
        5 => p("new ZPair<>(1, \"x\").swap()".to_string()),
        6 => p("new ZBox<>(new ZBox<>(2)).get().get() + 1".to_string()),
        7 => p("zcount(List.of(1, 2, 1, 3, 1), 1) + zcount(Set.of(\"a\"), \"a\")".to_string()),
        8 => p("new ZBox<>(\"s\").map(String::length).map(n -> n + 1).get()".to_string()),
        _ => "{ List<Number> ns = new ArrayList<>(); ns.add(1); ns.add(2.5); ns.add(3L); ns.add(4.5f); \
              System.out.println(ns + \" \" + ns.get(0).longValue() + \" \" + ns.get(1).intValue() + \" \" + zsum(ns)); }"
            .to_string(),
    }
}

/// Structural modification during an enhanced `for`: the iterator is fail-fast,
/// except that the check is skipped when the loop ends first. Only the three
/// kinds javars models with a modification counter are generated; a
/// `LinkedList`, `ArrayDeque` or `PriorityQueue` iterator is fail-fast by its own
/// rules (see BUGS.md).
pub fn g_failfast(r: &mut Rng) -> String {
    let at = r.below(6);
    let coll = *pick(
        r,
        &[
            "new ArrayList<>(List.of(0, 1, 2, 3, 4, 5))",
            "new HashSet<>(List.of(0, 1, 2, 3, 4, 5))",
            "new TreeSet<>(List.of(0, 1, 2, 3, 4, 5))",
        ],
    );
    let action = match r.below(4) {
        0 => "c.remove(v)".to_string(),
        1 => "c.add(v + 100)".to_string(),
        2 => "c.clear()".to_string(),
        _ => "c.contains(v)".to_string(),
    };
    format!(
        "{{ Collection<Integer> c = {coll}; int seen = 0; \
         try {{ for (int v : c) {{ seen++; if (v == {at}) {action}; }} System.out.println(\"done \" + seen + \" \" + c.size()); }} \
         catch (ConcurrentModificationException e) {{ System.out.println(\"CME \" + seen + \" \" + c.size()); }} }}"
    )
}

/// `Collections.unmodifiableX` wrappers: reads follow the target, writes
/// through the wrapper are refused, and an iterator over one cannot `remove`.
pub fn g_view(r: &mut Rng) -> String {
    let coll = match r.below(3) {
        0 => (
            "List<Integer>",
            "new ArrayList<>(List.of(3, 1, 2))",
            "unmodifiableList",
        ),
        1 => (
            "Set<Integer>",
            "new TreeSet<>(List.of(3, 1, 2))",
            "unmodifiableSet",
        ),
        _ => (
            "Collection<Integer>",
            "new LinkedList<>(List.of(3, 1, 2))",
            "unmodifiableCollection",
        ),
    };
    let (ty, init, wrap) = coll;
    let read = pick(
        r,
        &[
            "v + \" \" + v.size()",
            "\"\" + v.contains(2) + v.isEmpty()",
            "v.stream().mapToInt(x -> x).sum() + \"\"",
            "new ArrayList<>(v) + \"\"",
            "(v.isEmpty() ? \"-\" : \"\" + v.iterator().next())",
            // `unmodifiableCollection` does not delegate `equals`, so its
            // answer is identity; only the list and set wrappers are compared.
            "(v instanceof List || v instanceof Set ? v.equals(c) + \",\" + c.equals(v) : \"n/a\")",
        ],
    );
    let write = pick(
        r,
        &[
            "v.add(9)",
            "v.remove(3)",
            "v.clear()",
            "v.addAll(List.of(1))",
            "v.removeIf(x -> x > 1)",
            "v.iterator().remove()",
            "v.retainAll(List.of(1))",
        ],
    );
    let mutate = pick(
        r,
        &[
            "c.add(7)",
            "c.remove(1)",
            "c.clear()",
            "c.addAll(List.of(5, 6))",
        ],
    );
    format!(
        "{{ {ty} c = {init}; {ty} v = Collections.{wrap}(c); String before = {read}; {mutate}; \
         String after = {read}; String w; try {{ {write}; w = \"ok\"; }} \
         catch (UnsupportedOperationException e) {{ w = \"UOE\"; }} \
         System.out.println(before + \" | \" + after + \" | \" + w); }}"
    )
}

/// Calls through a receiver javars cannot type: the arguments still convert to
/// the parameter types of whichever class answers.
pub fn g_erased(r: &mut Rng) -> String {
    let arg = *pick(
        r,
        &["3", "-7", "'a'", "2L", "1.5", "(short) 4", "(byte) -2"],
    );
    match r.below(5) {
        0 => format!(
            "{{ Map<String, Pt> m = new HashMap<>(); m.put(\"k\", new Pt(1, 2)); \
             System.out.println(m.get(\"k\").sum() + \" \" + m.get(\"k\").equals(new Pt(1, 2))); }}"
        ),
        1 => format!(
            "{{ Map<String, Calc> m = new HashMap<>(); m.put(\"k\", mkAdder(1)); \
             System.out.println(m.get(\"k\").of(2, 3)); }}"
        ),
        2 => format!(
            "{{ List<Object> l = new ArrayList<>(); l.add(new Tag(\"t\", 2.5)); Object o = l.get(0); \
             System.out.println(((Tag) o).weight() + \" \" + o); }}"
        ),
        3 => format!(
            "{{ java.util.function.DoubleUnaryOperator f = d -> d * 2; \
             System.out.println(f.applyAsDouble({arg})); }}"
        ),
        _ => format!(
            "{{ java.util.function.Function<Double, String> f = d -> \"d\" + d; \
             java.util.function.BiFunction<Long, Double, Double> g = (a, b) -> a + b; \
             System.out.println(f.apply(2.5) + \" \" + g.apply(3L, 0.5)); }}"
        ),
    }
}

/// An access-ordered `LinkedHashMap`: reads and overwrites move an entry to the
/// end, `containsKey` and iteration do not.
pub fn g_lru(r: &mut Rng) -> String {
    let mut s = String::from(
        "{ Map<Integer, String> m = new LinkedHashMap<>(16, 0.75f, true); \
         for (int i = 0; i < 5; i++) m.put(i, \"v\" + i); ",
    );
    for _ in 0..(2 + r.below(5)) {
        let k = r.below(8);
        let op = match r.below(9) {
            0 => format!("m.get({k});"),
            1 => format!("m.put({k}, \"w{k}\");"),
            2 => format!("m.getOrDefault({k}, \"d\");"),
            3 => format!("m.containsKey({k});"),
            4 => format!("m.remove({k});"),
            5 => format!("m.putIfAbsent({k}, \"p\");"),
            6 => format!("m.merge({k}, \"m\", String::concat);"),
            7 => format!("m.computeIfAbsent({k}, x -> \"c\" + x);"),
            _ => format!("m.replace({k}, \"r\");"),
        };
        s.push_str(&op);
        s.push(' ');
    }
    s.push_str("System.out.println(m + \" \" + m.keySet() + \" \" + m.values()); }");
    s
}
