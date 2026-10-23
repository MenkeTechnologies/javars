//! `Math.sin` and `Math.cos` as HotSpot answers them on aarch64: a port of the
//! `dsin`/`dcos` stubs `MacroAssembler::generate_dsin_dcos` emits
//! (`src/hotspot/cpu/aarch64/macroAssembler_aarch64_trig.cpp`, openjdk 27),
//! which the interpreter, C1 and C2 all call for those two methods.
//!
//! The stub is fdlibm's algorithm (`crate::fdlibm::sin`/`cos`, which is
//! `StrictMath`) re-scheduled for the FPU, and three of those changes move the
//! last bit, so `Math.sin(x)` and `StrictMath.sin(x)` disagree on a few percent
//! of arguments:
//!
//! - Most multiply-adds are fused (`fmadd`/`fmsub`/`fnmsub`/`fmla`): the
//!   kernel polynomials, `t*invpio2+half` and `t-fn*pio2_1` in the medium
//!   reduction, and the inner products of the large one. Each is written here
//!   as [`f64::mul_add`], which is one correctly rounded operation on every
//!   target, so the port gives the stub's answer on x86-64 hosts too.
//! - The large reduction computes `q[0..=4]` in the opposite summation order
//!   from fdlibm (`x[jx]*f[i]` first), while its recompute loop keeps fdlibm's.
//! - `kernel_cos` reads the high word `ix` of the *original* argument, which
//!   the stub keeps in a register, not of the reduced one. After a reduction
//!   that word is at least `0x3fe921fb`, so the `qx` correction is always the
//!   constant 0.28125.
//!
//! The answer is the aarch64 JDK's — the platform this was measured on. An x86-64
//! JDK answers `Math.sin`/`cos` from a different stub (Intel's libm port), so
//! its last digit can differ from both this and `StrictMath`; the specification
//! allows one ulp either way.

use crate::fdlibm::{
    hi, C1, C2, C3, C4, C5, C6, INVPIO2, NPIO2_HW, PIO2, PIO2_1, PIO2_1T, PIO2_2, PIO2_2T, PIO2_3,
    PIO2_3T, S1, S2, S3, S4, S5, S6, TWON24, TWO_OVER_PI,
};

/// `2^24`.
const TWO24: f64 = f64::from_bits(0x4170_0000_0000_0000);

/// `Math.sin` — the aarch64 `dsin` stub.
pub fn sin(x: f64) -> f64 {
    sin_cos(x, false)
}

/// `Math.cos` — the aarch64 `dcos` stub.
pub fn cos(x: f64) -> f64 {
    sin_cos(x, true)
}

/// `generate_dsin_dcos`: the corner cases, then the reduction and the kernel
/// the quadrant selects.
fn sin_cos(x: f64, is_cos: bool) -> f64 {
    let ix = hi(x) & 0x7fff_ffff;
    if ix < 0x3e40_0000 {
        // |x| < 2^-27
        return if is_cos { 1.0 } else { x };
    }
    if ix <= 0x3fe9_21fb {
        // |x| ~<= pi/4
        return if is_cos {
            kernel_cos(x, 0.0, ix)
        } else {
            kernel_sin(x, 0.0, false)
        };
    }
    if ix >= 0x7ff0_0000 {
        // The stub builds its NaN as the infinity bits with the low bit set.
        return f64::from_bits(0x7ff0_0000_0000_0001);
    }
    let (n, y0, y1) = rem_pio2(x, ix);
    // Bit 0 of n picks the kernel; the sign is bit 1 for sin, bit 0 ^ bit 1
    // for cos.
    let use_sin = (n & 1 == 0) != is_cos;
    let negate = if is_cos {
        (n ^ (n >> 1)) & 1 != 0
    } else {
        n & 2 != 0
    };
    let r = if use_sin {
        kernel_sin(y0, y1, true)
    } else {
        kernel_cos(y0, y1, ix)
    };
    if negate {
        -r
    } else {
        r
    }
}

/// `generate_kernel_sin`.
fn kernel_sin(x: f64, y: f64, iy: bool) -> f64 {
    let z = x * x;
    let v = z * x;
    let mut r = z.mul_add(S6, S5);
    r = z.mul_add(r, S4);
    r = z.mul_add(r, S3);
    r = z.mul_add(r, S2);
    if !iy {
        // x + v*(S1 + z*r)
        v.mul_add(z.mul_add(r, S1), x)
    } else {
        // x - ((z*(half*y - v*r) - y) - v*S1)
        let t = (-v).mul_add(r, 0.5 * y);
        let t = (-z).mul_add(t, y);
        let t = v.mul_add(S1, t);
        x + t
    }
}

/// `generate_kernel_cos`. `ix` is the high word the stub's register holds:
/// the argument's own on the early path, the unreduced argument's after a
/// reduction.
fn kernel_cos(x: f64, y: f64, ix: i32) -> f64 {
    let z = x * x;
    let mut r = z.mul_add(C6, C5);
    r = z.mul_add(r, C4);
    let xy = x * y;
    r = z.mul_add(r, C3);
    r = z.mul_add(r, C2);
    let zz = z * z;
    r = z.mul_add(r, C1);
    if ix <= 0x3fd3_3333 {
        // 1 - (0.5*z - (z*r - x*y)), with z*r as zz*r
        let t = (-zz).mul_add(r, xy);
        let t = 0.5f64.mul_add(z, t);
        1.0 - t
    } else {
        let qx = if ix > 0x3fe9_0000 {
            0.28125
        } else {
            f64::from_bits(((ix - 0x0020_0000) as u32 as u64) << 32)
        };
        let zr_xy = zz.mul_add(r, -xy);
        let h = 0.5f64.mul_add(z, -qx);
        let a = 1.0 - qx;
        a - (h - zr_xy)
    }
}

/// The exponent field of `d`, as `(hi(d) >> 20) & 0x7ff`.
fn exponent(d: f64) -> i32 {
    ((d.to_bits() >> 52) & 0x7ff) as i32
}

/// `generate__ieee754_rem_pio2`: `x` (|x| > pi/4, finite) as `n*pi/2 + y0 + y1`.
/// Only `n`'s low two bits are meaningful.
fn rem_pio2(x: f64, ix: i32) -> (i32, f64, f64) {
    let negative = x.is_sign_negative();
    if ix < 0x4002_d97c {
        // |x| < 3pi/4, n = +-1
        let near_pio2 = ix == 0x3ff9_21fb;
        return if !negative {
            let mut z = x - PIO2_1;
            let tail = if near_pio2 {
                z -= PIO2_2;
                PIO2_2T
            } else {
                PIO2_1T
            };
            let y0 = z - tail;
            (1, y0, (z - y0) - tail)
        } else {
            let mut z = x + PIO2_1;
            let tail = if near_pio2 {
                z += PIO2_2;
                PIO2_2T
            } else {
                PIO2_1T
            };
            let y0 = z + tail;
            (-1, y0, (z - y0) + tail)
        };
    }
    let (n, y0, y1) = if ix <= 0x4139_21fb {
        medium(x.abs(), ix)
    } else {
        large(x, ix)
    };
    if negative {
        (n.wrapping_neg(), -y0, -y1)
    } else {
        (n, y0, y1)
    }
}

/// The medium branch (|x| ~<= 2^19*pi/2) of the reduction, on `t = |x|`.
fn medium(t: f64, ix: i32) -> (i32, f64, f64) {
    let v2 = t.mul_add(INVPIO2, 0.5);
    let n = v2 as i32;
    let fnn = v2.trunc();
    let mut r = (-fnn).mul_add(PIO2_1, t);
    let mut w = fnn * PIO2_1T;
    let mut y0 = r - w;
    // The stub takes the quick exit for n <= 32, where fdlibm's test is n < 32.
    let quick = n <= 32 && ix != NPIO2_HW[(n - 1) as usize];
    if !quick {
        let j = ix >> 20;
        if j - exponent(y0) > 16 {
            // 2nd iteration, good to 118 bits
            let t = r;
            let w1 = fnn * PIO2_2;
            r = t - w1;
            w = fnn.mul_add(PIO2_2T, w1 - (t - r));
            y0 = r - w;
            if j - exponent(y0) > 49 {
                // 3rd iteration, 151 bits
                let t = r;
                let w1 = fnn * PIO2_3;
                r = t - w1;
                w = fnn.mul_add(PIO2_3T, w1 - (t - r));
                y0 = r - w;
            }
        }
    }
    (n, y0, (r - y0) - w)
}

/// The large branch: `|x|` split into three 24-bit pieces and reduced by
/// [`kernel_rem_pio2`]. The sign is applied by the caller.
fn large(x: f64, ix: i32) -> (i32, f64, f64) {
    let e0 = (ix >> 20) - 1046;
    let low = x.to_bits() & 0xffff_ffff;
    let mut z = f64::from_bits((((ix - (e0 << 20)) as u32 as u64) << 32) | low);
    let mut tx = [0.0f64; 3];
    tx[0] = z.trunc();
    z = (z - tx[0]) * TWO24;
    tx[1] = z.trunc();
    z -= tx[1];
    tx[2] = z * TWO24;
    let jx = if z != 0.0 {
        2
    } else if tx[1] != 0.0 {
        1
    } else {
        0
    };
    kernel_rem_pio2(&tx, e0, jx)
}

/// `q[i] = sum over j of x[j] * f[jx+i-j]` as the recompute loop sums it:
/// `x[0]` first, each further term fused.
fn recompute_term(tx: &[f64; 3], f: &[f64; 20], jx: usize, i: usize) -> f64 {
    let mut fw = tx[0] * f[jx + i];
    for j in 1..=jx {
        fw = tx[j].mul_add(f[jx + i - j], fw);
    }
    fw
}

/// `generate__kernel_rem_pio2` with `prec == 2` (`jk == jp == 4`): `(n, y0, y1)`
/// for the positive argument whose 24-bit pieces are `tx[0..=jx]`.
fn kernel_rem_pio2(tx: &[f64; 3], e0: i32, jx: usize) -> (i32, f64, f64) {
    const JK: usize = 4;
    let mut f = [0.0f64; 20];
    let mut q = [0.0f64; 20];
    let mut fq = [0.0f64; 20];
    let mut iq = [0i32; 20];

    let jv = ((e0 - 3) / 24).max(0);
    let mut q0 = e0 - 24 * (jv + 1);
    let jv = jv as usize;

    // f[0..=jx+jk] = ipio2[jv-jx..], zero below the table.
    for (i, slot) in f.iter_mut().enumerate().take(jx + JK + 1) {
        let j = jv as isize - jx as isize + i as isize;
        *slot = if j < 0 {
            0.0
        } else {
            TWO_OVER_PI[j as usize] as f64
        };
    }
    // q[0..=jk]: the vectorized form starts from x[jx]*f[i] and fuses the
    // lower-indexed pieces in after it.
    for i in 0..=JK {
        let mut fw = tx[jx] * f[i];
        for j in (0..jx).rev() {
            fw = f[i + jx - j].mul_add(tx[j], fw);
        }
        q[i] = fw;
    }

    let two_q0 = f64::from_bits(((q0 + 1023) as u64) << 52);
    let two_minus_q0 = f64::from_bits(((1023 - q0) as u64) << 52);
    let mut jz = JK;
    let (mut n, mut ih, mut z);
    loop {
        // distill q[] into iq[] reversingly
        z = q[jz];
        for (i, j) in (1..=jz).rev().enumerate() {
            let fw = (TWON24 * z).trunc();
            iq[i] = (-TWO24).mul_add(fw, z) as i32;
            z = q[j - 1] + fw;
        }
        // compute n
        z *= two_q0;
        z = (-(z * 0.125).floor()).mul_add(8.0, z);
        n = z as i32;
        z -= z.trunc();
        ih = 0;
        if q0 > 0 {
            let i = iq[jz - 1] >> (24 - q0);
            n += i;
            iq[jz - 1] -= i << (24 - q0);
            ih = iq[jz - 1] >> (23 - q0);
        } else if q0 == 0 {
            ih = iq[jz - 1] >> 23;
        } else if z >= 0.5 {
            ih = 2;
        }
        let mut carry = false;
        if ih > 0 {
            // q > 0.5: compute 1 - q
            n += 1;
            for v in iq.iter_mut().take(jz) {
                if carry {
                    *v = 0xff_ffff - *v;
                } else if *v != 0 {
                    carry = true;
                    *v = 0x100_0000 - *v;
                }
            }
            if q0 > 0 {
                iq[jz - 1] &= 0xff_ffff >> q0;
            }
            if ih == 2 {
                z = 1.0 - z;
                if carry {
                    z -= two_q0;
                }
            }
        }
        // check if recomputation is needed
        if z == 0.0 && iq[JK..jz].iter().all(|&v| v == 0) {
            // k = number of terms needed, from the first nonzero of iq[3..=0]
            let k = (1..=JK).find(|&k| iq[JK - k] != 0).unwrap_or(JK);
            for i in jz + 1..=jz + k {
                f[jx + i] = TWO_OVER_PI[jv + i] as f64;
                q[i] = recompute_term(tx, &f, jx, i);
            }
            jz += k;
            continue;
        }
        break;
    }

    // chop off zero terms
    if z == 0.0 {
        loop {
            jz -= 1;
            q0 -= 24;
            if iq[jz] != 0 {
                break;
            }
        }
    } else {
        // break z into 24-bit if necessary
        z *= two_minus_q0;
        if z >= TWO24 {
            let fw = (TWON24 * z).trunc();
            iq[jz] = (-TWO24).mul_add(fw, z) as i32;
            jz += 1;
            q0 += 24;
            iq[jz] = fw as i32;
        } else {
            iq[jz] = z as i32;
        }
    }

    // convert integer "bit" chunk to floating-point value
    let mut fw = f64::from_bits(((q0 + 1023) as u64) << 52);
    for i in (0..=jz).rev() {
        q[i] = iq[i] as f64 * fw;
        fw *= TWON24;
    }
    // fq[jz-i] = PIo2[0..=min(jp, jz-i)] . q[i..]
    for i in (0..=jz).rev() {
        let mut fw = 0.0f64;
        for k in 0..=JK.min(jz - i) {
            fw = q[i + k].mul_add(PIO2[k], fw);
        }
        fq[jz - i] = fw;
    }
    // compress fq[] into y[]
    let mut y0 = 0.0f64;
    for i in (0..=jz).rev() {
        y0 += fq[i];
    }
    let mut y1 = fq[0] - y0;
    for v in fq.iter().take(jz + 1).skip(1) {
        y1 += *v;
    }
    if ih != 0 {
        (n, -y0, -y1)
    } else {
        (n, y0, y1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(x: u64) -> f64 {
        f64::from_bits(x)
    }

    /// Arguments where the stub and fdlibm disagree, one per path, each pair
    /// read from `Math.sin`/`Math.cos` on openjdk 25 and 27 (aarch64).
    #[test]
    fn matches_the_aarch64_stub_where_fdlibm_does_not() {
        #[rustfmt::skip]
        let cases: &[(u64, u64, u64)] = &[
            (0xbfeaaeee922fde48, 0xbfe7b2655e12b61a, 0x3fe5813e49ffac78), // -8.338539939131318e-1 -0.7405268514027654 0.6720267720496755
            (0x3ff044ea5b939cf0, 0x3feb36d24d1fad4f, 0x3fe0d58e058fcfcd), // 1.0168250634395086e0 0.8504420763323796 0.5260686977985586
            (0xbffa6b14d1a75e5c, 0xbfefe5927b9d1f90, 0xbfb48bedf632e914), // -1.651142901382534e0 -0.9967739500361734 -0.0802601553031404
            (0x3ff916a1f395f67c, 0x3feffff7f3282102, 0x3f66b2bf7529061d), // 1.5680255427427303e0 0.9999961613803239 0.002770780506836669
            (0xbfe9ac96bf1b7398, 0xbfe701cb826f5085, 0x3fe63dc8c6136fa2), // -8.023179752886902e-1 -0.7189691112094044 0.6950420254392962
            (0x3ff0c8bbaf48a814, 0x3febbde165f3b620, 0x3fdfe64f8ebaa7c1), // 1.0490071150802622e0 0.8669287673234898 0.4984320539321026
            (0xbfe47f78b01f7d48, 0xbfe31fe8350d6f3f, 0x3fe9a80682a8558c), // -6.40560478201146e-1 -0.5976449047261453 0.8017609168916109
            (0x3fe4322b17eaf6a0, 0x3fe2e1b61de0911f, 0x3fe9d5ee9fccccf7), // 6.311240641732248e-1 0.5900526603259414 0.8073647614568519
            (0xbfe73ab6d1ed1d38, 0xbfe53e071026dd2f, 0x3fe7eeb8c76c24f8), // -7.259172535058616e-1 -0.6638217273202026 0.7478908438653482
            (0x3fdb022848f0fe20, 0x3fda36c0f5a9490b, 0x3fed314e76bc29db), // 4.220066749876157e-1 0.4095919035945747 0.9122688597720371
            (0xbfe8f665ffa057a7, 0xbfe681b806d792db, 0x3fe6bf5accf2d4c6), // -7.80077933569136e-1 -0.703334821093985 0.7108587267781779
            (0x3fe627db07fed01a, 0x3fe46d70fbf81e45, 0x3fe8a1b247f60509), // 6.923651844176704e-1 0.638359539153449 0.7697383313644944
            (0xffefffffffffffff, 0xbf7452fc98b34e97, 0xbfefffe62ecfab76), // -1.7976931348623157e308 -0.004961954789184062 -0.99998768942656
            (0x7fefffffffffffff, 0x3f7452fc98b34e97, 0xbfefffe62ecfab76), // 1.7976931348623157e308 0.004961954789184062 -0.99998768942656
            (0xd2cd6a5e1ef6d3aa, 0xbfef1ede72f98434, 0x3fcdcd45fbceae70), // -7.49005363202941e90 -0.972518181381139 0.2328269462135384
            (0x520cbe6de828e607, 0x3fefa5c9079a1f32, 0x3fc2f1a46dc6418e), // 1.7868739678557093e87 0.988987459991199 0.14799933776931734
            (0xc006cb34bb19adda, 0xbfd272408f10b13d, 0xbfeea45b919e8d36), // -2.8492216698848383e0 -0.28822339984141704 -0.9575631946685579
            (0x4007857aad7394fe, 0x3fc99b814d0b9d27, 0xbfef5a64089801ce), // 2.9401753950975253e0 0.2000581384847717 -0.9797840278479784
            (0xc00f4a693dff5278, 0x3fe6453741443262, 0xbfe6fa9a047e6f3d), // -3.911333546022714e0 0.695949199174333 -0.7180910194178761
            (0x412921fe00000000, 0x3fef1e1d322aef28, 0x3fcdd9e00a413200), // 8.23551e5 0.9724260310972328 0.23321152210918683
            (0xc00f806438790ed0, 0x3fe6de44d8e5c569, 0xbfe6624e2a7c9908), // -3.937691155619042e0 0.7146324383049151 -0.6995001630610043
            (0x4003dd5a8cfae0ac, 0x3fe39500eabe90c8, 0xbfe94f1810851784), // 2.4830828680097365e0 0.6119389138080118 -0.79090502954999
            (0xbfc7e93b7799a1a0, 0xbfc7c5b16e782a2f, 0x3fef717afd96b932), // -1.8680518474435548e-1 -0.18572061437619555 0.9826025917916807
            (0x3fceb6cf0849e280, 0x3fce6b925a582111, 0x3fef154a5d92cf0e), // 2.3995387940137292e-1 0.2376578274854713 0.9713489368063806
        ];
        for &(x, s, c) in cases {
            assert_eq!(sin(bits(x)).to_bits(), s, "sin({:e})", bits(x));
            assert_eq!(cos(bits(x)).to_bits(), c, "cos({:e})", bits(x));
        }
    }

    /// NaN and the infinities answer the stub's own NaN, whose bits
    /// `Double.doubleToRawLongBits` exposes; a tiny argument is its own sine.
    #[test]
    fn corner_cases_take_the_stubs_early_exits() {
        for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(sin(x).to_bits(), 0x7ff0_0000_0000_0001);
            assert_eq!(cos(x).to_bits(), 0x7ff0_0000_0000_0001);
        }
        assert_eq!(sin(-0.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(sin(1e-10), 1e-10);
        assert_eq!(cos(-1e-10), 1.0);
    }
}
