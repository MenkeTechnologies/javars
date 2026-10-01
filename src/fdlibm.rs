//! `java.lang.FdLibm`: the JDK's own Java port of fdlibm, which every
//! `StrictMath` function answers from, and which `Math` delegates to wherever
//! HotSpot has no intrinsic for the function.
//!
//! Ported line for line from `java.base/java/lang/FdLibm.java` (openjdk 27),
//! so a result is bit-identical to the JDK's on every platform: the algorithm
//! is a sequence of IEEE `double` operations with no platform `libm` call in
//! it. Rust's `f64::ln` and friends are *not* used — they reach the host's
//! libm, which is free to differ from fdlibm in the last place.
//!
//! Constants are written as their exact bit patterns, the JDK source's hex
//! literal beside each, so no decimal rounding can move one.
//!
//! The word helpers mirror the file's `__HI`/`__LO`: the high and low 32 bits
//! of a `double`'s bit pattern, read as signed `int`s.

/// `0x7ff0_0000`: the exponent bits of a high word.
const EXP_BITS: i32 = 0x7ff0_0000;
/// `0x7fff_ffff`: a high word without its sign bit.
const EXP_SIGNIF_BITS: i32 = 0x7fff_ffff;
/// `2^54`, the scale that brings a subnormal into the normal range.
const TWO54: f64 = f64::from_bits(0x4350_0000_0000_0000);

/// `__HI(x)`: the high 32 bits of `x`'s bit pattern.
fn hi(x: f64) -> i32 {
    (x.to_bits() >> 32) as i32
}

/// `__LO(x)`: the low 32 bits of `x`'s bit pattern.
fn lo(x: f64) -> i32 {
    x.to_bits() as i32
}

/// `__HI(x, high)`: `x` with its high word replaced.
fn with_hi(x: f64, high: i32) -> f64 {
    f64::from_bits((x.to_bits() & 0x0000_0000_FFFF_FFFF) | ((high as u32 as u64) << 32))
}

/// `FdLibm.Log.compute` — `StrictMath.log`, the natural logarithm.
pub fn log(x: f64) -> f64 {
    const LN2_HI: f64 = f64::from_bits(0x3fe6_2e42_fee0_0000); // 0x1.62e42feep-1
    const LN2_LO: f64 = f64::from_bits(0x3dea_39ef_3579_3c76); // 0x1.a39ef35793c76p-33
    const LG1: f64 = f64::from_bits(0x3fe5_5555_5555_5593); // 0x1.5555555555593p-1
    const LG2: f64 = f64::from_bits(0x3fd9_9999_9997_fa04); // 0x1.999999997fa04p-2
    const LG3: f64 = f64::from_bits(0x3fd2_4924_9422_9359); // 0x1.2492494229359p-2
    const LG4: f64 = f64::from_bits(0x3fcc_71c5_1d8e_78af); // 0x1.c71c51d8e78afp-3
    const LG5: f64 = f64::from_bits(0x3fc7_4664_96cb_03de); // 0x1.7466496cb03dep-3
    const LG6: f64 = f64::from_bits(0x3fc3_9a09_d078_c69f); // 0x1.39a09d078c69fp-3
    const LG7: f64 = f64::from_bits(0x3fc2_f112_df3e_5244); // 0x1.2f112df3e5244p-3
    const THIRD: f64 = f64::from_bits(0x3fd5_5555_5555_5555); // 0.33333333333333333

    let mut x = x;
    let mut hx = hi(x);
    let lx = lo(x);
    let mut k: i32 = 0;
    if hx < 0x0010_0000 {
        // x < 2**-1022
        if ((hx & EXP_SIGNIF_BITS) | lx) == 0 {
            return -TWO54 / 0.0; // log(+-0) = -inf
        }
        if hx < 0 {
            #[allow(clippy::eq_op)]
            return (x - x) / 0.0; // log(-#) = NaN
        }
        k -= 54;
        x *= TWO54; // subnormal number, scale up x
        hx = hi(x);
    }
    if hx >= EXP_BITS {
        return x + x;
    }
    k += (hx >> 20) - 1023;
    hx &= 0x000f_ffff;
    let mut i = (hx.wrapping_add(0x9_5f64)) & 0x10_0000;
    x = with_hi(x, hx | (i ^ 0x3ff0_0000)); // normalize x or x/2
    k += i >> 20;
    let f = x - 1.0;
    if (0x000f_ffff & (2 + hx)) < 3 {
        // |f| < 2**-20
        if f == 0.0 {
            if k == 0 {
                return 0.0;
            }
            let dk = k as f64;
            return dk * LN2_HI + dk * LN2_LO;
        }
        let r = f * f * (0.5 - THIRD * f);
        if k == 0 {
            return f - r;
        }
        let dk = k as f64;
        return dk * LN2_HI - ((r - dk * LN2_LO) - f);
    }
    let s = f / (2.0 + f);
    let dk = k as f64;
    let z = s * s;
    i = hx - 0x6_147a;
    let w = z * z;
    let j = 0x6b851 - hx;
    let t1 = w * (LG2 + w * (LG4 + w * LG6));
    let t2 = z * (LG1 + w * (LG3 + w * (LG5 + w * LG7)));
    i |= j;
    let r = t2 + t1;
    if i > 0 {
        let hfsq = 0.5 * f * f;
        if k == 0 {
            f - (hfsq - s * (hfsq + r))
        } else {
            dk * LN2_HI - ((hfsq - (s * (hfsq + r) + dk * LN2_LO)) - f)
        }
    } else if k == 0 {
        f - s * (f - r)
    } else {
        dk * LN2_HI - ((s * (f - r) - dk * LN2_LO) - f)
    }
}
