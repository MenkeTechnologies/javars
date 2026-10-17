//! `java.math.BigDecimal`, `RoundingMode` and `MathContext`. Each program below was
//! run on a real JDK (25) and its stdout frozen, so the suite needs no JDK in CI.

use std::process::Command;

fn run(src: &str, class: &str) -> (String, bool) {
    let dir = std::env::temp_dir().join(format!("javars_bigdec_{}_{}", class, std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join(format!("{class}.java"));
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

/// String grammar and its refusals, exact `double` expansion, `valueOf`, scale-sensitive rendering, the divide/round/sqrt family and `Formatter` integration.
#[test]
fn bigdecimal_string_grammar_scale_and_rounding_match_the_jdk() {
    let src = r####"import java.math.*;
public class Y {
    public static void main(String[] a){
        for (String s : new String[]{"", "abc", "1e", "1.2.3", "-", ".", "+.5", "1e+", "1e5.5", " 1", "1 ", "1_0", "0x10", "--1", "1e99999999999", "5.", ".5e-3", "-0.00", "1E3", "0E-10","00012.3400"}) {
            try { BigDecimal b = new BigDecimal(s); System.out.println("[" + s + "] ok " + b + " scale=" + b.scale() + " unscaled=" + b.unscaledValue() + " plain=" + b.toPlainString()); }
            catch (Exception e) { System.out.println("[" + s + "] " + e.getClass().getName() + ": " + e.getMessage()); }
        }
        try { new BigDecimal(Double.NaN); } catch (Exception e) { System.out.println(e); }
        try { BigDecimal.ONE.divide(BigDecimal.ZERO); } catch (Exception e) { System.out.println(e); }
        try { BigDecimal.ZERO.divide(BigDecimal.ZERO); } catch (Exception e) { System.out.println(e); }
        try { BigDecimal.ONE.divide(new BigDecimal(3)); } catch (Exception e) { System.out.println(e); }
        try { new BigDecimal("1.5").setScale(0); } catch (Exception e) { System.out.println(e); }
        try { new BigDecimal("1.5").pow(-1); } catch (Exception e) { System.out.println(e); }
        try { new BigDecimal("1.5").intValueExact(); } catch (Exception e) { System.out.println(e); }
        try { new BigDecimal("1e20").intValueExact(); } catch (Exception e) { System.out.println(e); }
        try { new BigDecimal("-1").sqrt(MathContext.DECIMAL32); } catch (Exception e) { System.out.println(e); }
        try { new BigDecimal("2").sqrt(MathContext.UNLIMITED); } catch (Exception e) { System.out.println(e); }
        try { new MathContext(-1); } catch (Exception e) { System.out.println(e); }
        System.out.println(new BigDecimal(0.1) + " " + new BigDecimal(0.5) + " " + new BigDecimal(100.0) + " " + new BigDecimal(1e30)+ " " + new BigDecimal(-2.5e-10));
        System.out.println(BigDecimal.valueOf(0.1) + " " + BigDecimal.valueOf(100.0) + " " + BigDecimal.valueOf(1e10) + " " + BigDecimal.valueOf(5) + " " + BigDecimal.valueOf(12345, 3) + " " + BigDecimal.valueOf(1, -3));
        System.out.println(MathContext.DECIMAL64 + " " + new MathContext(5, RoundingMode.DOWN) + " " + RoundingMode.HALF_UP + " " + MathContext.UNLIMITED.getPrecision());
        BigDecimal x = new BigDecimal("123.456"), y = new BigDecimal("7.89");
        System.out.println(x.add(y) + " " + x.subtract(y) + " " + x.multiply(y) + " " + x.divide(y, 10, RoundingMode.HALF_UP) + " " + x.divide(y, RoundingMode.DOWN) + " " + x.divide(y, MathContext.DECIMAL32) + " " + x.remainder(y) + " " + x.divideToIntegralValue(y));
        System.out.println(x.setScale(1, RoundingMode.HALF_EVEN) + " " + x.setScale(5) + " " + x.setScale(-1, RoundingMode.CEILING) + " " + x.negate().setScale(0, RoundingMode.FLOOR) + " " + x.round(new MathContext(4)) + " " + x.precision() + " " + x.stripTrailingZeros() + " " + new BigDecimal("1000").stripTrailingZeros() + " " + new BigDecimal("1000").stripTrailingZeros().toPlainString());
        System.out.println(x.pow(3) + " " + x.movePointLeft(5) + " " + x.movePointRight(5) + " " + x.scaleByPowerOfTen(2) + " " + x.toBigInteger() + " " + x.intValue() + " " + x.longValue() + " " + x.doubleValue() + " " + x.floatValue() + " " + x.ulp() + " " + x.compareTo(y) + " " + x.equals(new BigDecimal("123.4560")) + " " + x.compareTo(new BigDecimal("123.4560")) + " " + x.hashCode() + " " + new BigDecimal("123.4560").hashCode() + " " + x.signum() + " " + x.negate().abs());
        System.out.println(new BigDecimal("2").sqrt(MathContext.DECIMAL64) + " " + new BigDecimal("16").sqrt(MathContext.DECIMAL64) + " " + new BigDecimal("0.25").sqrt(new MathContext(10)) + " " + new BigDecimal("1E-7").toString() + " " + new BigDecimal("1E+7").toString() + " " + new BigDecimal("1E+7").toPlainString()+ " " + new BigDecimal("0.0000001")+ " " + new BigDecimal("0.000001") + " " + new BigDecimal("12E-8") + " " + new BigDecimal("-12E+8"));
        System.out.println(new BigDecimal("10").divide(new BigDecimal("4")) + " " + new BigDecimal("1").divide(new BigDecimal("8")) + " " + new BigDecimal("100").divide(new BigDecimal("0.5")) + " " + new BigDecimal("1.00").divide(new BigDecimal("2")) + " " + new BigDecimal("1E+3").divide(new BigDecimal("2")) + " " + new BigDecimal("6").divide(new BigDecimal("2.0"))+ " " + BigDecimal.ONE.divide(new BigDecimal("3"), MathContext.DECIMAL128) + " " + new BigDecimal("2").divide(new BigDecimal("3"), new MathContext(3, RoundingMode.UP)) + " " + new BigDecimal("9.99").divide(BigDecimal.ONE, new MathContext(2, RoundingMode.HALF_UP)));
        System.out.println(String.format("%.2f|%10.3f|%s|%,.2f|%f|%e|%.0f", x, x, x, new BigDecimal("1234567.895"), y, x, new BigDecimal("2.5")));
        System.out.println(x.max(y) + " " + x.min(y) + " " + BigDecimal.TEN.add(BigDecimal.ONE) + " " + BigDecimal.ZERO.scale() + " " + BigDecimal.TWO + " " + new BigDecimal(new BigInteger("12345"), 2) + " " + new BigDecimal(new BigInteger("12345")) + " " + new BigDecimal(12L) + " " + new BigDecimal(12) + " " + x.add(y, new MathContext(4)) + " " + x.multiply(y, new MathContext(4)));
    }
}
"####;
    let (out, ok) = run(src, "Y");
    assert!(ok);
    assert_eq!(
        out,
        r####"[] java.lang.NumberFormatException: null
[abc] java.lang.NumberFormatException: Character a is neither a decimal digit number, decimal point, nor "e" notation exponential mark.
[1e] java.lang.NumberFormatException: null
[1.2.3] java.lang.NumberFormatException: Character array contains more than one decimal point.
[-] java.lang.NumberFormatException: No digits found.
[.] java.lang.NumberFormatException: No digits found.
[+.5] ok 0.5 scale=1 unscaled=5 plain=0.5
[1e+] java.lang.NumberFormatException: null
[1e5.5] java.lang.NumberFormatException: Not a digit.
[ 1] java.lang.NumberFormatException: Character   is neither a decimal digit number, decimal point, nor "e" notation exponential mark.
[1 ] java.lang.NumberFormatException: Character   is neither a decimal digit number, decimal point, nor "e" notation exponential mark.
[1_0] java.lang.NumberFormatException: Character _ is neither a decimal digit number, decimal point, nor "e" notation exponential mark.
[0x10] java.lang.NumberFormatException: Character x is neither a decimal digit number, decimal point, nor "e" notation exponential mark.
[--1] java.lang.NumberFormatException: Character - is neither a decimal digit number, decimal point, nor "e" notation exponential mark.
[1e99999999999] java.lang.NumberFormatException: Too many nonzero exponent digits.
[5.] ok 5 scale=0 unscaled=5 plain=5
[.5e-3] ok 0.0005 scale=4 unscaled=5 plain=0.0005
[-0.00] ok 0.00 scale=2 unscaled=0 plain=0.00
[1E3] ok 1E+3 scale=-3 unscaled=1 plain=1000
[0E-10] ok 0E-10 scale=10 unscaled=0 plain=0.0000000000
[00012.3400] ok 12.3400 scale=4 unscaled=123400 plain=12.3400
java.lang.NumberFormatException: Infinite or NaN
java.lang.ArithmeticException: Division by zero
java.lang.ArithmeticException: Division undefined
java.lang.ArithmeticException: Non-terminating decimal expansion; no exact representable decimal result.
java.lang.ArithmeticException: Rounding necessary
java.lang.ArithmeticException: Invalid operation
java.lang.ArithmeticException: Rounding necessary
java.lang.ArithmeticException: Overflow
java.lang.ArithmeticException: Attempted square root of negative BigDecimal
java.lang.ArithmeticException: Computed square root not exact.
java.lang.IllegalArgumentException: Digits < 0
0.1000000000000000055511151231257827021181583404541015625 0.5 100 1000000000000000019884624838656 -2.50000000000000015570397864444964104724267173196494695730507373809814453125E-10
0.1 100.0 1.0E+10 5 12.345 1E+3
precision=16 roundingMode=HALF_EVEN precision=5 roundingMode=DOWN HALF_UP 0
131.346 115.566 974.06784 15.6471482890 15.647 15.64715 5.106 15.0
123.5 123.45600 1.3E+2 -124 123.5 6 123.456 1E+3 1000
1881640.295202816 0.00123456 12345600 12345.6 123 123 123 123.456 123.456 0.001 1 false 0 3827139 38271364 1 123.456
1.414213562373095 4 0.5 1E-7 1E+7 10000000 1E-7 0.000001 1.2E-7 -1.2E+9
2.5 0.125 2.0E+2 0.50 5E+2 3 0.3333333333333333333333333333333333 0.667 10
123.46|   123.456|123.456|1,234,567.90|7.890000|1.234560e+02|3
123.456 7.89 11 0 2 123.45 12345 12 12 131.3 974.1
"####
    );
}

/// Natural ordering, hash and tree containers, stream reduction, currency arithmetic, `RoundingMode` identity.
#[test]
fn bigdecimal_flows_through_collections_money_math_and_enums() {
    let src = r####"import java.math.*;
import java.util.*;
import java.util.stream.*;
public class B1 { public static void main(String[] a){
  List<BigDecimal> l = new ArrayList<>(List.of(new BigDecimal("2.50"), new BigDecimal("10"), new BigDecimal("-1.25"), new BigDecimal("2.5")));
  Collections.sort(l); System.out.println(l);
  System.out.println(l.stream().reduce(BigDecimal.ZERO, BigDecimal::add));
  System.out.println(l.stream().map(BigDecimal::negate).sorted().toList());
  System.out.println(l.stream().max(Comparator.naturalOrder()).get() + " " + l.stream().mapToDouble(BigDecimal::doubleValue).sum());
  Set<BigDecimal> hs = new HashSet<>(l); System.out.println(hs.size() + " " + hs.contains(new BigDecimal("2.50")) + " " + hs.contains(new BigDecimal("2.500")));
  TreeSet<BigDecimal> ts = new TreeSet<>(l); System.out.println(ts.size() + " " + ts);
  Map<BigDecimal,String> m = new HashMap<>(); m.put(new BigDecimal("1.0"), "a"); m.put(new BigDecimal("1.00"), "b"); System.out.println(m.size());
  BigDecimal price = new BigDecimal("19.99"), qty = BigDecimal.valueOf(3), rate = new BigDecimal("0.0825");
  BigDecimal sub = price.multiply(qty), tax = sub.multiply(rate).setScale(2, RoundingMode.HALF_UP);
  System.out.println(sub + " " + tax + " " + sub.add(tax) + " " + String.format("Total: $%,.2f (%s)", sub.add(tax), tax));
  System.out.println(BigDecimal.valueOf(2.50) + " " + BigDecimal.valueOf(1e-5) + " " + new BigDecimal("1e-5").toPlainString() + " " + BigDecimal.valueOf(100) .divide(BigDecimal.valueOf(3), 2, RoundingMode.HALF_UP));
  System.out.println(price.compareTo(qty) > 0 ? "gt" : "le");
  System.out.println(price.equals(new BigDecimal("19.990")) + " " + price.hashCode() + " " + Objects.hash(price) + " " + (price instanceof Number) + " " + price.getClass().getSimpleName());
  BigDecimal acc = BigDecimal.ONE; for (int i = 0; i < 5; i++) acc = acc.multiply(new BigDecimal("1.1")); System.out.println(acc);
  System.out.println(new BigDecimal("1.23456789").round(new MathContext(4, RoundingMode.HALF_EVEN)) + " " + MathContext.DECIMAL64.getPrecision() + " " + MathContext.DECIMAL32.getRoundingMode());
  System.out.println(new BigDecimal(new BigInteger("12345678901234567890")).multiply(new BigDecimal("0.001")).toBigInteger());
  switchTest(RoundingMode.HALF_UP); 
  Object o = price; if (o instanceof BigDecimal bd) System.out.println(bd.scale());
  System.out.println(price.intValue() + price.longValue() / 2 + " " + (price.doubleValue() > 19));
  System.out.printf("%.3f %10.1f %s%n", new BigDecimal("2.0005"), new BigDecimal("-0.05"), new BigDecimal("1E+3"));
}
static void switchTest(RoundingMode r) { System.out.println(r == RoundingMode.HALF_UP ? "hu" : "other"); }
}
"####;
    let (out, ok) = run(src, "B1");
    assert!(ok);
    assert_eq!(
        out,
        r####"[-1.25, 2.50, 2.5, 10]
13.75
[-10, -2.50, -2.5, 1.25]
10 13.75
4 true false
3 [-1.25, 2.50, 10]
2
59.97 4.95 64.92 Total: $64.92 (4.95)
2.5 0.000010 0.00001 33.33
gt
false 61971 62002 true BigDecimal
1.61051
1.235 16 HALF_EVEN
12345678901234567
hu
2
28 true
2.001       -0.1 1E+3
"####
    );
}

/// Several thousand operations across values, scales, rounding modes and `MathContext`s, checked against the JDK as a line count, a rolling hash and sampled lines.
#[test]
fn bigdecimal_operation_matrix_matches_the_jdk_digest() {
    let src = r####"import java.math.*;
import java.util.*;
public class Z3 {
    static int lines = 0, h = 0;
    static void emit(String s) { lines++; h = 31 * h + s.hashCode(); if (lines % 600 == 1) System.out.println(s); }
    static void p(String label, java.util.function.Supplier<Object> f) {
        try { Object r = f.get(); emit(label + " = " + r + (r instanceof BigDecimal b ? " [" + b.scale() + "]" : "")); }
        catch (ArithmeticException e) { emit(label + " ! " + e.getMessage()); }
    }
    public static void main(String[] a) {
        String[] vs = {"0", "0.00", "1", "-1", "2.5", "-2.5", "3", "7", "100", "1E+3", "1.50", "123.456", "-0.001", "9.995", "1E-7", "0.25", "16", "12345678901234567890.123456789"};
        RoundingMode[] some = {RoundingMode.UP, RoundingMode.HALF_EVEN, RoundingMode.FLOOR};
        for (String x : vs) {
            BigDecimal X = new BigDecimal(x);
            p(x + " strip/plain", () -> X.stripTrailingZeros() + "|" + X.toPlainString());
            p(x + " prec/unscaled/signum/hash", () -> X.precision() + "/" + X.unscaledValue() + "/" + X.signum() + "/" + X.hashCode());
            p(x + " intv/longv/dbl", () -> X.intValue() + "/" + X.longValue() + "/" + X.doubleValue() + "/" + X.floatValue());
            p(x + " bigint", () -> X.toBigInteger());
            p(x + " exact", () -> X.toBigIntegerExact());
            p(x + " intExact", () -> X.intValueExact());
            p(x + " negabs/ulp", () -> X.negate() + "|" + X.abs() + "|" + X.ulp());
            p(x + " move", () -> X.movePointLeft(3) + "|" + X.movePointRight(3) + "|" + X.movePointRight(-2) + "|" + X.scaleByPowerOfTen(4));
            p(x + " pow", () -> X.pow(3) + "|" + X.pow(0));
            for (int sc : new int[]{-2, 0, 3}) {
                for (RoundingMode rm : RoundingMode.values()) {
                    p(x + " setScale " + sc + " " + rm, () -> X.setScale(sc, rm));
                }
            }
            for (int pr : new int[]{0, 1, 3, 20}) for (RoundingMode rm : some) {
                MathContext mc = new MathContext(pr, rm);
                p(x + " round " + mc, () -> X.round(mc));
                p(x + " sqrt " + mc, () -> X.sqrt(mc));
            }
            for (String y : vs) {
                BigDecimal Y = new BigDecimal(y);
                String l = x + " , " + y;
                p(l + " add/sub/mul", () -> X.add(Y) + "|" + X.subtract(Y) + "|" + X.multiply(Y));
                p(l + " div", () -> X.divide(Y));
                p(l + " cmp/eq/min/max", () -> X.compareTo(Y) + "/" + X.equals(Y) + "/" + X.min(Y) + "/" + X.max(Y));
                p(l + " rem", () -> X.remainder(Y));
                p(l + " dti", () -> X.divideToIntegralValue(Y));
                p(l + " mc7", () -> X.divide(Y, MathContext.DECIMAL32));
                if (Y.signum() != 0) {
                    for (RoundingMode rm : some) {
                        p(l + " div rm " + rm, () -> X.divide(Y, rm));
                        p(l + " div 4 " + rm, () -> X.divide(Y, 4, rm));
                    }
                    for (int pr : new int[]{1, 3, 16}) {
                        MathContext mc = new MathContext(pr, RoundingMode.HALF_UP);
                        p(l + " div " + mc, () -> X.divide(Y, mc));
                        p(l + " add " + mc, () -> X.add(Y, mc));
                        p(l + " mul " + mc, () -> X.multiply(Y, mc));
                    }
                }
            }
        }
        System.out.println("lines=" + lines + " hash=" + h);
    }
}
"####;
    let (out, ok) = run(src, "Z3");
    assert!(ok);
    assert_eq!(
        out,
        r####"0 strip/plain = 0|0
0.00 , 100 add/sub/mul = 100.00|-100.00|0.00
1 , 12345678901234567890.123456789 div rm UP = 1 [0]
2.5 , 7 div rm UP = 0.4 [1]
-2.5 , 16 div precision=1 roundingMode=HALF_UP = -0.2 [1]
7 , 3 div precision=1 roundingMode=HALF_UP = 2 [0]
100 , 0.25 div precision=16 roundingMode=HALF_UP = 4E+2 [-2]
1.50 , -2.5 div precision=16 roundingMode=HALF_UP = -0.6 [1]
123.456 , 0.25 rem = 0.206 [3]
9.995 , -2.5 rem = 2.495 [3]
1E-7 , 1E-7 div 4 HALF_EVEN = 1.0000 [4]
16 , 2.5 div 4 HALF_EVEN = 6.4000 [4]
12345678901234567890.123456789 , 9.995 div precision=3 roundingMode=HALF_UP = 1.24E+18 [-16]
lines=7290 hash=392876637
"####
    );
}
