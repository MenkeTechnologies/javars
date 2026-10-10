//! Round-two regression programs. Each `.java` body below was run on a real
//! JDK (25) and its stdout frozen, so the suite needs no JDK in CI.

use std::process::Command;

fn run(src: &str) -> (String, bool) {
    let dir = std::env::temp_dir().join(format!("javars_round2_{}", fnv1a(src)));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("T.java");
    std::fs::write(&path, src).expect("write program");
    let out = Command::new(env!("CARGO_BIN_EXE_java"))
        .arg(&path)
        .current_dir(&dir)
        .output()
        .expect("spawn java");
    let _ = std::fs::remove_dir_all(&dir);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Class initialization (JLS 12.4.1): a constant read does not initialize, a subclass initializes its superclass first, a use from inside the initializer sees the half-built state, an interface initializes only on a non-constant field read or when an implementor of it with a default method initializes.
#[test]
fn classes_initialize_on_first_use_in_the_order_the_language_fixes() {
    let src = r####"
import java.util.*;
public class T {
    static StringBuilder log = new StringBuilder();
    static int note(String s) { log.append(s).append(';'); return log.length(); }
    static class A {
        static int a = note("A");
        static { note("Ablk"); }
        static final int K = 7;
        static int m = 3;
        static int get() { return a; }
    }
    static class B extends A {
        static int b = note("B");
        B() { note("Bctor"); }
    }
    static class C {
        static final C INST = new C();
        static int cnt = 5;
        int seen;
        C() { seen = cnt; cnt++; note("Cctor"); }
    }
    interface I { int V = note("I"); static int sv() { return 1; } }
    static class D implements I { static int d = note("D"); }
    interface J { int W = note("J"); default int dm() { return 1; } }
    static class K implements J { static int k = note("K"); }
    enum E { P, Q; E() { note("Ector"); } static { note("Eblk"); } }
    static void show(String what, int v) { System.out.println(what + " -> " + v + " | " + log); }
    public static void main(String[] args) {
        System.out.println("start " + log);
        show("A.K", A.K);
        show("B.m", B.m);
        show("new B", new B() != null ? 1 : 0);
        show("C.INST.seen", C.INST.seen);
        show("C.cnt", C.cnt);
        show("D.d", D.d);
        show("I.sv", I.sv());
        show("I.V", I.V);
        show("K.k", K.k);
        show("E.Q", E.Q.ordinal());
    }
}
"####;
    let expected = r####"start 
A.K -> 7 | 
B.m -> 3 | A;Ablk;
new B -> 1 | A;Ablk;B;Bctor;
C.INST.seen -> 0 | A;Ablk;B;Bctor;Cctor;
C.cnt -> 5 | A;Ablk;B;Bctor;Cctor;
D.d -> 23 | A;Ablk;B;Bctor;Cctor;D;
I.sv -> 1 | A;Ablk;B;Bctor;Cctor;D;I;
I.V -> 25 | A;Ablk;B;Bctor;Cctor;D;I;
K.k -> 29 | A;Ablk;B;Bctor;Cctor;D;I;J;K;
E.Q -> 1 | A;Ablk;B;Bctor;Cctor;D;I;J;K;Ector;Ector;Eblk;
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// An enhanced `for` over a collection is a Java iterator: fail-fast on a structural modification, skipped when the loop ends first, `ArrayList` `hasNext` is `cursor != size`, element reads are live.
#[test]
fn an_enhanced_for_is_fail_fast_and_reads_a_list_live() {
    let src = r####"
import java.util.*;
public class T {
    public static void main(String[] args) {
        List<Integer> a = new ArrayList<>(List.of(1, 2, 3, 4));
        try { for (Integer i : a) if (i == 2) a.remove(i); System.out.println("no throw"); } catch (ConcurrentModificationException e) { System.out.println("CME " + a); }
        List<Integer> b = new ArrayList<>(List.of(1, 2, 3, 4));
        for (Integer i : b) if (i == 3) b.remove(i);
        System.out.println(b);
        List<Integer> c = new ArrayList<>(List.of(1, 2, 3));
        for (int i : c) { c.set(2, 99); System.out.print(i + " "); }
        System.out.println(c);
        List<Integer> d = new ArrayList<>(List.of(1, 2, 3));
        try { for (int i : d) { d.remove(0); d.remove(0); } System.out.println("ok " + d); } catch (ConcurrentModificationException e) { System.out.println("CME " + d); }
        Map<String, Integer> m = new HashMap<>(Map.of("a", 1, "b", 2, "c", 3));
        try { for (String k : m.keySet()) m.put(k + "x", 0); System.out.println("no throw"); } catch (ConcurrentModificationException e) { System.out.println("CME " + m.size()); }
        Map<String, Integer> t = new TreeMap<>(Map.of("a", 1, "b", 2, "c", 3));
        try { for (int v : t.values()) if (v == 1) t.remove("a"); System.out.println("no throw"); } catch (ConcurrentModificationException e) { System.out.println("CME " + t); }
        Map<String, Integer> t2 = new TreeMap<>(Map.of("a", 1, "b", 2, "c", 3));
        for (Map.Entry<String, Integer> e : t2.entrySet()) if (e.getKey().equals("c")) t2.remove("c");
        System.out.println(t2);
        Set<Integer> s = new HashSet<>(Set.of(1, 2, 3));
        try { for (int v : s) s.remove(v); System.out.println("no throw"); } catch (ConcurrentModificationException e) { System.out.println("CME " + s.size()); }
        Collection<Integer> col = new HashSet<>(List.of(5));
        col.remove(5);
        System.out.println(col.size());
        Map<Integer, Integer> memo = new HashMap<>();
        try { System.out.println(fib(20, memo)); } catch (ConcurrentModificationException e) { System.out.println("CME memo"); }
    }
    static int fib(int n, Map<Integer, Integer> memo) {
        if (n < 2) return n;
        return memo.computeIfAbsent(n, k -> fib(k - 1, memo) + fib(k - 2, memo));
    }
}
"####;
    let expected = r####"CME [1, 3, 4]
[1, 2, 4]
1 2 99 [1, 2, 99]
ok [3]
CME 4
CME {b=2, c=3}
{a=1, b=2}
CME 2
0
CME memo
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// JLS 5.6, 15.25, 15.26.2: compound assignment on an `int` wraps a `long` result, shifts mask to the promoted width, a `float` operand makes the operation `float`, a conditional with a `null` `Integer` unboxes, and a floating cast to a narrow integral type goes through `int`.
#[test]
fn compound_assignment_conditionals_and_casts_follow_the_promotion_rules() {
    let src = r####"
public class T {
    public static void main(String[] args) {
        int I = 2000000000; long L = 5000000000L; byte B = 100; short S = 30000; char C = 'x'; float F = 1.25f; double D = 0.1;
        I += L; System.out.println(I);
        I = 1; I *= L; System.out.println(I);
        I = 2000000000; I <<= 7L; System.out.println(I);
        S = 1; S <<= 33; System.out.println(S);
        S = 30000; S >>= 33; System.out.println(S);
        B = 1; B <<= 33; System.out.println(B);
        B = 100; B >>>= 33; System.out.println(B);
        C = 'x'; C >>>= 33; System.out.println((int) C);
        L = 5000000000L; L += F; System.out.println(L);
        I = 16777217; I += 0.0f; System.out.println(I);
        F = 1.25f; D = 0.1; F %= D; System.out.println(F);
        F = 1.25f; F += 1e-9; System.out.println(F);
        S = 5; S -= 0.5f; System.out.println(S);
        B = 5; B += 1e10; System.out.println(B);
        System.out.println((short) -1e10 + " " + (byte) 300.7 + " " + (char) 65.9 + " " + (byte) Double.NaN + " " + (short) 1e10f);
        System.out.println((int) 3.99e10 + " " + (long) 1e30 + " " + (int) -1e30);
        Object o1 = true ? 0 : (byte) 1;
        Object o2 = true ? (short) 0 : 0;
        Object o3 = true ? (byte) 1 : (short) 2;
        Object o4 = true ? 'a' : 98;
        Object o5 = true ? 'a' : I;
        Object o6 = true ? 1 : 2.0;
        Object o7 = true ? (Integer) 1 : (Long) 2L;
        Object o8 = true ? (Character) 'q' : 1;
        System.out.println(o1.getClass().getSimpleName() + o2.getClass().getSimpleName() + o3.getClass().getSimpleName() + o4.getClass().getSimpleName() + o5.getClass().getSimpleName() + o6.getClass().getSimpleName() + o7.getClass().getSimpleName() + o8.getClass().getSimpleName());
        Integer n = null;
        try { int v = true ? n : 0; System.out.println(v); } catch (NullPointerException e) { System.out.println("npe1"); }
        try { Integer v = true ? n : Integer.valueOf(0); System.out.println(v); } catch (NullPointerException e) { System.out.println("npe2"); }
        try { Object v = true ? n : "s"; System.out.println(v); } catch (NullPointerException e) { System.out.println("npe3"); }
        try { switch (n) { case 1: break; default: break; } } catch (NullPointerException e) { System.out.println("npe4"); }
        String s = null;
        try { switch (s) { case "a": break; default: break; } } catch (NullPointerException e) { System.out.println("npe5"); }
        try { int[] a = new int[2]; a[n] = 1; } catch (NullPointerException e) { System.out.println("npe6"); }
        System.out.println(switch (s) { case null -> "null case"; default -> "other"; });
    }
}
"####;
    let expected = r####"-1589934592
705032704
-1698037760
2
15000
2
50
60
5000000000
16777216
0.05
1.25
4
-1
0 44 A 0 -1
2147483647 9223372036854775807 -2147483648
ByteShortShortCharacterIntegerDoubleLongCharacter
npe1
null
null
npe4
npe5
npe6
null case
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// Class values and their string form, a lambda receiving a default method, octal and `\b\f\s` escapes, Formatter check order, JDK supertypes in overload resolution, `Throwable.toString` through `getLocalizedMessage`, and the deprecated box constructors.
#[test]
fn class_values_lambdas_escapes_format_and_overloads() {
    let src = r####"
import java.util.*;
import java.util.function.*;
public class T {
    interface Animal { String name(); default String greet() { return "I am " + name(); } static Animal of(String n) { return () -> n; } }
    enum Op { ADD { int f() { return 1; } }, SUB; int f() { return 0; } }
    static class A {}
    interface Marker {}
    static class P implements Comparable<P> {
        final int a; P(int a) { this.a = a; }
        public int compareTo(P o) { return Integer.compare(a, o.a); }
        public String toString() { return "P" + a; }
    }
    static class ZRt extends RuntimeException {
        ZRt(String m) { super(m); }
        @Override public String getMessage() { return "Z:" + super.getMessage(); }
    }
    static String od(Object o) { return "Object"; }
    static String od(String o) { return "String"; }
    static String od(CharSequence o) { return "CharSequence"; }
    public static void main(String[] args) {
        Animal x = new Animal() { public String name() { return "anon"; } public String greet() { return "hi " + Animal.super.greet(); } };
        System.out.println(Animal.of("rex").greet() + " / " + x.greet());
        A a = new A(), a2 = new A();
        System.out.println((a.getClass() == a2.getClass()) + " " + (a.getClass() == A.class));
        System.out.println(a.getClass());
        System.out.println(Marker.class + " " + int.class + " " + String.class + " " + int[].class + " " + List.class);
        System.out.println(Op.ADD.getClass().getName() + " " + Op.ADD.getDeclaringClass() + " " + Op.SUB.getClass().getName());
        System.out.println("T\101B\60\b\f\s|".length() + " [" + "a\sb" + "] " + (int) "\7".charAt(0) + " " + (int) "\377".charAt(0) + " " + "\0".length());
        System.out.println(String.format("%C|%#.0e|%+,(12.0g|%-8.3S|", 'x', 12345.678, 123456789.0, "hello"));
        for (String f : new String[]{"%,.2x", "%#+12.5X", "%-0,.2b", "%-+ S", "%+x", "%#d", "%,o"}) {
            try { System.out.println(String.format(f, f.contains("b") ? (Object) false : f.contains("S") ? (Object) 0 : (Object) 255)); }
            catch (IllegalFormatException e) { System.out.println(e.getClass().getSimpleName()); }
        }
        List<P> ps = List.of(new P(3), new P(9), new P(1));
        System.out.println("max=" + ps.stream().max(Comparator.naturalOrder()).get() + ps.stream().map(p -> p.a).max(Integer::compare).get());
        System.out.println(new ZRt("m") + " | " + new ZRt(null) + " | " + new ZRt("m").getLocalizedMessage());
        System.out.println(od(new StringBuilder("x")) + od("s") + od((Object) "s") + od(5) + od(null));
        Integer i1 = new Integer(127), i2 = new Integer(127);
        System.out.println((i1 == i2) + " " + i1.equals(i2) + " " + (Integer.valueOf(127) == Integer.valueOf(127)));
    }
}
"####;
    let expected = r####"I am rex / hi I am anon
true true
class T$A
interface T$Marker int class java.lang.String class [I interface java.util.List
T$Op$1 class T$Op T$Op
8 [a b] 7 255 1
X|1.e+04|      +1e+08|HEL     |
IllegalFormatPrecisionException
IllegalFormatPrecisionException
MissingFormatWidthException
MissingFormatWidthException
FormatFlagsConversionMismatchException
FormatFlagsConversionMismatchException
FormatFlagsConversionMismatchException
max=P99
T$ZRt: Z:m | T$ZRt: Z:null | Z:m
CharSequenceStringObjectObjectString
false true true
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}
