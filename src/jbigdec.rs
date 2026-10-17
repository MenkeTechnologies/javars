//! `java.math.BigDecimal`, `RoundingMode` and `MathContext`: an unscaled
//! `BigInt` plus a decimal scale, following the JDK's contract — the preferred
//! scales of `divide`/`sqrt`, the canonical `toString`, scale-sensitive
//! `equals`, the string grammar and its exception messages.

use crate::jbig::Refusal;
use num_bigint::{BigInt, Sign};
use num_integer::Integer;
use num_traits::{One, Signed, ToPrimitive, Zero};
use std::cmp::Ordering;

/// `java.math.RoundingMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rm {
    Up,
    Down,
    Ceiling,
    Floor,
    HalfUp,
    HalfDown,
    HalfEven,
    Unnecessary,
}

impl Rm {
    pub const NAMES: [&'static str; 8] = [
        "UP",
        "DOWN",
        "CEILING",
        "FLOOR",
        "HALF_UP",
        "HALF_DOWN",
        "HALF_EVEN",
        "UNNECESSARY",
    ];

    /// The mode named `UP`..`UNNECESSARY`.
    pub fn from_name(name: &str) -> Option<Rm> {
        Self::NAMES
            .iter()
            .position(|n| *n == name)
            .and_then(|i| Self::from_code(i as i64))
    }

    /// The mode for a `RoundingMode` ordinal, which is also the legacy
    /// `BigDecimal.ROUND_*` constant.
    pub fn from_code(code: i64) -> Option<Rm> {
        Some(match code {
            0 => Rm::Up,
            1 => Rm::Down,
            2 => Rm::Ceiling,
            3 => Rm::Floor,
            4 => Rm::HalfUp,
            5 => Rm::HalfDown,
            6 => Rm::HalfEven,
            7 => Rm::Unnecessary,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        Self::NAMES[self as usize]
    }
}

/// `java.math.MathContext`: a digit budget and a rounding mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mc {
    pub precision: u32,
    pub mode: Rm,
}

impl Mc {
    pub const UNLIMITED: Mc = Mc {
        precision: 0,
        mode: Rm::HalfUp,
    };

    pub fn new(precision: i64, mode: Rm) -> Result<Mc, Refusal> {
        let precision = u32::try_from(precision)
            .map_err(|_| ("IllegalArgumentException", "Digits < 0".to_string()))?;
        Ok(Mc { precision, mode })
    }

    /// `DECIMAL32`, `DECIMAL64`, `DECIMAL128`, `UNLIMITED`.
    pub fn constant(name: &str) -> Option<Mc> {
        let precision = match name {
            "UNLIMITED" => return Some(Mc::UNLIMITED),
            "DECIMAL32" => 7,
            "DECIMAL64" => 16,
            "DECIMAL128" => 34,
            _ => return None,
        };
        Some(Mc {
            precision,
            mode: Rm::HalfEven,
        })
    }

    pub fn render(&self) -> String {
        format!(
            "precision={} roundingMode={}",
            self.precision,
            self.mode.name()
        )
    }
}

/// One `BigDecimal`: `unscaled * 10^-scale`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dec {
    pub unscaled: BigInt,
    pub scale: i64,
}

fn arith(msg: &str) -> Refusal {
    ("ArithmeticException", msg.to_string())
}

fn nfe(msg: &str) -> Refusal {
    ("NumberFormatException", msg.to_string())
}

fn pow10(n: u64) -> BigInt {
    num_traits::pow::pow(BigInt::from(10), n as usize)
}

/// Decimal digits of `|x|`; zero has one.
fn digits(x: &BigInt) -> u64 {
    if x.is_zero() {
        1
    } else {
        x.magnitude().to_string().len() as u64
    }
}

/// `n / d` rounded to an integer under `mode`, with whether it was exact.
fn div_round(n: &BigInt, d: &BigInt, mode: Rm) -> Result<(BigInt, bool), Refusal> {
    let (q, r) = n.div_rem(d);
    if r.is_zero() {
        return Ok((q, true));
    }
    let negative = (n.sign() == Sign::Minus) != (d.sign() == Sign::Minus);
    let away = match mode {
        Rm::Unnecessary => return Err(arith("Rounding necessary")),
        Rm::Up => true,
        Rm::Down => false,
        Rm::Ceiling => !negative,
        Rm::Floor => negative,
        Rm::HalfUp | Rm::HalfDown | Rm::HalfEven => {
            match (r.magnitude() * 2u32).cmp(d.magnitude()) {
                Ordering::Greater => true,
                Ordering::Less => false,
                Ordering::Equal => match mode {
                    Rm::HalfUp => true,
                    Rm::HalfDown => false,
                    _ => q.is_odd(),
                },
            }
        }
    };
    let q = if away {
        if negative {
            q - 1u32
        } else {
            q + 1u32
        }
    } else {
        q
    };
    Ok((q, false))
}

impl Dec {
    pub fn new(unscaled: BigInt, scale: i64) -> Dec {
        Dec { unscaled, scale }
    }

    pub fn from_int(n: BigInt) -> Dec {
        Dec::new(n, 0)
    }

    fn zero(scale: i64) -> Dec {
        Dec::new(BigInt::zero(), scale)
    }

    pub fn signum(&self) -> i32 {
        match self.unscaled.sign() {
            Sign::Minus => -1,
            Sign::NoSign => 0,
            Sign::Plus => 1,
        }
    }

    pub fn precision(&self) -> u64 {
        digits(&self.unscaled)
    }

    /// `new BigDecimal(String)`, with the JDK's checks in its order.
    pub fn parse(text: &str) -> Result<Dec, Refusal> {
        let chars: Vec<char> = text.chars().collect();
        let null = || ("NumberFormatException", String::new());
        let mut at = 0;
        let mut negative = false;
        match chars.first() {
            None => return Err(null()),
            Some('-') => {
                negative = true;
                at = 1;
            }
            Some('+') => at = 1,
            _ => {}
        }
        let mut mantissa = String::new();
        let mut dot = false;
        let mut frac = 0i64;
        let mut exp = 0i64;
        while at < chars.len() {
            let c = chars[at];
            if c.is_ascii_digit() {
                mantissa.push(c);
                if dot {
                    frac += 1;
                }
            } else if c == '.' {
                if dot {
                    return Err(nfe("Character array contains more than one decimal point."));
                }
                dot = true;
            } else if c == 'e' || c == 'E' {
                exp = parse_exponent(&chars[at + 1..])?;
                break;
            } else {
                return Err(nfe(&format!(
                    "Character {c} is neither a decimal digit number, decimal point, nor \"e\" notation exponential mark."
                )));
            }
            at += 1;
        }
        if mantissa.is_empty() {
            return Err(nfe("No digits found."));
        }
        let scale = frac - exp;
        if i32::try_from(scale).is_err() {
            return Err(nfe(if exp > 0 {
                "Scale out of range."
            } else {
                "Exponent overflow."
            }));
        }
        let magnitude: BigInt = mantissa.parse().map_err(|_| nfe("No digits found."))?;
        Ok(Dec::new(
            if negative { -magnitude } else { magnitude },
            scale,
        ))
    }

    /// `new BigDecimal(double)`: the exact binary expansion.
    pub fn from_f64(d: f64) -> Result<Dec, Refusal> {
        if !d.is_finite() {
            return Err(nfe("Infinite or NaN"));
        }
        if d == 0.0 {
            return Ok(Dec::zero(0));
        }
        let bits = d.to_bits();
        let sign = if bits >> 63 == 0 { 1 } else { -1 };
        let raw_exp = ((bits >> 52) & 0x7ff) as i64;
        let mut mant = if raw_exp == 0 {
            (bits & 0xf_ffff_ffff_ffff) << 1
        } else {
            (bits & 0xf_ffff_ffff_ffff) | 0x10_0000_0000_0000
        } as i128;
        let mut exp = raw_exp - 1075;
        while mant & 1 == 0 {
            mant >>= 1;
            exp += 1;
        }
        let mant = BigInt::from(mant * sign);
        Ok(if exp >= 0 {
            Dec::new(mant << exp as u64, 0)
        } else {
            Dec::new(
                mant * num_traits::pow::pow(BigInt::from(5), (-exp) as usize),
                -exp,
            )
        })
    }

    /// `toString()`: plain notation while the exponent allows, else scientific.
    pub fn render(&self) -> String {
        if self.scale == 0 {
            return self.unscaled.to_string();
        }
        let coeff = self.unscaled.magnitude().to_string();
        let sign = if self.unscaled.is_negative() { "-" } else { "" };
        let adjusted = -self.scale + (coeff.len() as i64 - 1);
        if self.scale >= 0 && adjusted >= -6 {
            let pad = self.scale - coeff.len() as i64;
            if pad >= 0 {
                return format!("{sign}0.{}{coeff}", "0".repeat(pad as usize));
            }
            let split = (-pad) as usize;
            return format!("{sign}{}.{}", &coeff[..split], &coeff[split..]);
        }
        let mut out = format!("{sign}{}", &coeff[..1]);
        if coeff.len() > 1 {
            out.push('.');
            out.push_str(&coeff[1..]);
        }
        if adjusted != 0 {
            out.push('E');
            if adjusted > 0 {
                out.push('+');
            }
            out.push_str(&adjusted.to_string());
        }
        out
    }

    /// `toPlainString()`: never an exponent.
    pub fn plain(&self) -> String {
        let coeff = self.unscaled.magnitude().to_string();
        let sign = if self.unscaled.is_negative() { "-" } else { "" };
        if self.scale == 0 {
            return format!("{sign}{coeff}");
        }
        if self.scale < 0 {
            if self.unscaled.is_zero() {
                return "0".to_string();
            }
            return format!("{sign}{coeff}{}", "0".repeat((-self.scale) as usize));
        }
        let pad = self.scale - coeff.len() as i64;
        if pad >= 0 {
            return format!("{sign}0.{}{coeff}", "0".repeat(pad as usize));
        }
        let split = (-pad) as usize;
        format!("{sign}{}.{}", &coeff[..split], &coeff[split..])
    }

    /// `hashCode()`: `31 * unscaledValue().hashCode() + scale`.
    pub fn hash(&self) -> i32 {
        crate::jbig::hash(&self.unscaled)
            .wrapping_mul(31)
            .wrapping_add(self.scale as i32)
    }

    fn rescaled(&self, scale: i64) -> BigInt {
        &self.unscaled * pow10((scale - self.scale) as u64)
    }

    pub fn add(&self, o: &Dec) -> Dec {
        let scale = self.scale.max(o.scale);
        Dec::new(self.rescaled(scale) + o.rescaled(scale), scale)
    }

    pub fn subtract(&self, o: &Dec) -> Dec {
        self.add(&o.negate())
    }

    pub fn multiply(&self, o: &Dec) -> Dec {
        Dec::new(&self.unscaled * &o.unscaled, self.scale + o.scale)
    }

    pub fn negate(&self) -> Dec {
        Dec::new(-&self.unscaled, self.scale)
    }

    pub fn abs(&self) -> Dec {
        Dec::new(self.unscaled.abs(), self.scale)
    }

    pub fn compare(&self, o: &Dec) -> Ordering {
        let scale = self.scale.max(o.scale);
        self.rescaled(scale).cmp(&o.rescaled(scale))
    }

    /// `setScale(n, mode)`.
    pub fn set_scale(&self, scale: i64, mode: Rm) -> Result<Dec, Refusal> {
        match scale.cmp(&self.scale) {
            Ordering::Equal => Ok(self.clone()),
            Ordering::Greater => Ok(Dec::new(self.rescaled(scale), scale)),
            Ordering::Less => {
                let (q, _) = div_round(&self.unscaled, &pow10((self.scale - scale) as u64), mode)?;
                Ok(Dec::new(q, scale))
            }
        }
    }

    /// `round(mc)`: drop digits past the precision, repeating when the
    /// rounding carries into a new digit.
    pub fn round(&self, mc: Mc) -> Result<Dec, Refusal> {
        if mc.precision == 0 {
            return Ok(self.clone());
        }
        let mut cur = self.clone();
        loop {
            let prec = cur.precision();
            if prec <= u64::from(mc.precision) {
                return Ok(cur);
            }
            let drop = prec - u64::from(mc.precision);
            let (q, _) = div_round(&cur.unscaled, &pow10(drop), mc.mode)?;
            cur = Dec::new(q, cur.scale - drop as i64);
        }
    }

    /// Strip trailing zeros while the scale stays above `preferred`.
    fn strip_to(mut self, preferred: i64) -> Dec {
        while self.scale > preferred && !self.unscaled.is_zero() {
            let (q, r) = self.unscaled.div_rem(&BigInt::from(10));
            if !r.is_zero() {
                break;
            }
            self.unscaled = q;
            self.scale -= 1;
        }
        self
    }

    pub fn strip_trailing_zeros(&self) -> Dec {
        if self.unscaled.is_zero() {
            return Dec::zero(0);
        }
        self.clone().strip_to(i64::MIN)
    }

    fn divide_check(&self, o: &Dec) -> Result<(), Refusal> {
        if o.unscaled.is_zero() {
            return Err(arith(if self.unscaled.is_zero() {
                "Division undefined"
            } else {
                "Division by zero"
            }));
        }
        Ok(())
    }

    /// `divide(x, scale, mode)`: the quotient rounded at a fixed scale.
    pub fn divide_scale(&self, o: &Dec, scale: i64, mode: Rm) -> Result<Dec, Refusal> {
        // The JDK divides `long`s here when it can and `BigInteger`s otherwise,
        // and the two refuse a zero divisor in their own words.
        if o.unscaled.is_zero() {
            return Err(arith(if self.unscaled.to_i64().is_some() {
                "/ by zero"
            } else {
                "BigInteger divide by zero"
            }));
        }
        // quotient = self/o at `scale`: unscaled = self.u * 10^(scale - self.s + o.s) / o.u
        let shift = scale - self.scale + o.scale;
        let (num, den) = if shift >= 0 {
            (&self.unscaled * pow10(shift as u64), o.unscaled.clone())
        } else {
            (self.unscaled.clone(), &o.unscaled * pow10((-shift) as u64))
        };
        let (q, _) = div_round(&num, &den, mode)?;
        Ok(Dec::new(q, scale))
    }

    /// `divide(x, mc)` with a nonzero precision: `mc.precision` digits, stripped
    /// back toward the preferred scale when the quotient is exact.
    fn divide_mc(&self, o: &Dec, mc: Mc) -> Result<Dec, Refusal> {
        self.divide_check(o)?;
        let preferred = self.scale - o.scale;
        if self.unscaled.is_zero() {
            return Ok(Dec::zero(preferred));
        }
        let mcp = i64::from(mc.precision);
        let a = self.unscaled.abs();
        let b = o.unscaled.abs();
        let (da, db) = (digits(&a) as i64, digits(&b) as i64);
        // A scale that yields at least `mcp` digits of the truncated quotient.
        let t0 = mcp - (da - db) + 2;
        let at = |t: i64| -> (BigInt, BigInt) {
            if t >= 0 {
                (&a * pow10(t as u64), b.clone())
            } else {
                (a.clone(), &b * pow10((-t) as u64))
            }
        };
        let (n0, d0) = at(t0);
        let d = digits(&(n0 / d0)) as i64;
        let t = t0 - (d - mcp);
        let (n, den) = at(t);
        let negative = (self.unscaled.sign() == Sign::Minus) != (o.unscaled.sign() == Sign::Minus);
        let signed = if negative { -n } else { n };
        let (mut q, exact) = div_round(&signed, &den, mc.mode)?;
        let mut scale = t + preferred;
        if digits(&q) as i64 > mcp {
            q /= 10;
            scale -= 1;
        }
        let out = Dec::new(q, scale);
        Ok(if exact { out.strip_to(preferred) } else { out })
    }

    /// `divide(x)`: the exact quotient, or the JDK's non-terminating refusal.
    pub fn divide_exact(&self, o: &Dec) -> Result<Dec, Refusal> {
        self.divide_check(o)?;
        let preferred = self.scale - o.scale;
        if self.unscaled.is_zero() {
            return Ok(Dec::zero(preferred));
        }
        let g = self.unscaled.gcd(&o.unscaled);
        let mut den = (&o.unscaled / &g).abs();
        let (mut twos, mut fives) = (0u32, 0u32);
        while den.is_even() {
            den /= 2;
            twos += 1;
        }
        let five = BigInt::from(5);
        while (&den % &five).is_zero() {
            den /= &five;
            fives += 1;
        }
        if !den.is_one() {
            return Err(arith(
                "Non-terminating decimal expansion; no exact representable decimal result.",
            ));
        }
        let k = u64::from(twos.max(fives));
        let q = &self.unscaled * pow10(k) / &o.unscaled;
        Ok(Dec::new(q, preferred + k as i64).strip_to(preferred))
    }

    pub fn divide_with(&self, o: &Dec, mc: Mc) -> Result<Dec, Refusal> {
        if mc.precision == 0 {
            return self.divide_exact(o);
        }
        self.divide_mc(o, mc)
    }

    /// `divideToIntegralValue(x)`, ported from the JDK's own steps.
    pub fn divide_to_integral(&self, o: &Dec) -> Result<Dec, Refusal> {
        self.divide_check(o)?;
        let preferred = self.scale - o.scale;
        if self.abs().compare(&o.abs()) == Ordering::Less {
            return Ok(Dec::zero(preferred));
        }
        let max_digits = self.precision()
            + (10 * o.precision()).div_ceil(3)
            + (self.scale - o.scale).unsigned_abs()
            + 2;
        let mc = Mc {
            precision: u32::try_from(max_digits).unwrap_or(u32::MAX),
            mode: Rm::Down,
        };
        let mut q = self.divide_mc(o, mc)?;
        if q.scale > 0 {
            q = q.set_scale(0, Rm::Down)?.strip_to(preferred);
        }
        if q.scale < preferred {
            q = q.set_scale(preferred, Rm::Unnecessary)?;
        }
        Ok(q)
    }

    pub fn remainder(&self, o: &Dec) -> Result<Dec, Refusal> {
        let q = self.divide_to_integral(o)?;
        Ok(self.subtract(&q.multiply(o)))
    }

    pub fn pow(&self, n: i64) -> Result<Dec, Refusal> {
        if !(0..=999_999_999).contains(&n) {
            return Err(arith("Invalid operation"));
        }
        Ok(Dec::new(
            num_traits::pow::pow(self.unscaled.clone(), n as usize),
            self.scale * n,
        ))
    }

    /// `sqrt(mc)`, JDK 9+: correctly rounded to the precision, exact roots
    /// stripped back toward `scale / 2`.
    pub fn sqrt(&self, mc: Mc) -> Result<Dec, Refusal> {
        match self.signum() {
            -1 => return Err(arith("Attempted square root of negative BigDecimal")),
            0 => return Ok(Dec::zero(self.scale / 2)),
            _ => {}
        }
        let preferred = self.scale / 2;
        if mc.precision == 0 {
            // Unlimited: only an exactly representable root is allowed.
            let s = self.strip_trailing_zeros();
            let (u, sc) = if s.scale % 2 == 0 {
                (s.unscaled.clone(), s.scale)
            } else {
                (s.unscaled.clone() * 10u32, s.scale + 1)
            };
            let root = u.sqrt();
            if &root * &root != u {
                return Err(arith("Computed square root not exact."));
            }
            let out = Dec::new(root, sc / 2);
            return Ok(if out.scale < preferred {
                out.set_scale(preferred, Rm::Unnecessary)?
            } else {
                out.strip_to(preferred)
            });
        }
        let mcp = u64::from(mc.precision);
        // Scale the radicand to an even exponent with 2*(mcp+1) digits or more,
        // so the integer root carries at least mcp+1 digits.
        let mut u = self.unscaled.clone();
        let mut sc = self.scale;
        if sc % 2 != 0 {
            u *= 10u32;
            sc += 1;
        }
        let want = 2 * (mcp + 2);
        let have = digits(&u);
        let extra = if have < want {
            (want - have).div_ceil(2) * 2
        } else {
            0
        };
        u *= pow10(extra);
        sc += extra as i64;
        let root = u.sqrt();
        let exact_root = &root * &root == u;
        // `root` is truncated; fold the sticky bit in as an extra low digit so
        // directed and half rounding see an inexact value.
        let (r10, scale) = if exact_root {
            (root, sc / 2)
        } else {
            (root * 10u32 + 1u32, sc / 2 + 1)
        };
        if !exact_root && mc.mode == Rm::Unnecessary {
            return Err(arith("Computed square root not exact."));
        }
        let rounded = Dec::new(r10, scale).round(mc)?.strip_to(preferred);
        // The JDK finishes by adding a zero of the preferred scale at the requested
        // precision: scale at least `preferred`, digits at most `mc.precision`.
        let scaled = if rounded.scale < preferred {
            rounded.set_scale(preferred, Rm::Unnecessary)?
        } else {
            rounded
        };
        scaled.round(Mc {
            precision: mc.precision,
            mode: Rm::Unnecessary,
        })
    }

    pub fn to_big_integer(&self, mode: Rm) -> Result<BigInt, Refusal> {
        Ok(self.set_scale(0, mode)?.unscaled)
    }

    /// `movePointLeft(n)` / `movePointRight(n)` (`n` already signed rightward).
    pub fn move_point(&self, right: i64) -> Dec {
        if right == 0 {
            return self.clone();
        }
        let scale = self.scale - right;
        let moved = Dec::new(self.unscaled.clone(), scale);
        if scale < 0 {
            Dec::new(moved.rescaled(0), 0)
        } else {
            moved
        }
    }

    pub fn double_value(&self) -> f64 {
        self.render().parse().unwrap_or(f64::NAN)
    }

    pub fn ulp(&self) -> Dec {
        Dec::new(BigInt::one(), self.scale)
    }
}

/// The exponent digits after an `e`/`E`, with the JDK's refusals.
fn parse_exponent(rest: &[char]) -> Result<i64, Refusal> {
    let null = || ("NumberFormatException", String::new());
    let mut at = 0;
    let mut negative = false;
    match rest.first() {
        None => return Err(null()),
        Some('-') => {
            negative = true;
            at = 1;
        }
        Some('+') => at = 1,
        _ => {}
    }
    if at >= rest.len() {
        return Err(null());
    }
    let digits: Vec<char> = rest[at..]
        .iter()
        .copied()
        .skip_while(|&c| c == '0')
        .collect();
    if digits.len() > 10 {
        return Err(nfe("Too many nonzero exponent digits."));
    }
    let mut value = 0i64;
    for c in &digits {
        let Some(d) = c.to_digit(10) else {
            return Err(nfe("Not a digit."));
        };
        value = value * 10 + i64::from(d);
    }
    // A run of only zeros was skipped whole; check what remained was digits.
    if rest[at..].iter().any(|c| !c.is_ascii_digit()) {
        return Err(nfe("Not a digit."));
    }
    if value > i64::from(i32::MAX) {
        return Err(nfe("Exponent overflow."));
    }
    Ok(if negative { -value } else { value })
}

/// `intValueExact` / `longValueExact`: the integer part must be the whole value
/// and fit.
pub fn exact_i64(d: &Dec, bits: u32) -> Result<i64, Refusal> {
    // The JDK's own order of refusals: too many integer digits is an overflow
    // before any fractional digit is looked at.
    if d.precision() as i64 - d.scale > 19 {
        return Err(arith("Overflow"));
    }
    if d.signum() == 0 {
        return Ok(0);
    }
    if d.precision() as i64 - d.scale <= 0 {
        return Err(arith("Rounding necessary"));
    }
    let whole = d.to_big_integer(Rm::Unnecessary)?;
    let n = whole.to_i64().ok_or_else(|| arith("Overflow"))?;
    if bits == 32 && i32::try_from(n).is_err() {
        return Err(arith("Overflow"));
    }
    Ok(n)
}
