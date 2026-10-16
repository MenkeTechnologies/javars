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

/// `Collections.unmodifiableX` is a read-only view: writes through the target show, writes through the wrapper are `UnsupportedOperationException`, and an immutable collection's iterator refuses `remove()` before checking its cursor.
#[test]
fn unmodifiable_wrappers_show_their_target_and_refuse_writes() {
    let src = r####"
import java.util.*;
import java.util.stream.*;
public class T {
    static String attempt(Runnable r) { try { r.run(); return "ok"; } catch (UnsupportedOperationException e) { return "UOE"; } }
    public static void main(String[] args) {
        List<Integer> src = new ArrayList<>(List.of(3, 1, 2));
        List<Integer> view = Collections.unmodifiableList(src);
        System.out.println(view + " " + view.size() + " " + view.get(1) + " " + view.contains(2) + " " + view.indexOf(2));
        src.add(9); src.set(0, 7);
        System.out.println(view + " " + view.size() + " " + (view != src) + " " + view.equals(src) + " " + src.equals(view) + " " + view.hashCode());
        System.out.println(attempt(() -> view.add(1)) + attempt(() -> view.remove(0)) + attempt(() -> view.set(0, 1)) + attempt(() -> view.clear()) + attempt(() -> view.sort(null)) + attempt(() -> view.iterator().remove()) + attempt(() -> view.addAll(List.of(1))));
        int sum = 0; for (int v : view) sum += v; System.out.println(sum + " " + view.stream().map(x -> x * 2).collect(Collectors.toList()) + new ArrayList<>(view) + Collections.max(view));
        Set<String> s = new TreeSet<>(Set.of("b", "a"));
        Set<String> sv = Collections.unmodifiableSet(s);
        s.add("c");
        System.out.println(sv + " " + sv.contains("c") + " " + attempt(() -> sv.add("d")) + attempt(() -> sv.remove("a")) + sv.size());
        Map<String, Integer> m = new LinkedHashMap<>();
        Map<String, Integer> mv = Collections.unmodifiableMap(m);
        m.put("x", 1); m.put("y", 2);
        System.out.println(mv + " " + mv.get("y") + " " + mv.keySet() + " " + mv.values() + " " + mv.containsKey("x") + " " + mv.size() + " " + mv.getOrDefault("z", -1));
        System.out.println(attempt(() -> mv.put("z", 3)) + attempt(() -> mv.remove("x")) + attempt(() -> mv.clear()) + attempt(() -> mv.merge("x", 1, Integer::sum)) + attempt(() -> mv.putIfAbsent("q", 1)));
        for (Map.Entry<String, Integer> e : mv.entrySet()) System.out.print(e.getKey() + e.getValue());
        System.out.println();
        Collection<Integer> c = Collections.unmodifiableCollection(src);
        System.out.println(c + " " + c.size() + attempt(() -> c.add(1)));
        List<Integer> nested = Collections.unmodifiableList(Collections.unmodifiableList(src));
        src.add(100);
        System.out.println(nested.size() + " " + nested.get(nested.size() - 1));
        List<Integer> empty = Collections.emptyList();
        System.out.println(empty + " " + attempt(() -> empty.add(1)) + Collections.singletonList(5) + Collections.synchronizedList(new ArrayList<>(List.of(1))));
        try { Collections.unmodifiableList(null); } catch (NullPointerException e) { System.out.println("npe"); }
    }
}
"####;
    let expected = r####"[3, 1, 2] 3 1 true 2
[7, 1, 2, 9] 4 true true true 1133090
UOEUOEUOEUOEUOEUOEUOE
19 [14, 2, 4, 18][7, 1, 2, 9]9
[a, b, c] true UOEUOE3
{x=1, y=2} 2 [x, y] [1, 2] true 2 -1
UOEUOEUOEUOEUOE
x1y2
[7, 1, 2, 9] 4UOE
5 100
[] UOE[5][1]
npe
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// A call through an erased receiver converts an `int` argument for a `double` parameter, an anonymous class that names its own `this` or recurses stays a class, a user `Iterable` inherits `forEach`, private interface methods are statically bound, and the primitive optionals have factories.
#[test]
fn erased_calls_widen_anonymous_classes_keep_this_and_iterable_inherits_for_each() {
    let src = r####"
import java.util.*;
import java.util.function.*;
public class T {
    interface Account { void deposit(double amt); double balance(); }
    static class Acct implements Account {
        double bal;
        public void deposit(double amt) { bal += amt; System.out.println("deposit " + amt); }
        public double balance() { return bal; }
    }
    interface WithPriv { private int helper() { return 41; } default int val() { return helper() + 1; } }
    static class Bag implements Iterable<String> {
        final List<String> xs = new ArrayList<>(List.of("a", "b", "c"));
        public Iterator<String> iterator() { return xs.iterator(); }
    }
    static class Tree<E extends Comparable<E>> implements Iterable<E> {
        private final TreeSet<E> items = new TreeSet<>();
        void add(E e) { items.add(e); }
        public Iterator<E> iterator() { return items.iterator(); }
    }
    static float half(float f) { return f / 2; }
    static double twice(double d) { return d * 2; }
    public static void main(String[] args) {
        Map<String, Account> accts = new HashMap<>();
        accts.put("a", new Acct());
        accts.get("a").deposit(5);
        accts.get("a").deposit(-5);
        Object o = new Acct();
        ((Account) o).deposit(2);
        System.out.println(accts.get("a").balance() + " " + new WithPriv() {}.val());
        BiFunction<Integer, Integer, Integer> gcd = new BiFunction<>() {
            public Integer apply(Integer a, Integer b) { return b == 0 ? a : this.apply(b, a % b); }
        };
        Function<Integer, Integer> fact = new Function<>() {
            public Integer apply(Integer n) { return n <= 1 ? 1 : n * apply(n - 1); }
        };
        System.out.println(gcd.apply(84, 36) + " " + fact.apply(6));
        Bag bag = new Bag();
        List<String> got = new ArrayList<>();
        bag.forEach(got::add);
        bag.forEach(x -> System.out.print(x.toUpperCase()));
        System.out.println(" " + got);
        Tree<Integer> t = new Tree<>();
        for (int v : new int[]{5, 1, 3}) t.add(v);
        StringBuilder sb = new StringBuilder();
        t.forEach(v -> sb.append(v).append(';'));
        System.out.println(sb);
        System.out.println(OptionalInt.of(3) + " " + OptionalDouble.of(2) + " " + OptionalLong.empty() + " " + OptionalInt.empty().isPresent() + " " + OptionalInt.of(7).getAsInt());
        List<Integer> l = new ArrayList<>(List.of(1, 2, 3));
        System.out.println(l.addAll(1, List.of(8, 9)) + " " + l + " " + l.addAll(0, List.of()));
        try { l.addAll(9, List.of(1)); } catch (IndexOutOfBoundsException e) { System.out.println(e.getMessage()); }
        System.out.println(half(5) + " " + twice(3) + " " + (float) twice(0.1) + " " + (double) half(0.1f));
    }
}
"####;
    let expected = r####"deposit 5.0
deposit -5.0
deposit 2.0
0.0 42
12 720
ABC [a, b, c]
1;3;5;
OptionalInt[3] OptionalDouble[2.0] OptionalLong.empty false 7
true [1, 8, 9, 2, 3] false
Index: 9, Size: 5
2.5 6.0 0.2 0.05000000074505806
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// A small bank: interfaces, an abstract base, a checked exception, `String.format`, streams over an erased map, and an unmodifiable history view.
#[test]
fn a_bank_program_with_exceptions_interfaces_and_unmodifiable_history() {
    let src = r####"
import java.util.*;
import java.util.stream.*;

public class T {
    interface Account { String id(); double balance(); void deposit(double amt); void withdraw(double amt) throws InsufficientFunds; default String summary() { return String.format("%s[%s: %.2f]", getClass().getSimpleName(), id(), balance()); } }
    static class InsufficientFunds extends Exception {
        final double shortBy;
        InsufficientFunds(String id, double shortBy) { super("Insufficient funds in " + id + ": short by " + String.format("%.2f", shortBy)); this.shortBy = shortBy; }
    }
    static abstract class Base implements Account {
        private final String id; protected double bal; private final List<String> log = new ArrayList<>();
        Base(String id, double open) { this.id = id; this.bal = open; log("open " + open); }
        public String id() { return id; }
        public double balance() { return bal; }
        protected void log(String s) { log.add(s); }
        List<String> history() { return Collections.unmodifiableList(log); }
        public void deposit(double amt) { if (amt <= 0) throw new IllegalArgumentException("Deposit must be positive: " + amt); bal += amt; log("dep " + amt); }
        public void withdraw(double amt) throws InsufficientFunds { if (amt > available()) throw new InsufficientFunds(id, amt - available()); bal -= amt; log("wd " + amt); }
        abstract double available();
    }
    static class Checking extends Base { final double overdraft; Checking(String id, double open, double od) { super(id, open); overdraft = od; } double available() { return bal + overdraft; } }
    static class Savings extends Base { final double rate; Savings(String id, double open, double rate) { super(id, open); this.rate = rate; } double available() { return bal; } void accrue() { double i = bal * rate / 12; bal += i; log("int " + String.format("%.4f", i)); } }
    public static void main(String[] args) {
        Map<String, Account> accts = new LinkedHashMap<>();
        accts.put("C1", new Checking("C1", 100, 50));
        accts.put("S1", new Savings("S1", 1000, 0.05));
        try {
            accts.get("C1").withdraw(120);
            accts.get("C1").withdraw(50);
        } catch (InsufficientFunds e) {
            System.out.println(e.getMessage() + " / " + e.shortBy);
        }
        try { accts.get("S1").deposit(-5); } catch (IllegalArgumentException e) { System.out.println("IAE " + e.getMessage()); }
        ((Savings) accts.get("S1")).accrue();
        ((Savings) accts.get("S1")).accrue();
        for (Account a : accts.values()) System.out.println(a.summary());
        double total = accts.values().stream().mapToDouble(Account::balance).sum();
        System.out.printf("total=%.3f avg=%8.2f max=%s%n", total, total / accts.size(), accts.values().stream().max(Comparator.comparingDouble(Account::balance)).get().id());
        System.out.println(((Base) accts.get("S1")).history());
    }
}
"####;
    let expected = r####"Insufficient funds in C1: short by 20.00 / 20.0
IAE Deposit must be positive: -5.0
Checking[C1: -20.00]
Savings[S1: 1008.35]
total=988.351 avg=  494.18 max=S1
[open 1000.0, int 4.1667, int 4.1840]
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// A generic binary search tree with an anonymous iterator, a generic linked stack, composed functions and an anonymous recursive `BiFunction`.
#[test]
fn generic_bst_with_iterator_and_functional_helpers() {
    let src = r####"
import java.util.*;
import java.util.function.*;

public class T {
    static class Node<T extends Comparable<T>> { T val; Node<T> left, right; Node(T v) { val = v; } }
    static class Bst<T extends Comparable<T>> implements Iterable<T> {
        Node<T> root; int size;
        boolean add(T v) { if (root == null) { root = new Node<>(v); size++; return true; } Node<T> c = root; while (true) { int cmp = v.compareTo(c.val); if (cmp == 0) return false; Node<T> nx = cmp < 0 ? c.left : c.right; if (nx == null) { if (cmp < 0) c.left = new Node<>(v); else c.right = new Node<>(v); size++; return true; } c = nx; } }
        int height(Node<T> n) { return n == null ? 0 : 1 + Math.max(height(n.left), height(n.right)); }
        public Iterator<T> iterator() { Deque<Node<T>> st = new ArrayDeque<>(); for (Node<T> c = root; c != null; c = c.left) st.push(c); return new Iterator<T>() { public boolean hasNext() { return !st.isEmpty(); } public T next() { Node<T> n = st.pop(); for (Node<T> c = n.right; c != null; c = c.left) st.push(c); return n.val; } }; }
    }
    static class LinkedStack<E> { private static class N<E> { E v; N<E> next; N(E v, N<E> n) { this.v = v; next = n; } } private N<E> head; private int n; void push(E e) { head = new N<>(e, head); n++; } E pop() { if (head == null) throw new RuntimeException("empty"); E v = head.v; head = head.next; n--; return v; } boolean isEmpty() { return head == null; } int size() { return n; } }
    static <A, B, C> Function<A, C> compose(Function<A, B> f, Function<B, C> g) { return a -> g.apply(f.apply(a)); }
    static <T> void bubble(T[] a, Comparator<? super T> c) { for (int i = 0; i < a.length; i++) for (int j = 0; j + 1 < a.length - i; j++) if (c.compare(a[j], a[j + 1]) > 0) { T t = a[j]; a[j] = a[j + 1]; a[j + 1] = t; } }
    public static void main(String[] args) {
        Bst<Integer> t = new Bst<>(); for (int v : new int[]{50, 30, 70, 20, 40, 60, 80, 30, 65}) t.add(v);
        StringBuilder sb = new StringBuilder(); for (int v : t) sb.append(v).append(' '); System.out.println(sb + "size=" + t.size + " h=" + t.height(t.root));
        Bst<String> ts = new Bst<>(); for (String s : "kiwi apple mango banana cherry apple".split(" ")) ts.add(s); List<String> l = new ArrayList<>(); ts.forEach(l::add); System.out.println(l);
        LinkedStack<String> st = new LinkedStack<>(); for (String s : "a b c".split(" ")) st.push(s); System.out.println(st.pop() + st.pop() + st.size()); try { st.pop(); st.pop(); } catch (RuntimeException e) { System.out.println(e.getMessage() + st.isEmpty()); }
        System.out.println(compose((String s) -> s.length(), (Integer n) -> n * n).apply("hello") + " " + T.<Integer, Integer, String>compose(x -> x + 1, x -> "v" + x).apply(4));
        String[] names = {"delta", "Alpha", "charlie", "Bravo"}; bubble(names, String.CASE_INSENSITIVE_ORDER); System.out.println(Arrays.toString(names)); Integer[] nums = {5, 2, 9, 1}; bubble(nums, Comparator.reverseOrder()); System.out.println(Arrays.toString(nums));
        BiFunction<Integer, Integer, Integer> gcd = new BiFunction<>() { public Integer apply(Integer a, Integer b) { return b == 0 ? a : this.apply(b, a % b); } }; System.out.println(gcd.apply(84, 36));
        Supplier<Supplier<String>> ss = () -> () -> "deep"; System.out.println(ss.get().get());
    }
}
"####;
    let expected = r####"20 30 40 50 60 65 70 80 size=8 h=4
[apple, banana, cherry, kiwi, mango]
cb1
emptytrue
25 v5
[Alpha, Bravo, charlie, delta]
[9, 5, 2, 1]
12
deep
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// An access-ordered `LinkedHashMap` moves an entry to the end on `get`/`put`/`getOrDefault`/`merge`/`compute*`; `unmodifiableCollection` is a `Collection` and no narrower; a `char` beside a `String` in a conditional is a `Character`.
#[test]
fn access_ordered_linked_hash_maps_reorder_on_read_and_collection_views_are_not_lists() {
    let src = r####"
import java.util.*;
public class T {
    public static void main(String[] args) {
        Map<String, Integer> m = new LinkedHashMap<>(16, 0.75f, true);
        m.put("a", 1); m.put("b", 2); m.put("c", 3);
        m.get("a");
        System.out.println(m);
        m.put("b", 20);
        System.out.println(m + " " + m.keySet());
        m.getOrDefault("c", 0); m.getOrDefault("zz", 0); m.containsKey("a");
        System.out.println(m);
        m.putIfAbsent("a", 9); m.merge("b", 1, Integer::sum); m.computeIfAbsent("z", k -> 26); m.compute("c", (k, v) -> v + 100);
        System.out.println(m + " " + m.size());
        String eldest = m.keySet().iterator().next();
        m.remove(eldest);
        System.out.println(eldest + " " + m + " " + (m instanceof LinkedHashMap));
        Map<String, Integer> plain = new LinkedHashMap<>(16, 0.75f, false);
        plain.put("x", 1); plain.put("y", 2); plain.get("x");
        System.out.println(plain);
        Collection<Integer> base = new ArrayList<>(List.of(1, 2));
        Collection<Integer> view = Collections.unmodifiableCollection(base);
        List<Integer> lview = Collections.unmodifiableList(new ArrayList<>(base));
        System.out.println((view instanceof List) + " " + (view instanceof Collection) + " " + (lview instanceof List) + " " + (Collections.unmodifiableSet(new TreeSet<>(base)) instanceof Set));
        Object o = null;
        String s = "hi";
        for (Object x : new Object[]{null, "str", 5}) {
            System.out.print((x == null ? "null" : s.charAt(0)) + " " + (x == null ? s.charAt(1) : "str") + " ");
        }
        System.out.println();
    }
}
"####;
    let expected = r####"{b=2, c=3, a=1}
{c=3, a=1, b=20} [c, a, b]
{a=1, b=20, c=3}
{a=1, b=21, z=26, c=103} 4
a {b=21, z=26, c=103} true
{x=1, y=2}
false true true true
null i h str h str 
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// `Float.toString` decides between two nearest candidates on the exact expansion (ties to even), and `distinct()` passes an element the first time it is seen so a short-circuiting terminal stops the upstream early while `sorted()` stays a barrier.
#[test]
fn float_to_string_breaks_ties_to_even_and_distinct_streams() {
    let src = r####"
import java.util.*;
import java.util.stream.*;
public class T {
    public static void main(String[] args) {
        // Float.toString picks the nearest candidate, ties to even, on the exact expansion.
        for (int bits : new int[]{0xB9800000, 0x39800000, 0x3F800001, 0x4B800001, 0x33800000, 0x00000001, 0x7F7FFFFF, 0x3DCCCCCD}) {
            float f = Float.intBitsToFloat(bits);
            System.out.println(f + " " + (double) f);
        }
        System.out.println(2.44140625E-4f + " " + (0.1f + 0.2f) + " " + 16777217.0f * 0.2f + " " + 3.4e38f * 10);
        // distinct() is streaming: a short-circuiting terminal stops the upstream early.
        StringBuilder sb = new StringBuilder();
        boolean any = Stream.of(1, 0, 0, 0).peek(x -> sb.append('<').append(x).append('>')).distinct().anyMatch(x -> x > 0);
        System.out.println(any + " " + sb);
        sb.setLength(0);
        List<Integer> l = Stream.of(3, 1, 3, 2, 1).peek(x -> sb.append(x)).distinct().peek(x -> sb.append('!')).limit(2).collect(Collectors.toList());
        System.out.println(l + " " + sb);
        sb.setLength(0);
        Object first = Stream.of(5, 3, 8).peek(x -> sb.append(x)).sorted().findFirst().get();
        System.out.println(first + " " + sb);
    }
}
"####;
    let expected = r####"-2.4414062E-4 -2.44140625E-4
2.4414062E-4 2.44140625E-4
1.0000001 1.0000001192092896
1.6777218E7 1.6777218E7
5.9604645E-8 5.960464477539063E-8
1.4E-45 1.401298464324817E-45
3.4028235E38 3.4028234663852886E38
0.1 0.10000000149011612
2.4414062E-4 0.3 3355443.2 Infinity
true <1>
[3, 1] 3!1!
3 538
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// An `Optional` renders its value through the value's own `toString()` on every surface (a bare `println`, a collection or map element, `toString()`, `String.valueOf`, `%s`), and `equals`/`hashCode` are `Objects.equals`/`Objects.hashCode` of the value, reaching a user or record body. Expected output from openjdk 21.0.12.1.
#[test]
fn optional_renders_compares_and_hashes_its_value_through_the_value_methods() {
    let src = r####"
import java.util.*;
import java.util.stream.*;
public class T {
    record Emp(String name, String dept, int sal) {}
    static class Tag { final String t; Tag(String t) { this.t = t; } public String toString() { return "<" + t + ">"; } }
    static class K { final int v; K(int v) { this.v = v; }
        public boolean equals(Object o) { return o instanceof K k && k.v == v; }
        public int hashCode() { return 31 * v; } }
    public static void main(String[] args) {
        // An Optional renders, compares and hashes its value through the value's own methods.
        List<Emp> emps = List.of(new Emp("ann", "eng", 120), new Emp("bob", "eng", 99), new Emp("cy", "ops", 80));
        Map<String, Optional<Emp>> top = emps.stream().collect(Collectors.groupingBy(Emp::dept, TreeMap::new, Collectors.maxBy(Comparator.comparingInt(Emp::sal))));
        System.out.println(top);
        Optional<Tag> t = Optional.of(new Tag("x"));
        System.out.println(t + " " + List.of(t, Optional.empty()));
        System.out.println(String.valueOf(t) + " " + t.toString() + " " + String.format("%s", t));
        System.out.println(Optional.of(List.of(new Tag("y"))) + " " + Optional.of(Optional.of(new Tag("z"))));
        System.out.println(OptionalInt.of(3) + " " + OptionalDouble.of(2.5) + " " + Optional.of(1.5f) + " " + Optional.of('c'));
        System.out.println(Optional.of(new K(2)).equals(Optional.of(new K(2))) + " " + Optional.of(new K(2)).hashCode());
        System.out.println(Optional.of(new Emp("a", "b", 1)).equals(Optional.of(new Emp("a", "b", 1))) + " " + Optional.of(List.of(new K(1))).equals(Optional.of(List.of(new K(1)))) + " " + Optional.of(List.of(1, 2)).hashCode() + " " + Optional.empty().hashCode());
    }
}"####;
    let expected = r####"{eng=Optional[Emp[name=ann, dept=eng, sal=120]], ops=Optional[Emp[name=cy, dept=ops, sal=80]]}
Optional[<x>] [Optional[<x>], Optional.empty]
Optional[<x>] Optional[<x>] Optional[<x>]
Optional[[<y>]] Optional[Optional[<z>]]
OptionalInt[3] OptionalDouble[2.5] Optional[1.5] Optional[c]
true 62
true true 994 0
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// `Iterator.forEachRemaining` on the host iterators (list, priority queue, entry set, descending deque) walks from the cursor, keeps `remove()` and comodification semantics, and stops on a thrown exception; a user `Iterator` inherits the default body with its receiver evaluated once. Expected output from openjdk 21.0.12.1.
#[test]
fn iterator_for_each_remaining_on_host_and_user_iterators() {
    let src = r####"
import java.util.*;
public class T {
    static class Count implements Iterator<Integer> {
        int i = 0;
        public boolean hasNext() { return i < 3; }
        public Integer next() { return i++; }
    }
    static int evals = 0;
    static Count fresh() { evals++; return new Count(); }
    public static void main(String[] args) {
        // A host iterator: the walk starts at the cursor and leaves `remove()` pointing at the last element.
        List<Integer> l = new ArrayList<>(List.of(1, 2, 3, 4));
        Iterator<Integer> it = l.iterator();
        it.next();
        it.forEachRemaining(x -> System.out.print(x + " "));
        System.out.println(it.hasNext());
        it.remove();
        System.out.println(l);
        StringBuilder sb = new StringBuilder();
        new PriorityQueue<>(List.of(5, 1, 3)).iterator().forEachRemaining(sb::append);
        new TreeMap<>(Map.of("a", 1, "b", 2)).entrySet().iterator().forEachRemaining(e -> sb.append(e.getKey()).append(e.getValue()));
        new ArrayDeque<>(List.of("p", "q")).descendingIterator().forEachRemaining(sb::append);
        System.out.println(sb);
        try { l.iterator().forEachRemaining(x -> l.add(x)); } catch (ConcurrentModificationException e) { System.out.println("cme " + l); }
        try { l.iterator().forEachRemaining(null); } catch (NullPointerException e) { System.out.println("npe"); }
        try { l.iterator().forEachRemaining(x -> { if (x == 2) throw new IllegalStateException("at " + x); System.out.println("saw " + x); }); }
        catch (IllegalStateException e) { System.out.println(e.getMessage()); }
        // A user Iterator inherits the default method; its receiver is evaluated once.
        fresh().forEachRemaining(x -> System.out.print(x));
        System.out.println(" " + evals);
        Count c = new Count();
        c.next();
        c.forEachRemaining(x -> System.out.print(x));
        System.out.println(" " + c.hasNext());
        try { new Count().forEachRemaining(null); } catch (NullPointerException e) { System.out.println("npe"); }
    }
}"####;
    let expected = r####"2 3 4 false
[1, 2, 3]
153a1b2qp
cme [1, 2, 3, 1]
npe
saw 1
at 2
012 1
12 false
npe
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// Static JDK members with a single public arity (`Collections::unmodifiableList`, `Collections::nCopies`, `Boolean::logicalOr`, `Integer::decode`, …) and a bound `sb::append` are method-reference targets. Expected output from openjdk 21.0.12.1.
#[test]
fn jdk_static_and_builder_method_references() {
    let src = r####"
import java.util.*;
import java.util.function.*;
import java.util.stream.*;
public class T {
    public static void main(String[] args) {
        // Static JDK members with one public arity are method-reference targets.
        List<String> names = Stream.of("b", "a").collect(Collectors.collectingAndThen(Collectors.toList(), Collections::unmodifiableList));
        System.out.println(names);
        try { names.add("c"); } catch (UnsupportedOperationException e) { System.out.println("uoe"); }
        Set<Integer> s = Stream.of(3, 1, 3).collect(Collectors.collectingAndThen(Collectors.toCollection(TreeSet::new), Collections::unmodifiableSet));
        System.out.println(s);
        Function<Map<String, Integer>, Map<String, Integer>> ro = Collections::unmodifiableMap;
        System.out.println(ro.apply(new TreeMap<>(Map.of("k", 1))));
        Supplier<List<Integer>> none = Collections::emptyList;
        System.out.println(none.get() + " " + none.get().size());
        Function<String, List<String>> one = Collections::singletonList;
        System.out.println(Stream.of("x", "y").map(one).toList());
        BiFunction<Integer, String, List<String>> rep = Collections::nCopies;
        System.out.println(rep.apply(3, "ab"));
        BiFunction<Collection<?>, Object, Integer> freq = Collections::frequency;
        System.out.println(freq.apply(List.of(1, 2, 1), 1));
        Consumer<List<Integer>> rev = Collections::reverse;
        List<Integer> l = new ArrayList<>(List.of(1, 2, 3));
        rev.accept(l);
        System.out.println(l);
        BinaryOperator<Boolean> or = Boolean::logicalOr;
        System.out.println(Stream.of(false, true, false).reduce(false, or) + " " + Stream.of(true, true).reduce(true, Boolean::logicalAnd) + " " + Stream.of(true, true, true).reduce(false, Boolean::logicalXor));
        System.out.println(Stream.of("1.5", "2.25").map(Float::parseFloat).toList() + " " + Stream.of("0x10", "-010", "7").map(Integer::decode).toList());
        System.out.println(DoubleStream.of(1.0, Double.NaN, 1 / 0.0).boxed().filter(Double::isFinite).count());
        // A bound reference to a StringBuilder variable.
        StringBuilder sb = new StringBuilder();
        List.of(1, 2, 3).forEach(sb::append);
        Stream.of("x", "y").forEach(sb::append);
        System.out.println(sb);
    }
}"####;
    let expected = r####"[b, a]
uoe
[1, 3]
{k=1}
[] 0
[[x], [y]]
[ab, ab, ab]
2
[3, 2, 1]
true true true
[1.5, 2.25] [16, -8, 7]
1
123xy
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// An `Optional` hash key is found by its contents and buckets by the held value's hash, so a `HashSet`/`HashMap` of them iterates in the JDK's order; `==` stays reference identity and `Optional` never equals `OptionalInt`. Expected output from openjdk 21.0.12.1.
#[test]
fn optional_hash_keys_are_found_by_contents_and_iterate_in_jdk_order() {
    let src = r####"
import java.util.*;
public class T {
    public static void main(String[] args) {
        // An Optional key is found by its contents and buckets by its value's hash.
        Set<Optional<String>> s = new HashSet<>();
        for (String w : "banana apple cherry kiwi fig date elderberry grape apple".split(" ")) s.add(Optional.of(w));
        s.add(Optional.empty());
        s.add(Optional.empty());
        System.out.println(s + " " + s.size() + " " + s.contains(Optional.of("fig")) + " " + s.contains(Optional.of("lime")));
        Map<Optional<Integer>, Integer> m = new HashMap<>();
        for (int i = 40; i > 0; i -= 7) m.put(Optional.of(i * 1000), i);
        System.out.println(m + " " + m.containsKey(Optional.of(5000)) + " " + m.remove(Optional.of(33000)) + " " + m.size());
        System.out.println((Optional.of("x") == Optional.of("x")) + " " + Optional.of(1).equals(OptionalInt.of(1)) + " " + List.of(Optional.of("q")).contains(Optional.of("q")) + " " + List.of(Optional.of("q")).indexOf(Optional.empty()));
        System.out.println(List.of(Optional.of("a"), Optional.empty()).hashCode() + " " + Set.of(Optional.of(Optional.of(7))).contains(Optional.of(Optional.of(7))));
    }
}"####;
    let expected = r####"[Optional[banana], Optional[date], Optional.empty, Optional[apple], Optional[cherry], Optional[kiwi], Optional[fig], Optional[grape], Optional[elderberry]] 9 true false
{Optional[40000]=40, Optional[26000]=26, Optional[12000]=12, Optional[33000]=33, Optional[19000]=19, Optional[5000]=5} true 33 5
false false true -1
3968 true
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// A floating literal may start with the point (`.5`) or end with it (`5.`, `2.e1`, `5.f`); the lexer used to reject both. Expected output from openjdk 27 (Temurin).
#[test]
fn float_literals_may_omit_the_integer_or_fraction_digits() {
    let src = r####"
public class T {
    public static void main(String[] args) {
        double d = .5;
        double e = 5.;
        int[] x = {1, 2};
        System.out.println(d + " " + e + " " + 3d + " " + 2.e1 + " " + 5.f + " " + .25f + " " + 1.e-1 + " " + .5e1 + " " + 1_0.);
        System.out.println(x[1] + .5);
        System.out.println(1. + 2. + " " + (e * 2.) + " " + 10 / 4. + " " + (int) 7.);
    }
}
"####;
    let expected = r####"0.5 5.0 3.0 20.0 5.0 0.25 0.1 5.0 10.0
2.5
3.0 10.0 2.5 7
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:
{out}");
    assert_eq!(out, expected);
}

/// `Deque.removeFirstOccurrence`/`removeLastOccurrence` remove by equality from the head/tail side and answer whether one matched. Expected output from openjdk 27 (Temurin).
#[test]
fn deque_remove_first_and_last_occurrence() {
    let src = r####"
import java.util.*;
public class T { record P(int a){} public static void main(String[] a){ LinkedList<String> ll=new LinkedList<>(List.of("a","x","b","x","c")); System.out.println(ll.removeFirstOccurrence("x")+" "+ll); System.out.println(ll.removeLastOccurrence("x")+" "+ll+ll.removeLastOccurrence("q")+ll.removeFirstOccurrence("q"));
 Deque<Integer> d=new ArrayDeque<>(List.of(1,2,1,3,1)); d.removeFirstOccurrence(1); d.removeLastOccurrence(1); System.out.println(d);
 Deque<P> dp=new ArrayDeque<>(List.of(new P(1),new P(2),new P(1))); dp.removeLastOccurrence(new P(1)); System.out.println(dp); dp.removeFirstOccurrence(new P(1)); System.out.println(dp);} }
"####;
    let expected = r####"true [a, b, x, c]
true [a, b, c]falsefalse
[2, 1, 3]
[P[a=1], P[a=2]]
[P[a=2]]
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:
{out}");
    assert_eq!(out, expected);
}

/// `String.indexOf(int)`/`lastIndexOf(int)` search for the character with that code point (as in `s.chars().filter(c -> vowels.indexOf(c) >= 0)`); they used to search for the decimal digits and a non-code-point answers -1. Expected output from openjdk 27 (Temurin).
#[test]
fn string_index_of_takes_an_int_code_point() {
    let src = r####"
public class T { public static void main(String[] a){ String s="héllo wörld, 😀!"; int c='o'; char ch='l';
 System.out.println(s.indexOf(c)+" "+s.indexOf(c,6)+" "+s.lastIndexOf(c)+" "+s.lastIndexOf(c,5)+" "+s.indexOf(ch)+" "+s.indexOf(233)+" "+s.indexOf(-1)+" "+s.indexOf(0x110000)+" "+s.lastIndexOf(-5)+" "+s.indexOf(0xD800));
 long n=s.chars().filter(x->"aeiouö".indexOf(x)>=0).count(); System.out.println(n+" "+"abc".indexOf('c')+" "+"abc".indexOf("c")+" "+"a1".indexOf(49)); } }
"####;
    let expected = r####"4 -1 4 4 2 1 -1 -1 -1 -1
2 2 2 1
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:
{out}");
    assert_eq!(out, expected);
}

/// A conditional with a primitive branch and a non-wrapper reference branch is a reference conditional (JLS 15.25.3); as an overloaded call's argument it used to have no type and matched no overload. Expected output from openjdk 27 (Temurin).
#[test]
fn mixed_primitive_reference_conditional_is_an_object_argument() {
    let src = r####"
public class T {
    static class V {
        void f(long l) { System.out.print("long "); }
        void f(Integer i) { System.out.print("Integer "); }
        void f(Object o) { System.out.print("Object "); }
    }
    public static void main(String[] args) {
        V v = new V();
        boolean t = args.length == 0;
        v.f(t ? (Object) 1 : 2);
        v.f(t ? "s" : 2);
        v.f(t ? 1 : 2);
        v.f(t ? Integer.valueOf(1) : 2);
        v.f(t ? 1.5 : "x");
        System.out.println();
    }
}
"####;
    let expected = r####"Object Object long long Object 
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:
{out}");
    assert_eq!(out, expected);
}
