//! `java.math.BigInteger`. Each program below was run on a real JDK (25) and
//! its stdout frozen, so the suite needs no JDK in CI.

use std::process::Command;

fn run(src: &str, class: &str) -> (String, bool) {
    let dir = std::env::temp_dir().join(format!("javars_bigint_{}_{}", class, std::process::id()));
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

/// Constructor and arithmetic exception messages, truncating versus flooring
/// division, two's-complement bit operations on negatives, `hashCode`, narrowing
/// conversions, radix rendering, byte arrays and `Formatter` integration.
#[test]
fn biginteger_matches_the_jdk_on_messages_bit_ops_and_conversions() {
    let src = r####"import java.math.*;
public class W {
    static void t(Runnable r){ try{ r.run(); }catch(Exception e){ System.out.println(e.getClass().getName()+": "+e.getMessage()); } }
    public static void main(String[] a){
        t(()->new BigInteger("abc"));
        t(()->new BigInteger(""));
        t(()->new BigInteger("-"));
        t(()->new BigInteger("1_0"));
        t(()->new BigInteger("12", 99));
        t(()->new BigInteger("1-2"));
        t(()->new BigInteger("z", 10));
        t(()->new BigInteger(" 1"));
        t(()->BigInteger.ONE.divide(BigInteger.ZERO));
        t(()->BigInteger.ONE.mod(BigInteger.ZERO));
        t(()->BigInteger.ONE.remainder(BigInteger.ZERO));
        t(()->BigInteger.ONE.pow(-1));
        t(()->BigInteger.TWO.modInverse(BigInteger.TWO));
        t(()->BigInteger.TWO.modPow(BigInteger.ONE, BigInteger.valueOf(-3)));
        t(()->BigInteger.TWO.shiftLeft(1).intValueExact());
        t(()->BigInteger.TWO.pow(40).intValueExact());
        t(()->BigInteger.TWO.pow(70).longValueExact());
        t(()->BigInteger.valueOf(-4).sqrt());
        t(()->BigInteger.TEN.testBit(-1));
        t(()->System.out.println(new BigInteger("+0012")+" "+new BigInteger("-ff",16)+" "+new BigInteger("-0")));
        t(()->System.out.println(BigInteger.valueOf(-5).mod(BigInteger.valueOf(3))+" "+BigInteger.valueOf(-5).remainder(BigInteger.valueOf(3))+" "+BigInteger.valueOf(-5).divide(BigInteger.valueOf(3))));
        t(()->System.out.println(BigInteger.valueOf(-5).hashCode()+" "+BigInteger.TWO.pow(70).hashCode()+" "+BigInteger.TWO.pow(70).negate().hashCode()+" "+BigInteger.ZERO.hashCode()));
        t(()->System.out.println(BigInteger.TWO.pow(70).intValue()+" "+BigInteger.TWO.pow(70).add(BigInteger.valueOf(-3)).longValue()+" "+BigInteger.TWO.pow(70).doubleValue()+" "+BigInteger.TWO.pow(70).negate().bitLength()+" "+BigInteger.valueOf(-8).bitLength()+" "+BigInteger.valueOf(-8).bitCount()+" "+BigInteger.valueOf(-8).getLowestSetBit()));
        t(()->System.out.println(BigInteger.valueOf(-8).shiftRight(1)+" "+BigInteger.valueOf(-9).shiftRight(1)+" "+BigInteger.valueOf(-9).and(BigInteger.valueOf(6))+" "+BigInteger.valueOf(-9).not()+" "+BigInteger.valueOf(-9).or(BigInteger.valueOf(6))+" "+BigInteger.valueOf(-9).xor(BigInteger.valueOf(6))));
        t(()->System.out.println(BigInteger.valueOf(255).toString(16)+" "+BigInteger.valueOf(-255).toString(2)+" "+BigInteger.valueOf(35).toString(36)+" "+BigInteger.valueOf(35).toString(99)));
        t(()->System.out.println(java.util.Arrays.toString(BigInteger.valueOf(-17).divideAndRemainder(BigInteger.valueOf(5)))+" "+BigInteger.valueOf(48).gcd(BigInteger.valueOf(-18))+" "+BigInteger.valueOf(3).modInverse(BigInteger.valueOf(11))+" "+BigInteger.valueOf(3).modPow(BigInteger.valueOf(-1),BigInteger.valueOf(11))));
        t(()->System.out.println(BigInteger.valueOf(97).isProbablePrime(20)+" "+BigInteger.valueOf(100).nextProbablePrime()+" "+BigInteger.TEN.pow(30).sqrt()+" "+BigInteger.valueOf(-3).compareTo(BigInteger.TWO)+" "+BigInteger.TEN.equals(BigInteger.valueOf(10))+" "+BigInteger.TEN.equals(10)));
        t(()->System.out.println(BigInteger.valueOf(5).setBit(70)+" "+BigInteger.valueOf(7).clearBit(1)+" "+BigInteger.valueOf(7).flipBit(3)+" "+BigInteger.valueOf(-1).testBit(100)+" "+BigInteger.valueOf(Long.MIN_VALUE).abs()+" "+BigInteger.valueOf(0).signum()));
        t(()->System.out.println(java.util.Arrays.toString(BigInteger.valueOf(-129).toByteArray())+" "+new BigInteger(new byte[]{(byte)0xff,1})+" "+new BigInteger(1,new byte[]{(byte)0xff,1})+" "+BigInteger.valueOf(1).longValue()+" "+BigInteger.valueOf(3).floatValue()+" "+BigInteger.TEN.pow(400).doubleValue()+" "+new BigInteger("123456789012345678901234567890").doubleValue()));
        t(()->System.out.println(String.format("%d %5d %,d %x %s", BigInteger.TEN.pow(20), BigInteger.TEN, BigInteger.TEN.pow(7), BigInteger.valueOf(255), BigInteger.ONE)));
    }
}
"####;
    let (out, ok) = run(src, "W");
    assert!(ok);
    assert_eq!(
        out,
        r####"java.lang.NumberFormatException: For input string: "abc"
java.lang.NumberFormatException: Zero length BigInteger
java.lang.NumberFormatException: Zero length BigInteger
java.lang.NumberFormatException: For input string: "1_0"
java.lang.NumberFormatException: Radix out of range
java.lang.NumberFormatException: Illegal embedded sign character
java.lang.NumberFormatException: For input string: "z"
java.lang.NumberFormatException: For input string: " 1"
java.lang.ArithmeticException: BigInteger divide by zero
java.lang.ArithmeticException: BigInteger: modulus not positive
java.lang.ArithmeticException: BigInteger divide by zero
java.lang.ArithmeticException: Negative exponent
java.lang.ArithmeticException: BigInteger not invertible.
java.lang.ArithmeticException: BigInteger: modulus not positive
java.lang.ArithmeticException: BigInteger out of int range
java.lang.ArithmeticException: BigInteger out of long range
java.lang.ArithmeticException: Negative BigInteger
java.lang.ArithmeticException: Negative bit address
12 -255 0
1 -2 -1
-5 61504 -61504 0
0 -3 1.1805916207174113E21 70 3 3 3
-4 -5 6 8 -9 -15
ff -11111111 z 35
[-3, -2] 6 4 4
true 101 1000000000000000 -1 true false
1180591620717411303429 5 15 true 9223372036854775808 0
[-1, 127] -255 65281 1 3.0 Infinity 1.2345678901234568E29
100000000000000000000    10 10,000,000 ff 1
"####
    );
}

/// Natural ordering, hash-container keying, stream reduction through
/// `BigInteger::add`, string conversion and `instanceof` against the supertypes.
#[test]
fn biginteger_orders_hashes_and_flows_through_collections_and_streams() {
    let src = r####"import java.math.BigInteger;
import java.util.*;
public class X { public static void main(String[] a){
 BigInteger x = BigInteger.valueOf(-3), y = BigInteger.TWO;
 System.out.println(x.compareTo(y));
 System.out.println(BigInteger.valueOf(-3).compareTo(BigInteger.TWO));
 List<BigInteger> l = new ArrayList<>(List.of(y, x, BigInteger.TEN)); Collections.sort(l); System.out.println(l);
 TreeSet<BigInteger> ts = new TreeSet<>(l); System.out.println(ts + " " + Collections.max(l));
 Set<BigInteger> hs = new HashSet<>(l); hs.add(BigInteger.TWO); System.out.println(hs + " " + hs.contains(BigInteger.valueOf(10)));
 Map<BigInteger,String> m = new HashMap<>(); m.put(BigInteger.ONE,"a"); m.put(BigInteger.valueOf(1),"b"); System.out.println(m);
 BigInteger f = BigInteger.ONE; for (int i=1;i<=30;i++) f = f.multiply(BigInteger.valueOf(i)); System.out.println(f + " " + f.toString().length());
 System.out.println(l.stream().reduce(BigInteger.ZERO, BigInteger::add));
 System.out.println(l.stream().map(BigInteger::negate).toList());
 String s = "n=" + f; System.out.println(s); System.out.println(String.valueOf(f)); StringBuilder sb = new StringBuilder(); sb.append(f); System.out.println(sb);
 System.out.printf("%d|%s|%10d|%x|%,d|%08d%n", f, f, f, f, f, BigInteger.valueOf(-42));
 System.out.println(f.hashCode() + " " + (f.equals(f)) + " " + (f == f) + " " + f.getClass().getName() + " " + (f instanceof BigInteger) + " " + (f instanceof Number) + " " + (f instanceof Comparable));
 Object o = f; System.out.println(o instanceof BigInteger b2 ? b2.bitLength() : -1);
 System.out.println(f.intValue() / 7 + " " + f.longValue() % 10);
 System.out.println(Objects.equals(f, new BigInteger("265252859812191058636308480000000")) + " " + Objects.hash(f));
}}
"####;
    let (out, ok) = run(src, "X");
    assert!(ok);
    assert_eq!(
        out,
        r####"-1
-1
[-3, 2, 10]
[-3, 2, 10] 10
[-3, 2, 10] true
{1=b}
265252859812191058636308480000000 33
9
[3, -2, -10]
n=265252859812191058636308480000000
265252859812191058636308480000000
265252859812191058636308480000000
265252859812191058636308480000000|265252859812191058636308480000000|265252859812191058636308480000000|d13f6370f96865df5dd54000000|265,252,859,812,191,058,636,308,480,000,000|-0000042
-469516826 true true java.math.BigInteger true true true
108
201326592 -4
true -469516795
"####
    );
}
