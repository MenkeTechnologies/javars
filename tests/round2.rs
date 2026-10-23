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
    assert!(
        ok,
        "program failed:
{out}"
    );
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
    assert!(
        ok,
        "program failed:
{out}"
    );
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
    assert!(
        ok,
        "program failed:
{out}"
    );
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
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// A `static` method called through an instance expression binds to the expression's static type, evaluates the receiver once and never dereferences it (a null receiver is fine); it used to be a compile error. Expected output from openjdk 27 (Temurin).
#[test]
fn static_method_through_instance_uses_static_type() {
    let src = r####"
public class T {
    static int made;
    static class Base {
        static String who() { return "Base"; }
        static int twice(int x) { return 2 * x; }
        String viaThis() { return this.who() + "/" + who(); }
    }
    static class Der extends Base {
        static String who() { return "Der"; }
    }
    static Base make() { made++; return new Der(); }
    public static void main(String[] args) {
        Base b = new Der();
        Der d = new Der();
        System.out.println(b.who() + " " + d.who() + " " + ((Der) b).who() + " " + b.twice(4));
        System.out.println(b.viaThis() + " " + d.viaThis());
        System.out.println(make().who() + " " + make().twice(3) + " " + made);
        Base n = null;
        System.out.println(n.who());
    }
}
"####;
    let expected = r####"Base Der Der 8
Base/Base Base/Base
Base 6 2
Base
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// `Collection.parallelStream()` and the stream's `parallel`/`sequential`/`unordered` switches run (javars is single-threaded, so results are those of the sequential pipeline) instead of being refused. Expected output from openjdk 27 (Temurin).
#[test]
fn parallel_stream_and_execution_mode_switches() {
    let src = r####"
import java.util.*;
import java.util.stream.*;
public class T {
    public static void main(String[] a) {
        List<Integer> l = List.of(3, 1, 2);
        System.out.println(l.parallelStream().mapToInt(x -> x).sum());
        System.out.println(new TreeSet<>(l).parallelStream().map(x -> x * 2).collect(Collectors.toList()));
        System.out.println(Stream.of(1, 2).parallel().count() + " " + Arrays.stream(new int[]{1, 2}).parallel().sum());
        System.out.println(IntStream.range(0, 5).parallel().sequential().unordered().sum());
        System.out.println(l.stream().parallel().sorted().map(String::valueOf).collect(Collectors.joining(",")));
        System.out.println(Set.of(7).parallelStream().findFirst().get());
    }
}
"####;
    let expected = r####"6
[2, 4, 6]
2 3
10
1,2,3
7
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// `String.format`/`printf`/`formatted` given one array where the varargs `Object...` goes use its elements as the argument list; the array used to be a single argument and the specifiers ran out (MissingFormatArgumentException). Expected output from openjdk 27 (Temurin).
#[test]
fn format_spreads_a_lone_object_array_argument() {
    let src = r####"
import java.util.*;
public class T {
    public static void main(String[] a) {
        Object[] two = {"a", 2};
        String[] names = {"x", "y", "z"};
        Integer[] nums = {7, 8};
        System.out.println(String.format("%s %s", two));
        System.out.println(String.format("%s-%s-%s", (Object[]) names));
        System.out.printf("%d+%d%n", (Object[]) nums);
        System.out.println("%s/%s".formatted(two));
        System.out.println(String.format("%2$s %1$s", two));
        Object[] none = {};
        System.out.println(String.format("none", none));
    }
}
"####;
    let expected = r####"a 2
x-y-z
7+8
a/2
2 a
none
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// A lambda assigned to `Iterable` is its `iterator()`: the enhanced `for` and the default `forEach` walk the iterator the lambda returns (they used to be refused or silently iterate nothing). Expected output from openjdk 27 (Temurin).
#[test]
fn lambda_iterable_is_walked_by_for_each_and_foreach() {
    let src = r####"
import java.util.*;
public class T {
    static Iterable<Integer> range(int n) {
        return () -> new Iterator<Integer>() {
            int i = 0;
            public boolean hasNext() { return i < n; }
            public Integer next() { return i++ * 10; }
        };
    }
    public static void main(String[] a) {
        Iterable<Integer> itb = () -> List.of(1, 2, 3).iterator();
        for (int x : itb) System.out.print(x);
        System.out.println();
        for (int x : itb) System.out.print(x + ";");
        System.out.println();
        for (Integer v : range(4)) { if (v == 10) continue; System.out.print(v + " "); }
        System.out.println();
        Iterable<String> words = List.of("p", "q");
        for (String w : words) System.out.print(w);
        System.out.println();
        itb.forEach(System.out::print);
        System.out.println();
        words.forEach(w -> System.out.print(w + "."));
        System.out.println();
    }
}
"####;
    let expected = r####"123
1;2;3;
0 20 30 
pq
123
p.q.
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// The `float` overloads of `Math.ulp`/`nextUp`/`nextDown`/`nextAfter`/`signum`/`copySign`/`scalb`/`getExponent` step and measure in 32-bit units and answer a `float` (they ran the `double` overload: `Math.ulp(1.0f)` was `2.220446049250313E-16`); `Math.getExponent` was unsupported. Expected output from openjdk 21.0.12.1.
#[test]
fn float_overloads_of_the_math_bit_pattern_statics() {
    let src = r####"
public class T {
    public static void main(String[] a) {
        float[] fs = {0f, -0f, 1f, -1.5f, 0.1f, Float.MIN_VALUE, Float.MIN_NORMAL, Float.MAX_VALUE, Float.NaN, Float.NEGATIVE_INFINITY, 3.4e-39f};
        for (float f : fs) {
            System.out.println(f + " " + Math.ulp(f) + " " + Math.nextUp(f) + " " + Math.nextDown(f) + " " + Math.getExponent(f)
                + " " + Math.signum(f) + " " + Math.copySign(0.3f, f) + " " + Math.scalb(f, 3) + " " + Math.scalb(f, -150)
                + " " + Math.nextAfter(f, 0.0) + " " + Math.scalb(f, 300));
        }
        double[] ds = {0.0, 1.0, 0.1, Double.MIN_VALUE, Double.MAX_VALUE, Double.NaN, 1e-310};
        for (double d : ds) System.out.println(Math.getExponent(d) + " " + Math.ulp(d));
        float x = 2.5f;
        double y = -1;
        Float boxed = 0.1f;
        System.out.println(Math.copySign(x, y) + " " + Math.nextAfter(x, 3) + " " + (Math.getExponent(x) + 1) + " " + Math.ulp(boxed) + " " + Math.nextUp(boxed));
    }
}
"####;
    let expected = r####"0.0 1.4E-45 1.4E-45 -1.4E-45 -127 0.0 0.3 0.0 0.0 0.0 0.0
-0.0 1.4E-45 1.4E-45 -1.4E-45 -127 -0.0 -0.3 -0.0 -0.0 0.0 -0.0
1.0 1.1920929E-7 1.0000001 0.99999994 0 1.0 0.3 8.0 0.0 0.99999994 Infinity
-1.5 1.1920929E-7 -1.4999999 -1.5000001 0 -1.0 -0.3 -12.0 -1.4E-45 -1.4999999 -Infinity
0.1 7.450581E-9 0.10000001 0.099999994 -4 1.0 0.3 0.8 0.0 0.099999994 Infinity
1.4E-45 1.4E-45 2.8E-45 0.0 -127 1.0 0.3 1.1E-44 0.0 0.0 Infinity
1.1754944E-38 1.4E-45 1.1754945E-38 1.1754942E-38 -126 1.0 0.3 9.403955E-38 0.0 1.1754942E-38 Infinity
3.4028235E38 2.028241E31 Infinity 3.4028233E38 127 1.0 0.3 Infinity 2.3841856E-7 3.4028233E38 Infinity
NaN NaN NaN NaN 128 NaN 0.3 NaN NaN NaN NaN
-Infinity Infinity -3.4028235E38 -Infinity 128 -1.0 -0.3 -Infinity -Infinity -3.4028235E38 -Infinity
3.4E-39 1.4E-45 3.400001E-39 3.399998E-39 -127 1.0 0.3 2.72E-38 0.0 3.399998E-39 Infinity
-1023 4.9E-324
0 2.220446049250313E-16
-4 1.3877787807814457E-17
-1023 4.9E-324
1023 1.99584030953472E292
1024 NaN
-1023 4.9E-324
-2.5 2.5000002 2 7.450581E-9 0.10000001
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// A `float` entering a collection element, a factory argument, or the argument of `equals` boxes to a `Float` holding the number (it boxed its `Float.toString` text, so `Float.equals` answered `false`, element arithmetic was a type error, and `+ 1` concatenated); a boxed `Float` renders through `toString()` at 32 bits and answers `isNaN`/`isInfinite`. Expected output from openjdk 21.0.12.1.
#[test]
fn float_elements_box_as_float_not_as_text() {
    let src = r####"
import java.util.*;
public class T {
    public static void main(String[] a) {
        List<Float> l = new ArrayList<>(Arrays.asList(0.5f, 0.1f, 0.3f));
        l.add(0.7f);
        Collections.sort(l);
        float g0 = l.get(0), g1 = l.get(1);
        System.out.println(l + " " + (g0 + 1) + " " + g1 * 2 + " " + l.get(0) + l.get(1) + " " + l.indexOf(0.3f) + " " + l.remove(Float.valueOf(0.1f)) + l);
        System.out.println(List.of(1.5f, 2.5f).get(1) + " " + Set.of(0.25f).contains(0.25f));
        Map<String, Float> m = new TreeMap<>();
        m.put("b", 0.1f);
        m.put("a", 2.7f);
        m.merge("a", 0.1f, Float::sum);
        float mb = m.get("b");
        System.out.println(m + " " + (mb + 1) + " " + m.get("b") + " " + m.containsValue(0.1f));
        Float bf = 3.3f;
        float p = 3.3f;
        System.out.println(bf.equals(3.3f) + " " + bf.equals(p) + " " + bf.equals(3.3) + " " + Objects.equals(bf, p));
        Object o = 0.1f;
        System.out.println(o.equals(0.1f) + " " + o.getClass().getSimpleName() + " " + Float.valueOf(0.1f).toString() + " " + bf.toString());
        Double nan = 0.0 / 0;
        Float inf = 1f / 0;
        System.out.println(nan.isNaN() + " " + nan.isInfinite() + " " + inf.isInfinite() + " " + inf.isNaN() + " " + bf.isNaN());
    }
}
"####;
    let expected = r####"[0.1, 0.3, 0.5, 0.7] 1.1 0.6 0.10.3 1 true[0.3, 0.5, 0.7]
2.5 true
{a=2.8, b=0.1} 1.1 0.1 true
true true false true
true Float 0.1 3.3
true false true false false
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// An `assert` statement parses (it was "expected Semi") and runs as the reference launcher runs it without `-ea`: disabled, evaluating neither the condition nor the detail. Expected output from openjdk 21.0.12.1.
#[test]
fn assert_statement_runs_disabled() {
    let src = r####"
public class T {
    static int calls = 0;
    static boolean touch() { calls++; return false; }
    public static void main(String[] a) {
        int x = 0;
        assert (x = 5) > 0;
        assert touch() : "detail " + touch();
        if (x == 0) assert false;
        for (int i = 0; i < 3; i++) assert i < 0 : i;
        System.out.println(x + " " + calls);
    }
}
"####;
    let expected = r####"0 0
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// `Long.decode`, `Predicate.isEqual`, `Arrays.parallelPrefix` (both arities, with the JDK range checks), `Stream.mapMulti`, `flatMapToInt`/`Long`/`Double`, and the unbound `String::chars`/`codePoints`/`strip`/`isBlank`/`lines`/`matches`/`hashCode` references, each refused before. Expected output from openjdk 21.0.12.1.
#[test]
fn long_decode_parallel_prefix_map_multi_and_string_refs() {
    let src = r####"
import java.util.*;
import java.util.function.*;
import java.util.stream.*;
public class T {
    public static void main(String[] a) {
        System.out.println(Long.decode("0x1F") + " " + Long.decode("-010") + " " + Long.decode("#ff") + " " + Long.decode("9223372036854775807") + " " + Long.decode("-0x8000000000000000"));
        for (String s : new String[]{"", "0x", "--1", "0x-1", "9223372036854775808"}) {
            try { Long.decode(s); } catch (NumberFormatException e) { System.out.println(e.getMessage()); }
        }
        Predicate<Object> isNull = Predicate.isEqual(null);
        System.out.println(Predicate.isEqual("a").test("a") + " " + Predicate.isEqual(1).test(1L) + " " + isNull.test(null) + " " + isNull.test("x"));
        int[] pz = {1, 2, 3, 4, 5};
        Arrays.parallelPrefix(pz, Integer::sum);
        long[] lz = {3, 1, 4, 1, 5};
        Arrays.parallelPrefix(lz, 1, 4, Math::max);
        String[] sz = {"a", "b", "c"};
        Arrays.parallelPrefix(sz, String::concat);
        System.out.println(Arrays.toString(pz) + Arrays.toString(lz) + Arrays.toString(sz));
        try { Arrays.parallelPrefix(pz, 3, 1, Integer::sum); } catch (IllegalArgumentException e) { System.out.println(e.getMessage()); }
        try { Arrays.parallelPrefix(pz, 0, 9, Integer::sum); } catch (ArrayIndexOutOfBoundsException e) { System.out.println(e.getMessage()); }
        System.out.println(Stream.of(1, 2, 3).<Object>mapMulti((v, sink) -> { if (v != 2) { sink.accept(v); sink.accept(-v); } }).toList());
        List<String> ws = List.of(" Ab ", "  ", "cd\nef", "x");
        System.out.println(ws.stream().flatMapToInt(String::chars).sum() + " " + ws.stream().flatMapToInt(String::codePoints).max().getAsInt()
            + " " + ws.stream().flatMapToLong(w -> LongStream.of(w.length())).sum() + " " + ws.stream().flatMapToDouble(w -> DoubleStream.of(w.length() / 2.0)).boxed().toList());
        System.out.println(ws.stream().map(String::strip).toList() + " " + ws.stream().filter(String::isBlank).count() + " " + ws.stream().flatMap(String::lines).toList());
        BiPredicate<String, String> m = String::matches;
        System.out.println(m.test("aaa", "a+") + " " + m.test("b", "a+") + " " + ws.stream().map(String::hashCode).toList());
    }
}
"####;
    let expected = r####"31 -8 255 9223372036854775807 -9223372036854775808
Zero length string
For input string: "" under radix 16
Sign character in wrong position
Sign character in wrong position
For input string: "9223372036854775808"
true false true false
[1, 3, 6, 10, 15][3, 1, 4, 4, 5][a, ab, abc]
fromIndex(3) > toIndex(1)
Array index out of range: 9
[1, -1, 3, -3]
823 120 12 [2.0, 1.0, 2.5, 0.5]
[Ab, , cd
ef, x] 1 [ Ab ,   , cd, ef, x]
true false [1018847, 1024, 94420522, 120]
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// `System.out` as a value and the `java.io` writers (`PrintWriter`, `BufferedWriter`, `OutputStreamWriter`, `StringWriter`, `PrintStream`, `BufferedOutputStream`), each refused before: a buffered writer holds its text until a flush (an unflushed `PrintWriter(System.out)` prints nothing), autoflush follows `println`/`printf`, `print` converts its argument by static type, closing a writer closes `System.out` beneath it, and the classes answer `instanceof` along the `java.io` hierarchy. Expected output from openjdk 21.0.12.1.
#[test]
fn output_writers_buffer_flush_and_close_like_the_jdk() {
    let src = r####"
import java.io.*;
import java.util.*;
public class T {
    static PrintWriter out = new PrintWriter(new BufferedWriter(new OutputStreamWriter(System.out)));
    record P(int x) { public String toString() { return "P" + x; } }
    public static void main(String[] a) throws IOException {
        PrintWriter lazy = new PrintWriter(System.out);
        lazy.print("never flushed");
        PrintWriter auto = new PrintWriter(System.out, true);
        auto.print("p");
        System.out.print("s");
        auto.println("l");
        auto.printf("f%n");
        auto.print("q");
        System.out.println("e");
        PrintStream ps = new PrintStream(new BufferedOutputStream(System.out));
        ps.print("a");
        System.out.println("b");
        ps.println("c");
        ps.flush();
        out.println(new P(3)); out.println('c'); out.println(2.5f); out.println(1e20); out.println(7L); out.println((Object) null);
        out.println(new char[]{'o', 'k'}); out.printf("%5.2f|%s|%c%n", Math.PI, List.of(1, 2), 'z'); out.format("%d%n", 42);
        char c = 'q'; out.write(c); out.write("\n"); out.write(new char[]{'a', 'b', '\n'}); out.write("hello\n", 1, 3); out.append("x").append('y').append(null); out.println();
        System.out.println("before the flush");
        out.flush();
        BufferedWriter bw = new BufferedWriter(new OutputStreamWriter(System.out));
        bw.write("x"); bw.write(65); bw.newLine(); bw.append('c').append("d"); bw.newLine(); bw.flush();
        StringWriter sw = new StringWriter();
        PrintWriter pw = new PrintWriter(sw);
        pw.print(1); pw.println('c'); pw.printf("%.2f", 1.0); pw.print(0.1f);
        System.out.println("[" + sw + "] " + pw.checkError());
        PrintStream o = System.out; o.printf("direct %d%n", 1); o.append("ap").append('\n'); o.write(66); o.println();
        Object w = pw, s = System.out;
        System.out.println((w instanceof Writer) + " " + (w instanceof Flushable) + " " + (s instanceof PrintStream) + " " + (s instanceof OutputStream) + " " + (w instanceof AutoCloseable) + " " + w.getClass().getName());
        try (PrintWriter t = new PrintWriter(new OutputStreamWriter(System.out))) { t.println("closed at the end of the try"); }
        System.out.println("dropped: System.out is closed");
        bw.write("dropped too");
    }
}
"####;
    let expected = r####"spl
f
e
b
ac
before the flush
P3
c
2.5
1.0E20
7
null
ok
 3.14|[1, 2]|z
42
q
ab
ellxynull
xA
cd
[1c
1.000.1] false
direct 1
ap
B
true true true true true java.io.PrintWriter
closed at the end of the try
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// An unflushed `PrintWriter(System.out)` loses exactly what its buffers still
/// hold at exit: the `BufferedWriter` hands on each 8192 characters the moment
/// it fills, and the `OutputStreamWriter` writes its 8192 bytes only when the
/// next character no longer fits — so 20000 characters print 8192. Measured on
/// openjdk 21.0.12.1.
#[test]
fn unflushed_print_writer_prints_only_what_overflowed() {
    let src = r####"
import java.io.*;
public class T {
    public static void main(String[] a) {
        PrintWriter pw = new PrintWriter(System.out);
        for (int i = 0; i < 2000; i++) pw.print("abcdefghi\n");
    }
}
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, "abcdefghi\n".repeat(819) + "ab");
}

/// A collection holding itself renders `(this Collection)` / `(this Map)` (it overflowed the native stack and aborted); `Arrays.deepHashCode` exists, `Arrays.hashCode(null)` is 0 and both are typed `int`; `ArrayList.ensureCapacity`/`trimToSize` are accepted; an `ArrayDeque` refuses `null` from its add family and its copy constructor; `Double.parseDouble`/`Float.parseFloat` say `multiple points` for a second point in the significand. Expected output from openjdk 21.0.12.1.
#[test]
fn collection_self_reference_deep_hash_array_deque_nulls_and_parse_messages() {
    let src = r####"
import java.util.*;
public class T {
    record R(int v) { public String toString() { return "R" + v; } }
    public static void main(String[] a) {
        List<Object> l = new ArrayList<>(); l.add(1); l.add(l); l.add(new R(2));
        Map<Object, Object> m = new LinkedHashMap<>(); m.put("me", m); m.put("r", new R(3));
        Set<Object> s = new HashSet<>(); s.add(s);
        Deque<Object> q = new ArrayDeque<>(); q.add(q); q.add("x");
        System.out.println(l + " " + m + " " + s + " " + q + " " + String.valueOf(l).length());
        int[] big = new int[50]; Arrays.fill(big, 123456789);
        System.out.println(Arrays.deepHashCode(new Object[]{new int[]{1, 2}, "s", null, new String[]{"a"}}) + " " + Arrays.deepHashCode(new int[][]{{1, 2}, {3}})
            + " " + Arrays.deepHashCode(null) + " " + Arrays.hashCode((int[]) null) + " " + Arrays.hashCode(big) * 31 + " " + (Arrays.deepHashCode(new int[][]{big}) + Integer.MAX_VALUE));
        ArrayList<Integer> al = new ArrayList<>(5); al.add(1); al.ensureCapacity(100); al.trimToSize(); System.out.println(al);
        Deque<Integer> d = new ArrayDeque<>(); d.add(1);
        try { d.add(null); } catch (NullPointerException e) { System.out.println("add"); }
        try { d.offerFirst(null); } catch (NullPointerException e) { System.out.println("offerFirst"); }
        try { d.push(null); } catch (NullPointerException e) { System.out.println("push"); }
        try { d.addAll(Arrays.asList(7, 8, null, 9)); } catch (NullPointerException e) { System.out.println("addAll " + d); }
        try { new ArrayDeque<>(Arrays.asList(1, null)); } catch (NullPointerException e) { System.out.println("ctor"); }
        Deque<Integer> ll = new LinkedList<>(); ll.add(null); ll.push(null);
        System.out.println(d.contains(null) + " " + d.remove(null) + " " + ll);
        for (String t : new String[]{"1.2.3", "..1", "1.2.3f", " 1.2. ", "1e5.5", "0x1.2.3p1", "NaN.."}) {
            try { Double.parseDouble(t); } catch (NumberFormatException e) { System.out.println("[" + t + "] " + e.getMessage()); }
            try { Float.parseFloat(t); } catch (NumberFormatException e) { System.out.println("[" + t + "]f " + e.getMessage()); }
        }
    }
}
"####;
    let expected = r####"[1, (this Collection), R2] {me=(this Map), r=R3} [(this Collection)] [(this Collection), x] 26
30646418 31809 0 0 -835269441 -1620238465
[1]
add
offerFirst
push
addAll [1, 7, 8]
ctor
false false [null, null]
[1.2.3] multiple points
[1.2.3]f multiple points
[..1] multiple points
[..1]f multiple points
[1.2.3f] multiple points
[1.2.3f]f multiple points
[ 1.2. ] multiple points
[ 1.2. ]f multiple points
[1e5.5] For input string: "1e5.5"
[1e5.5]f For input string: "1e5.5"
[0x1.2.3p1] For input string: "0x1.2.3p1"
[0x1.2.3p1]f For input string: "0x1.2.3p1"
[NaN..] For input string: "NaN.."
[NaN..]f For input string: "NaN.."
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// `String.getBytes()` (UTF-8, signed bytes) and `getBytes(charset)`, `new String(byte[] [, offset, length] [, charset])`, and the `StandardCharsets` constants, each refused or mis-decoded before (a `byte[]` went through the `char[]` path). Expected output from openjdk 21.0.12.1.
#[test]
fn string_get_bytes_and_new_string_from_bytes() {
    let src = r####"
import java.util.*;
import java.nio.charset.StandardCharsets;
public class T {
    public static void main(String[] a) throws Exception {
        String s = "h\u00e9llo\u20ac";
        byte[] b = s.getBytes();
        System.out.println(b.length + " " + Arrays.toString(b) + " " + b[1] + " " + (b[1] & 0xff));
        System.out.println(new String(b) + " " + new String(b, 0, 3) + " " + new String(b, StandardCharsets.UTF_8).equals(s) + " " + new String(b, "UTF-8").length() + " " + (new String(b) == new String(b)));
        byte[] l1 = s.getBytes(StandardCharsets.ISO_8859_1);
        System.out.println(Arrays.toString(l1) + " " + new String(l1, StandardCharsets.ISO_8859_1));
        byte[] as = s.getBytes("US-ASCII");
        System.out.println(Arrays.toString(as) + " " + new String(new byte[]{72, (byte) 200, 73}, StandardCharsets.US_ASCII));
        System.out.println(StandardCharsets.UTF_8 + " " + new String(new byte[]{(byte) 0xC3, (byte) 0xA9, 65}, "utf8") + " " + "abc".getBytes()[2] + " " + new String(new byte[0]).isEmpty());
        try { new String(b, 5, 99); } catch (StringIndexOutOfBoundsException e) { System.out.println(e.getMessage()); }
        int sum = 0;
        for (byte x : "AB".getBytes()) sum += x;
        System.out.println(sum);
    }
}
"####;
    let expected = r####"9 [104, -61, -87, 108, 108, 111, -30, -126, -84] -61 195
héllo€ hé true 6 false
[104, -23, 108, 108, 111, 63] héllo?
[104, 63, 108, 108, 111, 63] H�I
UTF-8 éA 99 true
Range [5, 5 + 99) out of bounds for length 9
131
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// A `char` entering a collection (`add`, `push`, `put`, the factories, `Collections`, an erased receiver such as `m.get(k).add(c)`) is a real `Character` box: `stack.peek() == '('` was `false`, `(char) (deque.pop() + 1)` concatenated, `Map.of('(', ')').get(s.charAt(0))` missed, and `getClass()` said `String`. Expected output from openjdk 21.0.12.1.
#[test]
fn character_elements_are_character_boxes() {
    let src = r####"
import java.util.*;
public class T {
  public static void main(String[] args) {
    Stack<Character> st = new Stack<>(); Deque<Character> dq = new ArrayDeque<>(); List<Character> l = new ArrayList<>();
    for (char c : "({[".toCharArray()) { st.push(c); dq.push(c); l.add(c); }
    System.out.println(st.peek() == '[' ); System.out.println(dq.peek() == '['); System.out.println(l.get(0) == '(');
    System.out.println(st.pop() + 1); System.out.println((char) (dq.pop() + 1)); System.out.println(l.get(1) - 'a');
    char top = st.peek(); System.out.println(top + 1); int code = l.get(2); System.out.println(code);
    Map<Character, Character> pairs = Map.of(')', '(', ']', '['); System.out.println(pairs.get(')') == '(' ? "match" : "no");
    System.out.println(pairs.get(']').equals('[') + " " + (pairs.get(']') == l.get(2)));
    Map<Character, Integer> cnt = new HashMap<>(); for (char c : "aab".toCharArray()) cnt.put(c, cnt.getOrDefault(c, 0) + 1);
    for (char c = 'a'; c <= 'b'; c++) System.out.print(cnt.get(c) + " "); System.out.println();
    StringBuilder sb = new StringBuilder(); for (Character c : l) sb.append(c); System.out.println(sb);
    List<Character> sorted = new ArrayList<>(List.of('z', 'a', 'm')); Collections.sort(sorted); System.out.println(sorted + " " + (sorted.get(0) < sorted.get(1)) + " " + Character.isLetter(sorted.get(0)) + " " + Character.toUpperCase(sorted.get(2)));
    String s = ""; for (char c : st) s += c; System.out.println(s + " " + st);
    Character boxed = 'x'; System.out.println(boxed + 1); System.out.println((char) (boxed + 1)); int sw = switch (l.get(0)) { case '(' -> 1; default -> 0; }; System.out.println(sw);
  }
}
"####;
    let expected = r####"true
true
true
92
\
26
124
91
match
true true
2 1 
({[
[a, m, z] true true Z
({ [(, {]
121
y
1
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// The same `Character` boxes reached through streams (`mapToObj(c -> (char) c)`, `groupingBy(s -> s.charAt(0))`), `Collections.frequency`, `compareTo` on an erased element, and `indexOf`/`contains`/`remove(Character)` on an erased receiver. Expected output from openjdk 21.0.12.1.
#[test]
fn character_elements_from_streams_and_erased_receivers() {
    let src = r####"
import java.util.*;
import java.util.stream.*;
public class T { public static void main(String[] a) {
  List<Character> l = "hello".chars().mapToObj(c -> (char) c).collect(Collectors.toList());
  System.out.println(l + " " + l.get(0).getClass().getSimpleName() + " " + (l.get(0) + 1) + " " + l.contains('e') + " " + l.indexOf('l') + " " + (l.get(2) == l.get(3)) + " " + (l.get(1) == 'e'));
  Map<Character, List<String>> g = Stream.of("apple", "avocado", "banana").collect(Collectors.groupingBy(s -> s.charAt(0)));
  System.out.println(g + " " + g.get('a').size() + " " + g.containsKey("b".charAt(0)) + " " + g.keySet().iterator().next().getClass().getSimpleName());
  Set<Character> seen = new HashSet<>(); for (char c : "mississippi".toCharArray()) seen.add(c); System.out.println(seen + " " + seen.contains('s') + " " + seen.size());
  Map<Character, Integer> f = new HashMap<>(); for (char c : "aabbbc".toCharArray()) f.merge(c, 1, Integer::sum);
  char best = ' '; int bc = 0; for (Map.Entry<Character, Integer> e : f.entrySet()) if (e.getValue() > bc) { bc = e.getValue(); best = e.getKey(); } System.out.println(best + " " + bc + " " + (best + 1) + " " + f);
  List<Character> sorted = l.stream().sorted(Comparator.reverseOrder()).toList(); System.out.println(sorted + " " + l.stream().distinct().count() + " " + l.stream().map(String::valueOf).collect(Collectors.joining("-")));
  StringBuilder sb = new StringBuilder(); for (Character c : l) sb.append(c); for (char c : l) sb.append((char) (c - 32)); System.out.println(sb);
  Deque<Character> st = new ArrayDeque<>(); String expr = "{[()]}"; boolean ok = true;
  for (char c : expr.toCharArray()) { if (c == '(' || c == '[' || c == '{') st.push(c); else { if (st.isEmpty()) { ok = false; break; } char o = st.pop(); if ((c == ')' && o != '(') || (c == ']' && o != '[') || (c == '}' && o != '{')) ok = false; } }
  System.out.println(ok && st.isEmpty());
  Character[] arr = {'z', 'a'}; Arrays.sort(arr); System.out.println(Arrays.toString(arr) + " " + Arrays.asList(arr).contains('a') + " " + List.of('q').get(0).compareTo('r') + " " + Collections.max(l) + " " + Collections.frequency(l, 'l'));
  Optional<Character> oc = l.stream().filter(Character::isUpperCase).findFirst(); System.out.println(oc + " " + l.stream().filter(c -> c > 'h').toList());
} }
"####;
    let expected = r####"[h, e, l, l, o] Character 105 true 2 true true
{a=[apple, avocado], b=[banana]} 2 true Character
[p, s, i, m] true 4
b 3 99 {a=2, b=3, c=1}
[o, l, l, h, e] 4 h-e-l-l-o
helloHELLO
true
[a, z] true -1 o 2
Optional.empty [l, l, o]
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// A `char` argument to an erased receiver: `String.indexOf`/`replace` and `StringBuilder.append`/`insert`/`setCharAt` read it as a character, a collection stores the `Character`, and `remove(Character.valueOf(c))` removes by value. Expected output from openjdk 21.0.12.1.
#[test]
fn char_arguments_through_erased_receivers() {
    let src = r####"
import java.util.*;
import java.util.function.*;
public class T {
  static Object id(Object o) { return o; }
  public static void main(String[] a) {
    Map<String, String> ms = new HashMap<>(); ms.put("k", "hello");
    System.out.println(ms.get("k").indexOf('l') + " " + ms.get("k").lastIndexOf('l') + " " + ms.get("k").indexOf('l', 3) + " " + ms.get("k").replace('l', 'L') + " " + ms.get("k").indexOf("lo"));
    Map<String, StringBuilder> sbs = new HashMap<>(); sbs.put("s", new StringBuilder("ab")); sbs.get("s").append('c').insert(0, 'z').setCharAt(1, 'A'); System.out.println(sbs.get("s") + " " + sbs.get("s").indexOf("c"));
    Map<String, List<Character>> m = new HashMap<>(); m.put("x", new ArrayList<>(List.of('p', 'q'))); System.out.println(m.get("x").indexOf('q') + " " + m.get("x").lastIndexOf('p') + " " + m.get("x").contains('q') + " " + m.get("x").remove(Character.valueOf('p')) + m.get("x"));
    Function<String, Integer> f = s -> s.indexOf('b'); System.out.println(f.apply("abc"));
    List<String> strs = List.of("ab", "cb"); strs.forEach(s -> System.out.print(s.indexOf('b') + s.charAt(0) + " ")); System.out.println();
    Map<Character, Integer> cm = new HashMap<>(); Map<String, Map<Character, Integer>> nest = new HashMap<>(); nest.put("n", cm); nest.get("n").put('z', 1); nest.get("n").merge('z', 5, Integer::sum); System.out.println(nest + " " + nest.get("n").get('z') + " " + nest.get("n").containsKey('z'));
  }
}
"####;
    let expected = r####"2 3 3 heLLo 3
zAbc 3
1 0 true true[q]
1
98 100 
{n={z=6}} 6 true
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// `java.util.Stack` and `Vector` (both refused as unknown classes): `push`/`pop`/`peek`/`empty`/`search`, `firstElement`/`lastElement`/`elementAt`/`addElement`/`insertElementAt`, `EmptyStackException`, `Vector`'s own index messages, `getClass()` and `instanceof Vector`. Expected output from openjdk 21.0.12.1.
#[test]
fn java_util_stack_and_vector() {
    let src = r####"
import java.util.*;
public class T { public static void main(String[] a) {
  Stack<Integer> s = new Stack<>();
  System.out.println(s.push(1) + " " + s.push(2) + " " + s.push(3) + " " + s + " " + s.peek() + " " + s.search(1) + " " + s.search(3) + " " + s.search(9) + " " + s.empty() + " " + s.size() + " " + s.get(0) + " " + s.firstElement() + " " + s.lastElement());
  System.out.println(s.pop() + " " + s + " " + s.getClass().getName() + " " + (s instanceof List) + " " + (s instanceof Vector) + " " + s.capacity());
  for (int x : s) System.out.print(x); System.out.println();
  s.addElement(7); s.insertElementAt(0, 0); System.out.println(s + " " + s.elementAt(1) + " " + s.indexOf(7) + " " + s.remove(Integer.valueOf(7)) + " " + s.remove(0) + s);
  try { s.get(5); } catch (RuntimeException e) { System.out.println(e); }
  try { s.get(-1); } catch (RuntimeException e) { System.out.println(e); }
  try { s.elementAt(5); } catch (RuntimeException e) { System.out.println(e); }
  try { s.set(5, 1); } catch (RuntimeException e) { System.out.println(e); }
  try { s.remove(5); } catch (RuntimeException e) { System.out.println(e); }
  try { s.add(9, 1); } catch (RuntimeException e) { System.out.println(e); }
  s.clear(); try { s.pop(); } catch (EmptyStackException e) { System.out.println(e + " " + e.getMessage()); }
  try { s.peek(); } catch (RuntimeException e) { System.out.println(e); }
  try { s.firstElement(); } catch (RuntimeException e) { System.out.println(e); }
  Vector<String> v = new Vector<>(List.of("a", "b")); v.add("c"); System.out.println(v + " " + v.getClass().getSimpleName() + " " + v.lastElement() + " " + v.contains("b") + " " + v.equals(List.of("a", "b", "c")) + " " + v.hashCode());
  Stack<Character> cs = new Stack<>(); for (char c : "abc".toCharArray()) cs.push(c); StringBuilder sb = new StringBuilder(); while (!cs.isEmpty()) sb.append(cs.pop()); System.out.println(sb + " " + cs.empty());
  Iterator<Integer> it = new Stack<Integer>().iterator(); System.out.println(it.hasNext());
  Stack<Integer> st = new Stack<>(); st.push(5); st.push(6); System.out.println(st.stream().map(x -> x * 2).toList() + " " + new ArrayList<>(st) + " " + st.toArray().length + " " + st.contains(6));
} }
"####;
    let expected = r####"1 2 3 [1, 2, 3] 3 3 1 -1 false 3 1 1 3
3 [1, 2] java.util.Stack true true 10
12
[0, 1, 2, 7] 1 3 true 0[1, 2]
java.lang.ArrayIndexOutOfBoundsException: Array index out of range: 5
java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 10
java.lang.ArrayIndexOutOfBoundsException: 5 >= 2
java.lang.ArrayIndexOutOfBoundsException: Array index out of range: 5
java.lang.ArrayIndexOutOfBoundsException: Array index out of range: 5
java.lang.ArrayIndexOutOfBoundsException: 9 > 2
java.util.EmptyStackException null
java.util.EmptyStackException
java.util.NoSuchElementException
[a, b, c] Vector c true true 126145
cba true
false
[10, 12] [5, 6] 2 true
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// `Vector.capacity()` follows the JDK growth rule (double, or exactly the need when doubling falls short; `trimToSize`, `ensureCapacity`), and a negative index reports the backing array's capacity as the JDK's message does — including the growth an `add(-1, e)` performs before it fails. Expected output from openjdk 21.0.12.1.
#[test]
fn vector_capacity_and_negative_index_messages() {
    let src = r####"
import java.util.*;
public class T { public static void main(String[] a) {
  Vector<Integer> v = new Vector<>(List.of(1, 2));
  Runnable[] rs = { () -> v.set(-1, 0), () -> v.remove(-1), () -> v.add(-1, 0), () -> v.removeElementAt(-1), () -> v.removeElementAt(5), () -> v.setElementAt(0, -1), () -> v.setElementAt(0, 5), () -> v.insertElementAt(0, -1), () -> v.elementAt(-1), () -> new Vector<Integer>(-1), () -> v.subList(0, 9), () -> v.listIterator(9) };
  for (Runnable r : rs) { try { r.run(); System.out.println("ok"); } catch (RuntimeException e) { System.out.println(e); } }
  System.out.println(v.capacity()); v.add(3); System.out.println(v.capacity()); v.addAll(List.of(4, 5, 6, 7, 8)); System.out.println(v.capacity()); v.trimToSize(); System.out.println(v.capacity()); v.ensureCapacity(9); System.out.println(v.capacity()); v.ensureCapacity(100); System.out.println(v.capacity());
  Vector<Integer> w = new Vector<>(3); System.out.println(w.capacity()); for (int i = 0; i < 4; i++) w.add(i); System.out.println(w.capacity()); w.clear(); System.out.println(w.capacity() + " " + new Vector<>(0).capacity()); Vector<Integer> z = new Vector<>(0); z.add(1); System.out.println(z.capacity()); z.add(2); z.add(3); System.out.println(z.capacity());
  Stack<Integer> s = new Stack<>(); for (int i = 0; i < 11; i++) s.push(i); System.out.println(s.capacity());
} }
"####;
    let expected = r####"java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 2
java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 2
java.lang.ArrayIndexOutOfBoundsException: arraycopy: source index -1 out of bounds for object array[4]
java.lang.ArrayIndexOutOfBoundsException: Array index out of range: -1
java.lang.ArrayIndexOutOfBoundsException: 5 >= 2
java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 4
java.lang.ArrayIndexOutOfBoundsException: 5 >= 2
java.lang.ArrayIndexOutOfBoundsException: arraycopy: source index -1 out of bounds for object array[4]
java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 4
java.lang.IllegalArgumentException: Illegal Capacity: -1
java.lang.IndexOutOfBoundsException: toIndex = 9
java.lang.IndexOutOfBoundsException: Index: 9
4
4
8
8
16
100
3
6
6 0
1
4
20
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// The Java 21 SequencedCollection/SequencedSet/SequencedMap members, refused before: List/Deque.reversed(), LinkedHashMap putFirst/putLast/firstEntry/lastEntry/pollFirstEntry/reversed/sequencedKeySet, LinkedHashSet addFirst/addLast/getFirst/getLast/removeFirst/removeLast/reversed, and the sorted collections' getFirst/removeFirst/reversed with their UnsupportedOperationException for addFirst/putFirst. Expected output from openjdk 21.0.12.1.
#[test]
fn java21_sequenced_collection_members() {
    let src = r####"
import java.util.*;
public class T {
    public static void main(String[] a) {
        List<Integer> l = new ArrayList<>(List.of(1, 2, 3));
        System.out.println(l.reversed() + " " + l.getFirst() + " " + l.getLast() + " " + List.of(4, 5).reversed() + " " + new ArrayDeque<>(List.of(1, 2)).reversed());
        try { List.of(1).reversed().add(2); } catch (UnsupportedOperationException e) { System.out.println("uoe"); }
        LinkedHashMap<String, Integer> m = new LinkedHashMap<>();
        m.put("a", 1); m.put("b", 2); m.putFirst("z", 0); m.putLast("a", 5);
        System.out.println(m + " " + m.firstEntry() + " " + m.lastEntry() + " " + m.sequencedKeySet().reversed() + " " + m.pollFirstEntry() + " " + m.reversed() + " " + m.sequencedValues() + " " + m.sequencedEntrySet());
        try { m.firstEntry().setValue(9); } catch (UnsupportedOperationException e) { System.out.println("snapshot"); }
        System.out.println(new LinkedHashMap<String, Integer>().firstEntry() + " " + new LinkedHashMap<String, Integer>().pollLastEntry());
        LinkedHashSet<Integer> s = new LinkedHashSet<>(List.of(3, 1, 2));
        s.addFirst(9); s.addLast(3);
        System.out.println(s + " " + s.getFirst() + " " + s.getLast() + " " + s.reversed() + " " + s.removeFirst() + s + " " + s.removeLast() + s);
        try { new LinkedHashSet<Integer>().getFirst(); } catch (NoSuchElementException e) { System.out.println("nse " + e.getMessage()); }
        TreeSet<Integer> t = new TreeSet<>(List.of(5, 1, 3));
        System.out.println(t.getFirst() + " " + t.getLast() + " " + t.reversed() + " " + t.removeFirst() + t + " " + t.removeLast() + t);
        try { t.addFirst(0); } catch (UnsupportedOperationException e) { System.out.println("sorted uoe"); }
        try { new TreeSet<Integer>().removeFirst(); } catch (NoSuchElementException e) { System.out.println("sorted nse"); }
        TreeMap<Integer, String> tm = new TreeMap<>(Map.of(1, "x", 2, "y"));
        System.out.println(tm.reversed() + " " + tm.sequencedKeySet() + " " + tm.firstEntry());
        try { tm.putFirst(0, "w"); } catch (UnsupportedOperationException e) { System.out.println("map uoe"); }
    }
}
"####;
    let expected = r####"[3, 2, 1] 1 3 [5, 4] [2, 1]
uoe
{z=0, b=2, a=5} z=0 a=5 [a, b, z] z=0 {a=5, b=2} [2, 5] [b=2, a=5]
snapshot
null null
[9, 1, 2, 3] 9 3 [3, 2, 1, 9] 9[1, 2, 3] 3[1, 2]
nse null
1 5 [5, 3, 1] 1[3, 5] 5[3]
sorted uoe
sorted nse
{2=y, 1=x} [1, 2] 1=x
map uoe
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// Locale.US/ROOT/UK/ENGLISH/CANADA (a leading Locale argument to String.format/printf and the case maps is the root-locale call), Random.ints/longs/doubles(n [, origin, bound]) drawing the JDK's sequence, Stream.reduce(identity, accumulator, combiner), and System.getProperty/getenv, each refused before. Expected output from openjdk 21.0.12.1.
#[test]
fn locale_constants_random_streams_reduce3_and_properties() {
    let src = r####"
import java.util.*;
import java.util.stream.*;
import java.io.*;
public class T {
    public static void main(String[] a) {
        System.out.println(String.format(Locale.US, "%,.2f|%d|%s", 1234.5, 7, "x") + " " + String.format(Locale.ROOT, "%e", 12345.678) + " " + "ABC".toLowerCase(Locale.ROOT) + "abc".toUpperCase(Locale.ENGLISH));
        System.out.println(Locale.US + "|" + Locale.ROOT + "|" + Locale.UK + "|" + Locale.ENGLISH + "|" + Locale.CANADA);
        System.out.printf(Locale.US, "%.3f%n", Math.E);
        PrintWriter pw = new PrintWriter(System.out); pw.printf(Locale.US, "%08.2f%n", -3.5); pw.flush();
        Locale l = Locale.US; System.out.println(String.format(l, "%.1f", 2.25));
        Random r = new Random(1); int[] arr = r.ints(5, 0, 10).toArray(); System.out.println(Arrays.toString(arr) + " " + r.nextInt());
        System.out.println(new Random(2).doubles(2).sum() + " " + new Random(3).ints(3).boxed().toList() + " " + new Random(4).longs(2).boxed().toList() + " " + new Random(5).longs(3, -5, 5).sum() + " " + new Random(6).doubles(2, 1.0, 2.0).boxed().toList());
        System.out.println(new Random(7).ints(4, -3, 3).map(x -> x * x).sum() + " " + new Random(8).ints(0).count() + " " + new Random(9).ints(6, 0, 1 << 20).max().getAsInt());
        try { new Random().ints(-1); } catch (IllegalArgumentException e) { System.out.println(e.getMessage()); }
        try { new Random().ints(0, 5, 1); } catch (IllegalArgumentException e) { System.out.println(e.getMessage()); }
        System.out.println(Stream.of(1, 2, 3).reduce(10, (acc, x) -> acc + x * x, Integer::sum) + " " + Stream.of("a", "bb").reduce(0, (n, s) -> n + s.length(), (p, q) -> p + q));
        System.out.println(System.getProperty("line.separator").length() + " " + System.getProperty("file.separator") + System.getProperty("path.separator") + " " + System.getProperty("my.flag") + " " + System.getProperty("my.flag", "dflt") + " " + System.getenv("JAVARS_SURELY_UNSET_VAR"));
    }
}
"####;
    let expected = r####"1,234.50|7|x 1.234568e+04 abcABC
en_US||en_GB|en|en_CA
2.718
-0003.50
2.3
[5, 8, 7, 3, 4] 155629808
1.6325945600499603 [-1155099828, -1879439976, 304908421] [-4969378402838085704, -1499461942424923123] -12 [1.7307886238322472, 1.5796252073129176]
3 0 851619
size must be non-negative
bound must be greater than origin
24 3
1 /: null dflt null
"####;
    let (out, ok) = run(src);
    assert!(
        ok,
        "program failed:
{out}"
    );
    assert_eq!(out, expected);
}

/// A `char` cast to a reference type boxes to `Character` (JLS 5.1.7): `(Object) c` prints `c`, its class is `Character`, and `equals(99)` is false. A boxed `Character`/`Short`/`Byte` receiver whose static type the compiler could not name (a `Comparable<Character>` local, an erased `List.get`, `naturalOrder()`) compares by difference, not sign.
#[test]
fn char_cast_to_reference_boxes_and_boxed_compare_to_follows_the_runtime_class() {
    let src = r####"
import java.util.*;
public class T {
    static String show(Object o) { return o + ":" + o.getClass().getSimpleName(); }
    public static void main(String[] a) {
        char c = 'c';
        System.out.println((Object) c);
        System.out.println(show((Object) c) + " " + show((Comparable<Character>) c) + " " + ((Object) 'z').getClass().getName());
        System.out.println(((Object) c).equals('c') + " " + ((Object) c).equals(99) + " " + ((Object) c).hashCode() + " " + (((Object) c) instanceof Character) + " " + (((Object) c) instanceof Integer));
        System.out.println(String.valueOf((Object) c) + Objects.toString((Object) c) + String.format("%s", (Object) c));
        List<Object> l = new ArrayList<>(); l.add((Object) c); Object[] arr = {(Object) c};
        System.out.println(l + " " + Arrays.toString(arr) + " " + l.get(0).getClass().getSimpleName());
        Comparable<Character> cc = 'c';
        List<Character> cs = new ArrayList<>(List.of('z', 'a'));
        Comparator<Character> nat = Comparator.naturalOrder();
        System.out.println(cc.compareTo('a') + " " + cs.get(0).compareTo(cs.get(1)) + " " + nat.compare('c', 'a') + " " + ((Comparable<Character>) (Character) 'a').compareTo('c'));
        List<Short> ss = List.of((short) 9, (short) 2); List<Byte> bs = List.of((byte) -3, (byte) 4); List<Integer> is = List.of(9, 2);
        System.out.println(ss.get(0).compareTo(ss.get(1)) + " " + bs.get(0).compareTo(bs.get(1)) + " " + is.get(0).compareTo(is.get(1)) + " " + Comparator.<Short>naturalOrder().compare((short) 1, (short) 8));
        cs.sort(null); System.out.println(cs + " " + Collections.max(cs));
    }
}
"####;
    let expected = r####"c
c:Character c:Character java.lang.Character
true false 99 true false
ccc
[c] [c] Character
2 25 2 -2
7 -7 1 -7
[a, z] z
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// Type arguments are kept beside the erased declared type (JLS 4.5) and type what a JDK generic member answers: `List<Float>.get` is a `Float` and computes at `float` width, `Map<String, Integer>.get` an `Integer` that wraps at 32 bits — through locals, `var`, fields, parameters, a method's declared return type, `Map.Entry`, `Optional`, `Iterator` and nested arguments — and `List.remove` of an `Integer`-typed element removes by value.
#[test]
fn generic_type_arguments_type_the_members_of_jdk_collections() {
    let src = r####"
import java.util.*;
public class T {
    static Map<String, Integer> counts = new HashMap<>();
    List<Float> fs = new ArrayList<>(List.of(0.1f));
    static int twice(List<Integer> xs) { return xs.get(0) + xs.get(0); }
    static List<Integer> big() { return new ArrayList<>(List.of(2_000_000_000)); }
    public static void main(String[] a) {
        List<Float> l = new ArrayList<>(List.of(0.1f));
        Map<String, Float> m = new HashMap<>(Map.of("x", 0.1f));
        System.out.println(l.get(0) + 1 + " " + m.get("x") * 3 + " " + new T().fs.get(0) * 3);
        List<Integer> li = new ArrayList<>(List.of(2_000_000_000));
        Map<String, List<Integer>> ml = new HashMap<>(Map.of("k", li));
        counts.put("z", 2_000_000_000);
        System.out.println(li.get(0) + li.get(0) + " " + ml.get("k").get(0) * 2 + " " + (counts.get("z") + counts.get("z")) + " " + twice(li) + " " + (big().get(0) + big().get(0)));
        for (Map.Entry<String, Integer> e : counts.entrySet()) System.out.print(e.getValue() + e.getValue() + " ");
        for (var e : counts.entrySet()) System.out.print(e.getValue() * 2 + " ");
        var v = li.get(0);
        Optional<Integer> o = Optional.of(2_000_000_000);
        Iterator<Integer> it = li.iterator();
        Deque<Integer> st = new ArrayDeque<>(li);
        TreeMap<Integer, String> tm = new TreeMap<>(Map.of(2_000_000_000, "b"));
        System.out.println(v * 3 + " " + (o.get() + o.get()) + " " + (it.next() + 2_000_000_000) + " " + (st.peek() + st.peekLast()) + " " + (tm.lastKey() + tm.firstEntry().getKey()));
        List<Short> sh = List.of((short) 30000); List<Character> lc = List.of('x'); List<String> ls = List.of("a");
        System.out.println(sh.get(0) + sh.get(0) + " " + (lc.get(0) + lc.get(0)) + " " + ls.get(0) + 1 + 2 + " " + li.stream().toList().get(0) * 2);
        List<Integer> src = new ArrayList<>(List.of(10, 20, 30)); List<Integer> key = List.of(20);
        src.remove(key.get(0));
        System.out.println(src + " " + (src.remove(0) + 1) + " " + src);
    }
}
"####;
    let expected = r####"1.1 0.3 0.3
-294967296 -294967296 -294967296 -294967296 -294967296
-294967296 -294967296 1705032704 -294967296 -294967296 -294967296 -294967296
60000 240 a12 -294967296
[10, 30] 11 [30]
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// A lambda assigned to a parameterized functional interface types its parameters and result by the type arguments (JLS 15.27.3): `Function<Integer, Integer> f = x -> x * 2` wraps at 32 bits, whether the target is a local, an assignment, a static or instance field, a method parameter, or a method's return type; a `void` lambda body discards a `null` `Integer` without unboxing it.
#[test]
fn lambdas_take_parameter_and_result_types_from_the_declared_type_arguments() {
    let src = r####"
import java.util.*;
import java.util.function.*;
public class T {
    static int ap(Function<Integer, Integer> f, int x) { return f.apply(x); }
    static Function<Integer, Integer> mk() { return x -> x * 2; }
    static Function<Integer, Integer> field = x -> x + x;
    BinaryOperator<Integer> inst = (x, y) -> x * y;
    public static void main(String[] a) {
        Function<Integer, Integer> f = x -> x * 2;
        BinaryOperator<Integer> add = (x, y) -> x + y;
        Supplier<Integer> s = () -> Integer.MAX_VALUE + 1;
        Comparator<Integer> c = (x, y) -> x - y;
        Predicate<Integer> p = x -> x + 1 > 0;
        System.out.println(f.apply(2_000_000_000) + " " + add.apply(2_000_000_000, 2_000_000_000) + " " + s.get() + " " + c.compare(-2_000_000_000, 2_000_000_000) + " " + p.test(Integer.MAX_VALUE));
        Function<Integer, Integer> g; g = x -> x * 3;
        System.out.println(g.apply(1_000_000_000) + " " + ap(x -> x * 2, 2_000_000_000) + " " + mk().apply(2_000_000_000) + " " + field.apply(2_000_000_000) + " " + new T().inst.apply(100000, 100000));
        Supplier<Float> sf = () -> 0.1f;
        Function<Float, Float> ff = x -> x * 3;
        BiFunction<Integer, Integer, Integer> bf = (x, y) -> { int z = x * y; return z + x; };
        Function<List<Integer>, Integer> fl = xs -> xs.get(0) + xs.get(0);
        System.out.println(sf.get() + 1 + " " + ff.apply(0.1f) + " " + bf.apply(100000, 100000) + " " + fl.apply(List.of(2_000_000_000)));
        Consumer<Integer> co = x -> System.out.println(x * 2);
        co.accept(2_000_000_000);
        Runnable r = () -> counts().put("k", null);
        r.run();
        System.out.println(counts());
    }
    static Map<String, Integer> M = new HashMap<>();
    static Map<String, Integer> counts() { return M; }
}
"####;
    let expected = r####"-294967296 -294967296 -2147483648 294967296 false
-1294967296 -294967296 -294967296 -294967296 1410065408
1.1 0.3 1410165408 -294967296
-294967296
{k=null}
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// A lambda passed to a JDK generic method takes its parameter types from the receiver's type arguments: `List<Integer>.replaceAll`'s `UnaryOperator<Integer>`, `Map.merge`'s `BiFunction<V, V, V>`, a `Stream<Integer>`'s `filter`/`map`/`reduce`/`max`, `Optional.ifPresent`, `Iterator.forEachRemaining` — so `Integer` arithmetic in the body wraps at 32 bits.
#[test]
fn lambdas_passed_to_jdk_generic_methods_take_the_receivers_element_type() {
    let src = r####"
import java.util.*;
import java.util.stream.*;
public class T {
    public static void main(String[] a) {
        List<Integer> li = new ArrayList<>(List.of(2_000_000_000, 1));
        li.replaceAll(x -> x * 2);
        System.out.print(li + " ");
        li.sort((x, y) -> y - x);
        System.out.print(li + " ");
        li.forEach(x -> System.out.print(x + x + " "));
        li.removeIf(x -> x + x > 0);
        System.out.println(li);
        Map<String, Integer> m = new LinkedHashMap<>(Map.of("a", 2_000_000_000));
        m.merge("a", 2_000_000_000, (x, y) -> x + y);
        System.out.print(m + " ");
        m.compute("a", (k, v) -> v * 2);
        m.computeIfPresent("a", (k, v) -> v + 2_000_000_000);
        System.out.print(m + " ");
        m.forEach((k, v) -> System.out.print(k + (v + v) + " "));
        m.replaceAll((k, v) -> v * 3);
        System.out.println(m);
        List<Integer> src = List.of(10, 20, 30);
        System.out.println(src.stream().filter(x -> x + 2_147_483_637 < 0).map(x -> x * 100_000_000).collect(Collectors.toList())
            + " " + src.stream().reduce(0, (x, y) -> x + y * 100_000_000)
            + " " + src.stream().max((x, y) -> x * 100_000_000 - y * 100_000_000).get()
            + " " + src.stream().mapToLong(x -> x * 1_000_000_000).sum());
        Optional<Integer> o = Optional.of(2_000_000_000);
        o.ifPresent(v -> System.out.print(v + v + " "));
        System.out.println(o.filter(v -> v + v < 0).isPresent() + " " + o.map(v -> v + v).get());
        Iterator<Integer> it = src.iterator();
        it.forEachRemaining(v -> System.out.print(v * 1_000_000_000 + " "));
        System.out.println();
    }
}
"####;
    let expected = r####"[-294967296, 2] [2, -294967296] 4 -589934592 [-294967296]
{a=-294967296} {a=1410065408} a-1474836480 {a=-64771072}
[2000000000, -1294967296] 1705032704 30 -129542144
-294967296 true -294967296
1410065408 -1474836480 -64771072 
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// A lone primitive array passed to `List.of`/`Set.of`/`Stream.of`/`Arrays.asList` is the one element of the result, since an `int[]` is no `E[]` (JLS 15.12.2.4); a reference array (`Integer[]`, `int[][]`) is the varargs array itself.
#[test]
fn a_lone_primitive_array_is_one_element_of_a_varargs_factory() {
    let src = r####"
import java.util.*;
import java.util.stream.*;
public class T {
    public static void main(String[] a) {
        int[] x = {3, 4};
        char[] cs = {'q'};
        System.out.println(List.of(x).size() + " " + Arrays.asList(x).size() + " " + Set.of(cs).size() + " " + Stream.of(x).count());
        System.out.println(List.of(x).get(0)[1] + " " + Arrays.asList(x).get(0)[0] + " " + Arrays.asList(cs).get(0).length);
        Integer[] boxed = {1, 2};
        int[][] grid = {{1}, {2}, {3}};
        System.out.println(List.of(boxed).size() + " " + Arrays.asList(boxed).size() + " " + List.of(grid).size() + " " + Stream.of(boxed).count() + " " + Arrays.asList(x, x).size());
    }
}
"####;
    let expected = r####"1 1 1 1
4 3 1
2 2 3 2 2
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// `Random.ints`/`longs`/`doubles` streams, sized and unsized, ranged and not, draw from the generator at each pull (the JDK's `RandomIntsSpliterator` calls `nextInt()` per `tryAdvance`), so a draw made on the generator after the stream is built comes first; they are `SIZED` (`Long.MAX_VALUE` for the unsized forms), so `count()` runs no `peek`, and a bad range is rejected at the call.
#[test]
fn random_streams_draw_lazily_and_are_sized() {
    let src = r####"
import java.util.*;
import java.util.stream.*;
public class T {
    public static void main(String[] a) {
        System.out.println(new Random(1).ints(3).peek(x -> System.out.print("p")).count());
        System.out.println(new Random(1).ints().count());
        System.out.println(new Random(1).ints().limit(3).peek(x -> System.out.print("q")).count());
        Random r = new Random(1);
        IntStream s = r.ints(2);
        int first = r.nextInt();
        System.out.println(first + " " + s.boxed().toList());
        Random r2 = new Random(1);
        IntStream s2 = r2.ints();
        int f2 = r2.nextInt();
        System.out.println(f2 + " " + s2.limit(2).boxed().toList());
        System.out.println(new Random(1).ints().limit(3).boxed().toList());
        System.out.println(new Random(1).doubles().limit(2).boxed().toList());
        System.out.println(new Random(1).ints(0, 10).limit(5).boxed().toList());
        System.out.println(new Random(1).longs(-5, 5).limit(3).boxed().toList());
        System.out.println(new Random(1).doubles(1.0, 2.0).limit(2).boxed().toList());
        System.out.println(new Random(1).longs().limit(2).boxed().toList());
        try { new Random(1).ints(5, 5); } catch (IllegalArgumentException e) { System.out.println(e.getMessage()); }
        System.out.println(new Random(1).ints().anyMatch(x -> x > 0));
        System.out.println(new Random(1).ints(0, 100).filter(x -> x > 90).findFirst().getAsInt());
        System.out.println(new Random(1).ints().skip(2).limit(1).sum());
    }
}
"####;
    let expected = r####"3
9223372036854775807
3
-1155869325 [431529176, 1761283695]
-1155869325 [431529176, 1761283695]
[-1155869325, 431529176, 1761283695]
[0.7308781907032909, 0.41008081149220166]
[5, 8, 7, 3, 4]
[1, -2, 3]
[1.7308781907032909, 1.4100808114922017]
[-4964420948893066024, 7564655870752979346]
bound must be greater than origin
true
92
1761283695
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// `Enum.valueOf(Color.class, s)` is `Color.valueOf(s)` (JLS 8.9.3), nested enum class literals included, and a `null` name is `NullPointerException: Name is null` rather than a miss.
#[test]
fn enum_value_of_by_class_literal_and_a_null_name() {
    let src = r####"
public class T {
    enum Color { RED, GREEN; String low() { return name().toLowerCase(); } }
    static class Box { enum In { A, B } }
    public static void main(String[] a) {
        System.out.println(Enum.valueOf(Color.class, "GREEN"));
        System.out.println(Enum.valueOf(Color.class, "RED").low() + Enum.valueOf(Color.class, "GREEN").ordinal());
        try { Enum.valueOf(Color.class, "BLUE"); } catch (IllegalArgumentException e) { System.out.println(e.getMessage()); }
        try { Enum.valueOf(Box.In.class, "C"); } catch (IllegalArgumentException e) { System.out.println(e.getMessage()); }
        System.out.println(Enum.valueOf(Box.In.class, "B") == Box.In.B);
        System.out.println(Box.In.valueOf("A").compareTo(Box.In.B) + " " + Box.In.values().length);
        Color c = Enum.valueOf(Color.class, "RED");
        switch (c) { case RED -> System.out.println("r"); default -> System.out.println("d"); }
        try { Enum.valueOf(Color.class, null); } catch (NullPointerException e) { System.out.println(e.getMessage()); }
    }
}
"####;
    let expected = r####"GREEN
red1
No enum constant T.Color.BLUE
No enum constant T.Box.In.C
true
-1 2
r
Name is null
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// A nested type named through its enclosers resolves like its simple name: `Box.In.B` is the enum constant, `T.Box.n` the static field, and `Box.In.class` the binary-named `T$Box$In` (an `interface` for `Box.I.class`).
#[test]
fn nested_types_named_through_their_enclosers() {
    let src = r####"
public class T {
    static class Box { enum In { A, B } static int n = 5; interface I {} static class Deep { static class D2 {} } }
    public static void main(String[] a) {
        System.out.println(Box.In.B + " " + Box.In.class + " " + Box.In.class.getName() + " " + Box.Deep.D2.class.getName() + " " + Box.I.class);
        Box.In x = Box.In.A;
        System.out.println(x == Box.In.A);
        switch (x) { case A -> System.out.println("a"); case B -> System.out.println("b"); }
        System.out.println(Box.n + T.Box.n);
        System.out.println(T.Box.In.B.ordinal());
    }
}
"####;
    let expected = r####"B class T$Box$In T$Box$In T$Box$Deep$D2 interface T$Box$I
true
a
10
1
"####;
    let (out, ok) = run(src);
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, expected);
}

/// A local class or interface takes no access modifier, no `static` (JLS 14.3)
/// and no `sealed`/`non-sealed` (JLS 8.1.1.2): `javac` rejects each, so the
/// program never prints. `final`, `abstract` and `strictfp` are allowed.
#[test]
fn a_local_type_rejects_access_static_and_sealed_modifiers() {
    let program = |decl: &str| {
        format!(
            "public class T {{\n    public static void main(String[] a) {{\n        {decl}\n        System.out.println(\"ok\");\n    }}\n}}\n"
        )
    };
    for decl in [
        "sealed interface S permits A {} final class A implements S {}",
        "non-sealed class N {}",
        "public class L {}",
        "private class L {}",
        "protected record R(int x) {}",
        "static class L {}",
        "static enum E { X }",
    ] {
        let (out, ok) = run(&program(decl));
        assert!(
            !ok && out.is_empty(),
            "javac rejects `{decl}`, javars ran it: {out:?}"
        );
    }
    let (out, ok) = run(&program(
        "final class F {} abstract class G {} strictfp class H {}",
    ));
    assert!(ok, "program failed:\n{out}");
    assert_eq!(out, "ok\n");
}
