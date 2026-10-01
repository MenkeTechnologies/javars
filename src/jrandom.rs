//! `java.util.Random`: the JDK's 48-bit linear congruential generator.
//!
//! Ported from `java.base/java/util/Random.java` and, for the methods
//! `Random` inherits rather than overrides (`nextInt(origin, bound)`,
//! `nextLong(bound)`, `nextDouble(bound)` and their ranged forms), from the
//! `RandomGenerator` defaults and `jdk.internal.util.random.RandomSupport`
//! (openjdk 27). A seeded generator therefore yields the JDK's sequence
//! exactly: `new Random(42).nextInt(100)` is 0 in both.
//!
//! An unseeded `new Random()` is seeded the JDK's way — a uniquifier stepped
//! by a fixed multiplier, xored with a nanosecond clock — so its sequence is
//! as unpredictable as the JDK's and no more meaningful to compare.

use std::cell::Cell;

const MULTIPLIER: i64 = 0x5_DEEC_E66D;
const ADDEND: i64 = 0xB;
const MASK: i64 = (1 << 48) - 1;

/// `RandomSupport.BAD_BOUND`.
pub const BAD_BOUND: &str = "bound must be positive";
/// `RandomSupport.BAD_RANGE`.
pub const BAD_RANGE: &str = "bound must be greater than origin";
/// `RandomSupport.BAD_FLOATING_BOUND`.
pub const BAD_FLOATING_BOUND: &str = "bound must be finite and positive";

thread_local! {
    /// `Random.seedUniquifier`, stepped once per unseeded construction.
    static UNIQUIFIER: Cell<i64> = const { Cell::new(8_682_522_807_148_012) };
}

/// One `java.util.Random`: the 48-bit seed and the second Gaussian of the
/// last pair `nextGaussian` computed.
#[derive(Clone, Debug)]
pub struct Random {
    seed: i64,
    next_gaussian: Option<f64>,
}

/// `(seed ^ multiplier) & mask`.
fn initial_scramble(seed: i64) -> i64 {
    (seed ^ MULTIPLIER) & MASK
}

impl Random {
    /// `new Random(seed)`.
    pub fn new(seed: i64) -> Self {
        Random {
            seed: initial_scramble(seed),
            next_gaussian: None,
        }
    }

    /// `new Random()`: `seedUniquifier() ^ System.nanoTime()`.
    pub fn unseeded() -> Self {
        let u = UNIQUIFIER.with(|c| {
            let next = c.get().wrapping_mul(1_181_783_497_276_652_981);
            c.set(next);
            next
        });
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as i64);
        Random::new(u ^ nanos)
    }

    /// `setSeed(seed)`, which also forgets a pending Gaussian.
    pub fn set_seed(&mut self, seed: i64) {
        self.seed = initial_scramble(seed);
        self.next_gaussian = None;
    }

    /// `next(bits)`: step the generator and answer its top `bits` bits.
    fn next(&mut self, bits: u32) -> i32 {
        self.seed = (self.seed.wrapping_mul(MULTIPLIER).wrapping_add(ADDEND)) & MASK;
        ((self.seed as u64) >> (48 - bits)) as u32 as i32
    }

    /// `nextInt()`.
    pub fn next_int(&mut self) -> i32 {
        self.next(32)
    }

    /// `nextInt(bound)`, Random's own override.
    pub fn next_int_bounded(&mut self, bound: i32) -> Result<i32, &'static str> {
        if bound <= 0 {
            return Err(BAD_BOUND);
        }
        let mut r = self.next(31);
        let m = bound - 1;
        if (bound & m) == 0 {
            // bound is a power of 2
            r = ((i64::from(bound) * i64::from(r)) >> 31) as i32;
        } else {
            let mut u = r;
            loop {
                r = u % bound;
                if u.wrapping_sub(r).wrapping_add(m) >= 0 {
                    break;
                }
                u = self.next(31);
            }
        }
        Ok(r)
    }

    /// `nextInt(origin, bound)`: `RandomSupport.boundedNextInt` over
    /// `nextInt()`.
    pub fn next_int_range(&mut self, origin: i32, bound: i32) -> Result<i32, &'static str> {
        if origin >= bound {
            return Err(BAD_RANGE);
        }
        let mut r = self.next_int();
        let n = bound.wrapping_sub(origin);
        let m = n.wrapping_sub(1);
        if (n & m) == 0 {
            r = (r & m).wrapping_add(origin);
        } else if n > 0 {
            let mut u = ((r as u32) >> 1) as i32;
            loop {
                r = u % n;
                if u.wrapping_add(m).wrapping_sub(r) >= 0 {
                    break;
                }
                u = ((self.next_int() as u32) >> 1) as i32;
            }
            r = r.wrapping_add(origin);
        } else {
            while r < origin || r >= bound {
                r = self.next_int();
            }
        }
        Ok(r)
    }

    /// `nextLong()`.
    pub fn next_long(&mut self) -> i64 {
        (i64::from(self.next(32)) << 32).wrapping_add(i64::from(self.next(32)))
    }

    /// `nextLong(bound)`: `RandomSupport.boundedNextLong(rng, bound)`.
    pub fn next_long_bounded(&mut self, bound: i64) -> Result<i64, &'static str> {
        if bound <= 0 {
            return Err(BAD_BOUND);
        }
        let m = bound - 1;
        let mut r = self.next_long();
        if (bound & m) == 0 {
            r &= m;
        } else {
            let mut u = ((r as u64) >> 1) as i64;
            loop {
                r = u % bound;
                if u.wrapping_add(m).wrapping_sub(r) >= 0 {
                    break;
                }
                u = ((self.next_long() as u64) >> 1) as i64;
            }
        }
        Ok(r)
    }

    /// `nextLong(origin, bound)`: `RandomSupport.boundedNextLong(rng, origin,
    /// bound)`.
    pub fn next_long_range(&mut self, origin: i64, bound: i64) -> Result<i64, &'static str> {
        if origin >= bound {
            return Err(BAD_RANGE);
        }
        let mut r = self.next_long();
        let n = bound.wrapping_sub(origin);
        let m = n.wrapping_sub(1);
        if (n & m) == 0 {
            r = (r & m).wrapping_add(origin);
        } else if n > 0 {
            let mut u = ((r as u64) >> 1) as i64;
            loop {
                r = u % n;
                if u.wrapping_add(m).wrapping_sub(r) >= 0 {
                    break;
                }
                u = ((self.next_long() as u64) >> 1) as i64;
            }
            r = r.wrapping_add(origin);
        } else {
            while r < origin || r >= bound {
                r = self.next_long();
            }
        }
        Ok(r)
    }

    /// `nextBoolean()`.
    pub fn next_boolean(&mut self) -> bool {
        self.next(1) != 0
    }

    /// `nextFloat()`: 24 random bits scaled by `2^-24`.
    pub fn next_float(&mut self) -> f32 {
        self.next(24) as f32 * (1.0 / (1u32 << 24) as f32)
    }

    /// `nextDouble()`: 53 random bits scaled by `2^-53`.
    pub fn next_double(&mut self) -> f64 {
        let bits = (i64::from(self.next(26)) << 27) + i64::from(self.next(27));
        bits as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// `nextDouble(bound)`: `RandomSupport.boundedNextDouble(rng, bound)`.
    pub fn next_double_bounded(&mut self, bound: f64) -> Result<f64, &'static str> {
        if !(0.0 < bound && bound < f64::INFINITY) {
            return Err(BAD_FLOATING_BOUND);
        }
        let mut r = self.next_double() * bound;
        if r >= bound {
            r = next_down(bound);
        }
        Ok(r)
    }

    /// `nextDouble(origin, bound)`: `RandomSupport.boundedNextDouble(rng,
    /// origin, bound)`.
    pub fn next_double_range(&mut self, origin: f64, bound: f64) -> Result<f64, &'static str> {
        if !(f64::NEG_INFINITY < origin && origin < bound && bound < f64::INFINITY) {
            return Err(BAD_RANGE);
        }
        let mut r = self.next_double();
        if bound - origin < f64::INFINITY {
            r = r * (bound - origin) + origin;
        } else {
            let half_origin = 0.5 * origin;
            r = (r * (0.5 * bound - half_origin) + half_origin) * 2.0;
        }
        if r >= bound {
            r = next_down(bound);
        }
        Ok(r)
    }

    /// `nextGaussian()`: Knuth's polar method, two values per round, with
    /// `StrictMath`'s (fdlibm's) `log` and the exact `sqrt`.
    pub fn next_gaussian(&mut self) -> f64 {
        if let Some(g) = self.next_gaussian.take() {
            return g;
        }
        let (v1, v2, s) = loop {
            let v1 = 2.0 * self.next_double() - 1.0;
            let v2 = 2.0 * self.next_double() - 1.0;
            let s = v1 * v1 + v2 * v2;
            if !(s >= 1.0 || s == 0.0) {
                break (v1, v2, s);
            }
        };
        let multiplier = (-2.0 * crate::fdlibm::log(s) / s).sqrt();
        self.next_gaussian = Some(v2 * multiplier);
        v1 * multiplier
    }

    /// `nextBytes(bytes)`: one `nextInt()` per four bytes, low byte first.
    pub fn next_bytes(&mut self, len: usize) -> Vec<i8> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let mut rnd = self.next_int();
            for _ in 0..(len - out.len()).min(4) {
                out.push(rnd as i8);
                rnd >>= 8;
            }
        }
        out
    }
}

/// `Math.nextDown` for the finite positive bounds the bounded draws pass.
fn next_down(d: f64) -> f64 {
    if d.is_nan() || d == f64::NEG_INFINITY {
        d
    } else if d == 0.0 {
        -f64::from_bits(1)
    } else {
        f64::from_bits(if d > 0.0 {
            d.to_bits() - 1
        } else {
            d.to_bits() + 1
        })
    }
}
