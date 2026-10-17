//! `java.math.BigInteger`: the arithmetic of `num-bigint` behind the JDK's
//! contract — truncating `divide`/`remainder`, always-non-negative `mod`,
//! two's-complement bitwise semantics on negatives, the exact exception
//! messages, `hashCode` over the big-endian 32-bit magnitude words, and the
//! low-bits narrowing of `intValue`/`longValue`.

use num_bigint::{BigInt, Sign};
use num_integer::Integer;
use num_traits::{One, Signed, ToPrimitive, Zero};

/// A failure the JDK reports as an exception: (class, message).
pub type Refusal = (&'static str, String);

fn arith(msg: &str) -> Refusal {
    ("ArithmeticException", msg.to_string())
}

/// `Integer.parseInt`'s per-group digit count in `BigInteger(String, int)`.
const DIGITS_PER_INT: [usize; 37] = [
    0, 0, 30, 19, 15, 13, 11, 11, 10, 9, 9, 8, 8, 8, 8, 7, 7, 7, 7, 7, 7, 7, 6, 6, 6, 6, 6, 6, 6,
    6, 6, 6, 6, 6, 6, 6, 5,
];

/// `new BigInteger(String, radix)`, with the constructor's checks in the
/// constructor's order and its group-wise `Integer.parseInt` failure text.
pub fn parse(text: &str, radix: i64) -> Result<BigInt, Refusal> {
    let nfe = |m: String| ("NumberFormatException", m);
    if !(2..=36).contains(&radix) {
        return Err(nfe("Radix out of range".into()));
    }
    let radix = radix as u32;
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Err(nfe("Zero length BigInteger".into()));
    }
    let mut sign = 1;
    let mut cursor = 0;
    let minus = chars.iter().rposition(|&c| c == '-');
    let plus = chars.iter().rposition(|&c| c == '+');
    if let Some(i) = minus {
        if i != 0 || plus.is_some() {
            return Err(nfe("Illegal embedded sign character".into()));
        }
        sign = -1;
        cursor = 1;
    } else if let Some(i) = plus {
        if i != 0 {
            return Err(nfe("Illegal embedded sign character".into()));
        }
        cursor = 1;
    }
    if cursor == chars.len() {
        return Err(nfe("Zero length BigInteger".into()));
    }
    while cursor < chars.len() && chars[cursor].to_digit(radix) == Some(0) {
        cursor += 1;
    }
    if cursor == chars.len() {
        return Ok(BigInt::zero());
    }
    let per = DIGITS_PER_INT[radix as usize];
    let digits = chars.len() - cursor;
    let mut group_len = digits % per;
    if group_len == 0 {
        group_len = per;
    }
    let mut mag = BigInt::zero();
    while cursor < chars.len() {
        let end = cursor + group_len;
        let group = &chars[cursor..end];
        let mut value = 0u64;
        for &c in group {
            let Some(d) = c.to_digit(radix) else {
                let shown: String = group.iter().collect();
                return Err(nfe(if radix == 10 {
                    format!("For input string: \"{shown}\"")
                } else {
                    format!("For input string: \"{shown}\" under radix {radix}")
                }));
            };
            value = value * u64::from(radix) + u64::from(d);
        }
        mag = mag * BigInt::from(u64::from(radix).pow(group.len() as u32)) + BigInt::from(value);
        cursor = end;
        group_len = per;
    }
    Ok(if sign < 0 { -mag } else { mag })
}

/// `x.toString(radix)`; a radix outside `2..=36` falls back to 10.
pub fn to_string_radix(x: &BigInt, radix: i64) -> String {
    let radix = if (2..=36).contains(&radix) { radix } else { 10 };
    x.to_str_radix(radix as u32)
}

pub fn divide(a: &BigInt, b: &BigInt) -> Result<BigInt, Refusal> {
    if b.is_zero() {
        return Err(arith("BigInteger divide by zero"));
    }
    Ok(a / b)
}

pub fn remainder(a: &BigInt, b: &BigInt) -> Result<BigInt, Refusal> {
    if b.is_zero() {
        return Err(arith("BigInteger divide by zero"));
    }
    Ok(a % b)
}

pub fn modulo(a: &BigInt, m: &BigInt) -> Result<BigInt, Refusal> {
    if m.sign() != Sign::Plus {
        return Err(arith("BigInteger: modulus not positive"));
    }
    Ok(a.mod_floor(m))
}

pub fn pow(a: &BigInt, e: i64) -> Result<BigInt, Refusal> {
    if e < 0 {
        return Err(arith("Negative exponent"));
    }
    let e = u32::try_from(e).map_err(|_| arith("BigInteger would overflow supported range"))?;
    Ok(num_traits::pow::pow(a.clone(), e as usize))
}

/// `a.modInverse(m)` by the extended Euclid algorithm.
pub fn mod_inverse(a: &BigInt, m: &BigInt) -> Result<BigInt, Refusal> {
    if m.sign() != Sign::Plus {
        return Err(arith("BigInteger: modulus not positive"));
    }
    if m.is_one() {
        return Ok(BigInt::zero());
    }
    let ext = a.mod_floor(m).extended_gcd(m);
    if !ext.gcd.is_one() {
        return Err(arith("BigInteger not invertible."));
    }
    Ok(ext.x.mod_floor(m))
}

pub fn mod_pow(base: &BigInt, exp: &BigInt, m: &BigInt) -> Result<BigInt, Refusal> {
    if m.sign() != Sign::Plus {
        return Err(arith("BigInteger: modulus not positive"));
    }
    let (base, exp) = if exp.is_negative() {
        (mod_inverse(base, m)?, -exp)
    } else {
        (base.clone(), exp.clone())
    };
    Ok(base.mod_floor(m).modpow(&exp, m))
}

pub fn sqrt(a: &BigInt) -> Result<BigInt, Refusal> {
    if a.is_negative() {
        return Err(arith("Negative BigInteger"));
    }
    Ok(a.sqrt())
}

/// The low 64 bits of the two's-complement form.
fn low64(a: &BigInt) -> u64 {
    let low = a.iter_u64_digits().next().unwrap_or(0);
    if a.is_negative() {
        low.wrapping_neg()
    } else {
        low
    }
}

pub fn int_value(a: &BigInt) -> i32 {
    low64(a) as i32
}

pub fn long_value(a: &BigInt) -> i64 {
    low64(a) as i64
}

pub fn int_value_exact(a: &BigInt) -> Result<i32, Refusal> {
    a.to_i32()
        .ok_or_else(|| arith("BigInteger out of int range"))
}

pub fn long_value_exact(a: &BigInt) -> Result<i64, Refusal> {
    a.to_i64()
        .ok_or_else(|| arith("BigInteger out of long range"))
}

/// `doubleValue`: round-half-even, `Infinity` past the double range.
pub fn double_value(a: &BigInt) -> f64 {
    a.to_f64().unwrap_or(if a.is_negative() {
        f64::NEG_INFINITY
    } else {
        f64::INFINITY
    })
}

/// `bitLength`: bits of the minimal two's-complement form, excluding the sign.
pub fn bit_length(a: &BigInt) -> u64 {
    if a.is_negative() {
        (-a - 1u32).bits()
    } else {
        a.bits()
    }
}

/// `bitCount`: bits that differ from the sign bit.
pub fn bit_count(a: &BigInt) -> u64 {
    let ones = if a.is_negative() { !a } else { a.clone() };
    ones.magnitude().count_ones()
}

pub fn lowest_set_bit(a: &BigInt) -> i64 {
    a.magnitude().trailing_zeros().map_or(-1, |z| z as i64)
}

fn bit_index(n: i64) -> Result<u64, Refusal> {
    u64::try_from(n).map_err(|_| arith("Negative bit address"))
}

pub fn test_bit(a: &BigInt, n: i64) -> Result<bool, Refusal> {
    let n = bit_index(n)?;
    Ok(((a >> n) & BigInt::one()).is_one())
}

pub fn set_bit(a: &BigInt, n: i64, on: Option<bool>) -> Result<BigInt, Refusal> {
    let mask = BigInt::one() << bit_index(n)?;
    Ok(match on {
        Some(true) => a | mask,
        Some(false) => a & !mask,
        None => a ^ mask,
    })
}

pub fn shift(a: &BigInt, n: i64) -> BigInt {
    if n >= 0 {
        a << n as u64
    } else {
        a >> n.unsigned_abs()
    }
}

/// `hashCode`: `31 * h + word` over the big-endian 32-bit magnitude words,
/// negated for a negative value.
pub fn hash(a: &BigInt) -> i32 {
    let words = a.magnitude().to_u32_digits();
    let h = words
        .iter()
        .rev()
        .fold(0i32, |h, &w| h.wrapping_mul(31).wrapping_add(w as i32));
    match a.sign() {
        Sign::Minus => h.wrapping_neg(),
        _ => h,
    }
}

/// `toByteArray`: big-endian two's complement, minimal length.
pub fn to_bytes(a: &BigInt) -> Vec<i8> {
    a.to_signed_bytes_be()
        .into_iter()
        .map(|b| b as i8)
        .collect()
}

/// `new BigInteger(byte[])`: big-endian two's complement.
pub fn from_bytes(bytes: &[i8]) -> Result<BigInt, Refusal> {
    if bytes.is_empty() {
        return Err(("NumberFormatException", "Zero length BigInteger".into()));
    }
    let raw: Vec<u8> = bytes.iter().map(|&b| b as u8).collect();
    Ok(BigInt::from_signed_bytes_be(&raw))
}

/// `new BigInteger(signum, magnitude)`.
pub fn from_sign_magnitude(signum: i64, bytes: &[i8]) -> Result<BigInt, Refusal> {
    if !(-1..=1).contains(&signum) {
        return Err(("NumberFormatException", "Invalid signum value".into()));
    }
    let raw: Vec<u8> = bytes.iter().map(|&b| b as u8).collect();
    let mag = BigInt::from_bytes_be(Sign::Plus, &raw);
    if signum == 0 && !mag.is_zero() {
        return Err(("NumberFormatException", "signum-magnitude mismatch".into()));
    }
    Ok(if signum < 0 { -mag } else { mag })
}

/// `isProbablePrime`: deterministic Miller-Rabin over the first twelve prime
/// bases, exact below 3.3e24 and overwhelmingly reliable beyond.
pub fn is_probable_prime(a: &BigInt) -> bool {
    let n = a.abs();
    let two = BigInt::from(2);
    if n < two {
        return false;
    }
    const BASES: [u32; 12] = [2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37];
    for p in BASES {
        let p = BigInt::from(p);
        if n == p {
            return true;
        }
        if (&n % &p).is_zero() {
            return false;
        }
    }
    let n1 = &n - 1u32;
    let s = n1.magnitude().trailing_zeros().unwrap_or(0);
    let d = &n1 >> s;
    'bases: for p in BASES {
        let mut x = BigInt::from(p).modpow(&d, &n);
        if x.is_one() || x == n1 {
            continue;
        }
        for _ in 1..s {
            x = x.modpow(&two, &n);
            if x == n1 {
                continue 'bases;
            }
        }
        return false;
    }
    true
}

pub fn next_probable_prime(a: &BigInt) -> Result<BigInt, Refusal> {
    if a.is_negative() {
        return Err(arith(&format!("start < 0: {a}")));
    }
    let mut n = a + 1u32;
    while !is_probable_prime(&n) {
        n += 1u32;
    }
    Ok(n)
}
