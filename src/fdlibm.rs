//! `java.lang.FdLibm`: the JDK's own Java port of fdlibm, which every
//! `StrictMath` function answers from, and which `Math` delegates to wherever
//! HotSpot has no intrinsic for the function.
//!
//! Ported line for line from `java.base/java/lang/FdLibm.java` (openjdk 27),
//! so a result is bit-identical to the JDK's on every platform: the algorithm
//! is a sequence of IEEE `double` operations with no platform `libm` call in
//! it. Rust's `f64::ln` and friends are *not* used — they reach the host's
//! libm, which is free to differ from fdlibm in the last place. `f64::sqrt` is
//! used where the JDK calls `Math.sqrt`: both are the correctly rounded IEEE
//! square root.
//!
//! Constants are written as their exact bit patterns, the JDK source's hex
//! literal beside each, so no decimal rounding can move one.
//!
//! The word helpers mirror the file's `__HI`/`__LO`: the high and low 32 bits
//! of a `double`'s bit pattern, read as signed `int`s. Integer arithmetic on
//! them wraps, as Java's `int` does.
//!
//! `sin` and `cos` are not here: on the JDK's own platforms `Math.sin` and
//! `Math.cos` are answered by an intrinsic that is not fdlibm (measured on
//! openjdk 27, aarch64: 7,725 of 200,000 `Math.sin` results differ from
//! `StrictMath.sin`), so an fdlibm port would answer the last digit
//! differently from the program's own `Math` calls.

/// `0x7ff0_0000`: the exponent bits of a high word.
const EXP_BITS: i32 = 0x7ff0_0000;
/// `0x7fff_ffff`: a high word without its sign bit.
const EXP_SIGNIF_BITS: i32 = 0x7fff_ffff;
/// `0x8000_0000`: the sign bit of a high word.
const SIGN_BIT: i32 = 0x8000_0000_u32 as i32;
/// `2^24`.
const TWO24: f64 = f64::from_bits(0x4170_0000_0000_0000);
/// `2^54`, the scale that brings a subnormal into the normal range.
const TWO54: f64 = f64::from_bits(0x4350_0000_0000_0000);
const HUGE: f64 = 1.0e300;
const PI: f64 = std::f64::consts::PI;

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

/// `__LO(x, low)`: `x` with its low word replaced.
fn with_lo(x: f64, low: i32) -> f64 {
    f64::from_bits((x.to_bits() & 0xFFFF_FFFF_0000_0000) | (low as u32 as u64))
}

/// `__HI_LO(high, low)`: the `double` with these two words.
fn hi_lo(high: i32, low: i32) -> f64 {
    f64::from_bits(((high as u32 as u64) << 32) | (low as u32 as u64))
}

/// `Math.powerOfTwoD(n)`: `2^n` for an exponent in the normal range.
fn power_of_two(n: i32) -> f64 {
    f64::from_bits(((n + 1023) as u64) << 52)
}

/// `Math.scalb(d, scaleFactor)`: `d * 2^scaleFactor`, rounded once.
pub fn scalb(d: f64, scale_factor: i32) -> f64 {
    const EXP_BIAS: i32 = 1023;
    const PRECISION: i32 = 53;
    const F_UP: f64 = f64::from_bits(0x7fe0_0000_0000_0000);
    const F_DOWN: f64 = f64::from_bits(0x0008_0000_0000_0000);
    if scale_factor > -EXP_BIAS {
        if scale_factor <= EXP_BIAS {
            return d * power_of_two(scale_factor);
        }
        if scale_factor <= 2 * EXP_BIAS {
            return d * power_of_two(scale_factor - EXP_BIAS) * F_UP;
        }
        if scale_factor < 2 * EXP_BIAS + PRECISION - 1 {
            return d * power_of_two(scale_factor - 2 * EXP_BIAS) * F_UP * F_UP;
        }
        return d * F_UP * F_UP * F_UP;
    }
    if scale_factor > -2 * EXP_BIAS {
        return d * power_of_two(scale_factor + EXP_BIAS) * F_DOWN;
    }
    if scale_factor > -2 * EXP_BIAS - PRECISION {
        return d * power_of_two(scale_factor + 2 * EXP_BIAS) * F_DOWN * F_DOWN;
    }
    d * f64::from_bits(1) * f64::from_bits(1)
}

/// The NaN fdlibm makes as `(x - x) / (x - x)`.
#[allow(clippy::eq_op)]
fn nan_of(x: f64) -> f64 {
    (x - x) / (x - x)
}

/// `FdLibm.Tan.compute` — `StrictMath.tan`.
pub fn tan(x: f64) -> f64 {
    let ix = hi(x) & EXP_SIGNIF_BITS;
    if ix <= 0x3fe9_21fb {
        kernel_tan(x, 0.0, 1)
    } else if ix >= EXP_BITS {
        #[allow(clippy::eq_op)]
        let nan = x - x;
        nan
    } else {
        let mut y = [0.0; 2];
        let n = rem_pio2(x, &mut y);
        kernel_tan(y[0], y[1], 1 - ((n & 1) << 1))
    }
}

/// `FdLibm.Tan.__kernel_tan`.
fn kernel_tan(x: f64, y: f64, iy: i32) -> f64 {
    const PIO4: f64 = f64::from_bits(0x3fe9_21fb_5444_2d18);
    const PIO4LO: f64 = f64::from_bits(0x3c81_a626_3314_5c07);
    const T: [f64; 13] = [
        f64::from_bits(0x3fd5_5555_5555_5563),
        f64::from_bits(0x3fc1_1111_1110_fe7a),
        f64::from_bits(0x3fab_a1ba_1bb3_41fe),
        f64::from_bits(0x3f96_64f4_8406_d637),
        f64::from_bits(0x3f82_26e3_e96e_8493),
        f64::from_bits(0x3f6d_6d22_c956_0328),
        f64::from_bits(0x3f57_dbc8_fee0_8315),
        f64::from_bits(0x3f43_44d8_f2f2_6501),
        f64::from_bits(0x3f30_26f7_1a8d_1068),
        f64::from_bits(0x3f14_7e88_a037_92a6),
        f64::from_bits(0x3f12_b80f_32f0_a7e9),
        -f64::from_bits(0x3ef3_75cb_db60_5373),
        f64::from_bits(0x3efb_2a70_74bf_7ad4),
    ];
    let (mut x, mut y) = (x, y);
    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS;
    if ix < 0x3e30_0000 && x as i32 == 0 {
        // x < 2**-28
        if ((ix | lo(x)) | (iy + 1)) == 0 {
            return 1.0 / x.abs();
        } else if iy == 1 {
            return x;
        } else {
            // compute -1 / (x+y) carefully
            let w = x + y;
            let z = with_lo(w, 0);
            let v = y - (z - x);
            let a = -1.0 / w;
            let t = with_lo(a, 0);
            let s = 1.0 + t * z;
            return t + a * (s + t * v);
        }
    }
    if ix >= 0x3FE5_9428 {
        // |x| >= 0.6744
        if hx < 0 {
            x = -x;
            y = -y;
        }
        let z = PIO4 - x;
        let w = PIO4LO - y;
        x = z + w;
        y = 0.0;
    }
    let z = x * x;
    let w = z * z;
    let mut r = T[1] + w * (T[3] + w * (T[5] + w * (T[7] + w * (T[9] + w * T[11]))));
    let mut v = z * (T[2] + w * (T[4] + w * (T[6] + w * (T[8] + w * (T[10] + w * T[12])))));
    let s = z * x;
    r = y + z * (s * (r + v) + y);
    r += T[0] * s;
    let w = x + r;
    if ix >= 0x3FE5_9428 {
        v = iy as f64;
        return (1 - ((hx >> 30) & 2)) as f64 * (v - 2.0 * (x - (w * w / (w + v) - r)));
    }
    if iy == 1 {
        w
    } else {
        // compute -1.0/(x + r) accurately
        let z = with_lo(w, 0);
        let v = r - (z - x);
        let a = -1.0 / w;
        let t = with_lo(a, 0);
        let s = 1.0 + t * z;
        t + a * (s + t * v)
    }
}

/// `FdLibm.RemPio2.__ieee754_rem_pio2`: `x` reduced by the nearest multiple
/// `n` of pi/2, as the pair `y[0] + y[1]`; answers `n`.
fn rem_pio2(x: f64, y: &mut [f64; 2]) -> i32 {
    const TWO_OVER_PI: [i32; 66] = [
        0xA2F983, 0x6E4E44, 0x1529FC, 0x2757D1, 0xF534DD, 0xC0DB62, 0x95993C, 0x439041, 0xFE5163,
        0xABDEBB, 0xC561B7, 0x246E3A, 0x424DD2, 0xE00649, 0x2EEA09, 0xD1921C, 0xFE1DEB, 0x1CB129,
        0xA73EE8, 0x8235F5, 0x2EBB44, 0x84E99C, 0x7026B4, 0x5F7E41, 0x3991D6, 0x398353, 0x39F49C,
        0x845F8B, 0xBDF928, 0x3B1FF8, 0x97FFDE, 0x05980F, 0xEF2F11, 0x8B5A0A, 0x6D1F6D, 0x367ECF,
        0x27CB09, 0xB74F46, 0x3F669E, 0x5FEA2D, 0x7527BA, 0xC7EBE5, 0xF17B3D, 0x0739F7, 0x8A5292,
        0xEA6BFB, 0x5FB11F, 0x8D5D08, 0x560330, 0x46FC7B, 0x6BABF0, 0xCFBC20, 0x9AF436, 0x1DA9E3,
        0x91615E, 0xE61B08, 0x659985, 0x5F14A0, 0x68408D, 0xFFD880, 0x4D7327, 0x310606, 0x1556CA,
        0x73A8C9, 0x60E27B, 0xC08C6B,
    ];
    const NPIO2_HW: [i32; 32] = [
        0x3FF921FB, 0x400921FB, 0x4012D97C, 0x401921FB, 0x401F6A7A, 0x4022D97C, 0x4025FDBB,
        0x402921FB, 0x402C463A, 0x402F6A7A, 0x4031475C, 0x4032D97C, 0x40346B9C, 0x4035FDBB,
        0x40378FDB, 0x403921FB, 0x403AB41B, 0x403C463A, 0x403DD85A, 0x403F6A7A, 0x40407E4C,
        0x4041475C, 0x4042106C, 0x4042D97C, 0x4043A28C, 0x40446B9C, 0x404534AC, 0x4045FDBB,
        0x4046C6CB, 0x40478FDB, 0x404858EB, 0x404921FB,
    ];
    const INVPIO2: f64 = f64::from_bits(0x3fe4_5f30_6dc9_c883);
    const PIO2_1: f64 = f64::from_bits(0x3ff9_21fb_5440_0000);
    const PIO2_1T: f64 = f64::from_bits(0x3dd0_b461_1a62_6331);
    const PIO2_2: f64 = f64::from_bits(0x3dd0_b461_1a60_0000);
    const PIO2_2T: f64 = f64::from_bits(0x3ba3_198a_2e03_7073);
    const PIO2_3: f64 = f64::from_bits(0x3ba3_198a_2e00_0000);
    const PIO2_3T: f64 = f64::from_bits(0x397b_839a_2520_49c1);

    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS;
    if ix <= 0x3fe9_21fb {
        // |x| ~<= pi/4, no need for reduction
        y[0] = x;
        y[1] = 0.0;
        return 0;
    }
    if ix < 0x4002_d97c {
        // |x| < 3pi/4, special case with n=+-1
        if hx > 0 {
            let mut z = x - PIO2_1;
            if ix != 0x3ff9_21fb {
                y[0] = z - PIO2_1T;
                y[1] = (z - y[0]) - PIO2_1T;
            } else {
                z -= PIO2_2;
                y[0] = z - PIO2_2T;
                y[1] = (z - y[0]) - PIO2_2T;
            }
            return 1;
        } else {
            let mut z = x + PIO2_1;
            if ix != 0x3ff9_21fb {
                y[0] = z + PIO2_1T;
                y[1] = (z - y[0]) + PIO2_1T;
            } else {
                z += PIO2_2;
                y[0] = z + PIO2_2T;
                y[1] = (z - y[0]) + PIO2_2T;
            }
            return -1;
        }
    }
    if ix <= 0x4139_21fb {
        // |x| ~<= 2^19*(pi/2), medium size
        let mut t = x.abs();
        let n = (t * INVPIO2 + 0.5) as i32;
        let fnn = n as f64;
        let mut r = t - fnn * PIO2_1;
        let mut w = fnn * PIO2_1T; // 1st round good to 85 bit
        if n < 32 && ix != NPIO2_HW[(n - 1) as usize] {
            y[0] = r - w; // quick check no cancellation
        } else {
            let j = ix >> 20;
            y[0] = r - w;
            let mut i = j - ((hi(y[0]) >> 20) & 0x7ff);
            if i > 16 {
                // 2nd iteration needed, good to 118
                t = r;
                w = fnn * PIO2_2;
                r = t - w;
                w = fnn * PIO2_2T - ((t - r) - w);
                y[0] = r - w;
                i = j - ((hi(y[0]) >> 20) & 0x7ff);
                if i > 49 {
                    // 3rd iteration need, 151 bits acc
                    t = r;
                    w = fnn * PIO2_3;
                    r = t - w;
                    w = fnn * PIO2_3T - ((t - r) - w);
                    y[0] = r - w;
                }
            }
        }
        y[1] = (r - y[0]) - w;
        if hx < 0 {
            y[0] = -y[0];
            y[1] = -y[1];
            return -n;
        }
        return n;
    }
    // all other (large) arguments
    if ix >= EXP_BITS {
        // x is inf or NaN
        #[allow(clippy::eq_op)]
        let nan = x - x;
        y[0] = nan;
        y[1] = nan;
        return 0;
    }
    // set z = scalbn(|x|, ilogb(x)-23)
    let mut z = with_lo(0.0, lo(x));
    let e0 = (ix >> 20) - 1046; // e0 = ilogb(z) - 23;
    z = with_hi(z, ix - (e0 << 20));
    let mut tx = [0.0; 3];
    for t in tx.iter_mut().take(2) {
        *t = (z as i32) as f64;
        z = (z - *t) * TWO24;
    }
    tx[2] = z;
    let mut nx = 3;
    while tx[nx - 1] == 0.0 {
        // skip zero term
        nx -= 1;
    }
    let mut yy = [0.0; 3];
    let n = kernel_rem_pio2(&tx, &mut yy, e0, nx, 2, &TWO_OVER_PI);
    y[0] = yy[0];
    y[1] = yy[1];
    if hx < 0 {
        y[0] = -y[0];
        y[1] = -y[1];
        return -n;
    }
    n
}

/// `FdLibm.KernelRemPio2.__kernel_rem_pio2`: the multi-precision reduction
/// for an argument too large for the three-step one.
fn kernel_rem_pio2(
    x: &[f64],
    y: &mut [f64; 3],
    e0: i32,
    nx: usize,
    prec: usize,
    ipio2: &[i32],
) -> i32 {
    const INIT_JK: [i32; 4] = [2, 3, 4, 6];
    const PIO2: [f64; 8] = [
        f64::from_bits(0x3ff9_21fb_4000_0000),
        f64::from_bits(0x3e74_442d_0000_0000),
        f64::from_bits(0x3cf8_4698_8000_0000),
        f64::from_bits(0x3b78_cc51_6000_0000),
        f64::from_bits(0x39f0_1b83_8000_0000),
        f64::from_bits(0x387a_2520_4000_0000),
        f64::from_bits(0x36e3_8222_8000_0000),
        f64::from_bits(0x3569_f31d_0000_0000),
    ];
    const TWON24: f64 = f64::from_bits(0x3e70_0000_0000_0000);

    let mut iq = [0i32; 20];
    let mut f = [0.0f64; 20];
    let mut fq = [0.0f64; 20];
    let mut q = [0.0f64; 20];

    // initialize jk
    let jk = INIT_JK[prec];
    let jp = jk;

    // determine jx, jv, q0, note that 3 > q0
    let jx = nx as i32 - 1;
    let mut jv = (e0 - 3) / 24;
    if jv < 0 {
        jv = 0;
    }
    let mut q0 = e0 - 24 * (jv + 1);

    // set up f[0] to f[jx+jk] where f[jx+jk] = ipio2[jv+jk]
    let m = jx + jk;
    for (j, fi) in (jv - jx..).zip(f.iter_mut().take((m + 1) as usize)) {
        *fi = if j < 0 { 0.0 } else { ipio2[j as usize] as f64 };
    }

    // compute q[0],q[1],...q[jk]
    for i in 0..=jk {
        let mut fw = 0.0;
        for j in 0..=jx {
            fw += x[j as usize] * f[(jx + i - j) as usize];
        }
        q[i as usize] = fw;
    }

    let mut jz = jk;
    let mut z;
    let mut n;
    let mut ih;
    loop {
        // distill q[] into iq[] reversingly
        let mut i = 0;
        let mut j = jz;
        z = q[jz as usize];
        while j > 0 {
            let fw = ((TWON24 * z) as i32) as f64;
            iq[i] = (z - TWO24 * fw) as i32;
            z = q[(j - 1) as usize] + fw;
            i += 1;
            j -= 1;
        }

        // compute n
        z = scalb(z, q0); // actual value of z
        z -= 8.0 * (z * 0.125).floor(); // trim off integer >= 8
        n = z as i32;
        z -= n as f64;
        ih = 0;
        if q0 > 0 {
            // need iq[jz - 1] to determine n
            let i = iq[(jz - 1) as usize] >> (24 - q0);
            n += i;
            iq[(jz - 1) as usize] -= i << (24 - q0);
            ih = iq[(jz - 1) as usize] >> (23 - q0);
        } else if q0 == 0 {
            ih = iq[(jz - 1) as usize] >> 23;
        } else if z >= 0.5 {
            ih = 2;
        }

        if ih > 0 {
            // q > 0.5
            n += 1;
            let mut carry = 0;
            for v in iq.iter_mut().take(jz as usize) {
                // compute 1-q
                let j = *v;
                if carry == 0 {
                    if j != 0 {
                        carry = 1;
                        *v = 0x100_0000 - j;
                    }
                } else {
                    *v = 0xff_ffff - j;
                }
            }
            if q0 > 0 {
                // rare case: chance is 1 in 12
                match q0 {
                    1 => iq[(jz - 1) as usize] &= 0x7f_ffff,
                    2 => iq[(jz - 1) as usize] &= 0x3f_ffff,
                    _ => {}
                }
            }
            if ih == 2 {
                z = 1.0 - z;
                if carry != 0 {
                    z -= scalb(1.0, q0);
                }
            }
        }

        // check if recomputation is needed
        if z == 0.0 {
            let mut j = 0;
            let mut i = jz - 1;
            while i >= jk {
                j |= iq[i as usize];
                i -= 1;
            }
            if j == 0 {
                // need recomputation
                let mut k = 1;
                while iq[(jk - k) as usize] == 0 {
                    k += 1; // k = no. of terms needed
                }
                for i in (jz + 1)..=(jz + k) {
                    // add q[jz+1] to q[jz+k]
                    f[(jx + i) as usize] = ipio2[(jv + i) as usize] as f64;
                    let mut fw = 0.0;
                    for j in 0..=jx {
                        fw += x[j as usize] * f[(jx + i - j) as usize];
                    }
                    q[i as usize] = fw;
                }
                jz += k;
                continue;
            }
        }
        break;
    }

    // chop off zero terms
    if z == 0.0 {
        jz -= 1;
        q0 -= 24;
        while iq[jz as usize] == 0 {
            jz -= 1;
            q0 -= 24;
        }
    } else {
        // break z into 24-bit if necessary
        z = scalb(z, -q0);
        if z >= TWO24 {
            let fw = ((TWON24 * z) as i32) as f64;
            iq[jz as usize] = (z - TWO24 * fw) as i32;
            jz += 1;
            q0 += 24;
            iq[jz as usize] = fw as i32;
        } else {
            iq[jz as usize] = z as i32;
        }
    }

    // convert integer "bit" chunk to floating-point value
    let mut fw = scalb(1.0, q0);
    let mut i = jz;
    while i >= 0 {
        q[i as usize] = fw * iq[i as usize] as f64;
        fw *= TWON24;
        i -= 1;
    }

    // compute PIo2[0,...,jp]*q[jz,...,0]
    let mut i = jz;
    while i >= 0 {
        let mut fw = 0.0;
        let mut k = 0;
        while k <= jp && k <= jz - i {
            fw += PIO2[k as usize] * q[(i + k) as usize];
            k += 1;
        }
        fq[(jz - i) as usize] = fw;
        i -= 1;
    }

    // compress fq[] into y[]
    match prec {
        0 => {
            let mut fw = 0.0;
            let mut i = jz;
            while i >= 0 {
                fw += fq[i as usize];
                i -= 1;
            }
            y[0] = if ih == 0 { fw } else { -fw };
        }
        1 | 2 => {
            let mut fw = 0.0;
            let mut i = jz;
            while i >= 0 {
                fw += fq[i as usize];
                i -= 1;
            }
            y[0] = if ih == 0 { fw } else { -fw };
            fw = fq[0] - fw;
            for v in fq.iter().take((jz + 1) as usize).skip(1) {
                fw += *v;
            }
            y[1] = if ih == 0 { fw } else { -fw };
        }
        _ => {
            // painful
            let mut i = jz;
            while i > 0 {
                let fw = fq[(i - 1) as usize] + fq[i as usize];
                fq[i as usize] += fq[(i - 1) as usize] - fw;
                fq[(i - 1) as usize] = fw;
                i -= 1;
            }
            let mut i = jz;
            while i > 1 {
                let fw = fq[(i - 1) as usize] + fq[i as usize];
                fq[i as usize] += fq[(i - 1) as usize] - fw;
                fq[(i - 1) as usize] = fw;
                i -= 1;
            }
            let mut fw = 0.0;
            let mut i = jz;
            while i >= 2 {
                fw += fq[i as usize];
                i -= 1;
            }
            if ih == 0 {
                y[0] = fq[0];
                y[1] = fq[1];
                y[2] = fw;
            } else {
                y[0] = -fq[0];
                y[1] = -fq[1];
                y[2] = -fw;
            }
        }
    }
    n & 7
}

/// The `pS`/`qS` rational approximation `asin` and `acos` share.
const PS0: f64 = f64::from_bits(0x3fc5_5555_5555_5555);
const PS1: f64 = -f64::from_bits(0x3fd4_d612_03eb_6f7d);
const PS2: f64 = f64::from_bits(0x3fc9_c155_0e88_4455);
const PS3: f64 = -f64::from_bits(0x3fa4_8228_b568_8f3b);
const PS4: f64 = f64::from_bits(0x3f49_efe0_7501_b288);
const PS5: f64 = f64::from_bits(0x3f02_3de1_0dfd_f709);
const QS1: f64 = -f64::from_bits(0x4003_3a27_1c8a_2d4b);
const QS2: f64 = f64::from_bits(0x4000_2ae5_9c59_8ac8);
const QS3: f64 = -f64::from_bits(0x3fe6_066c_1b8d_0159);
const QS4: f64 = f64::from_bits(0x3fb3_b8c5_b12e_9282);
const PIO2_HI: f64 = f64::from_bits(0x3ff9_21fb_5444_2d18);
const PIO2_LO: f64 = f64::from_bits(0x3c91_a626_3314_5c07);

/// `FdLibm.Asin.compute` — `StrictMath.asin`.
pub fn asin(x: f64) -> f64 {
    const PIO4_HI: f64 = f64::from_bits(0x3fe9_21fb_5444_2d18);
    let mut t = 0.0;
    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS;
    if ix >= 0x3ff0_0000 {
        // |x| >= 1
        if ((ix - 0x3ff0_0000) | lo(x)) == 0 {
            // asin(1) = +-pi/2 with inexact
            return x * PIO2_HI + x * PIO2_LO;
        }
        return nan_of(x); // asin(|x| > 1) is NaN
    } else if ix < 0x3fe0_0000 {
        // |x| < 0.5
        if ix < 0x3e40_0000 {
            // if |x| < 2**-27
            if HUGE + x > 1.0 {
                return x;
            }
        } else {
            t = x * x;
        }
        let p = t * (PS0 + t * (PS1 + t * (PS2 + t * (PS3 + t * (PS4 + t * PS5)))));
        let q = 1.0 + t * (QS1 + t * (QS2 + t * (QS3 + t * QS4)));
        let w = p / q;
        return x + x * w;
    }
    // 1 > |x| >= 0.5
    let mut w = 1.0 - x.abs();
    t = w * 0.5;
    let mut p = t * (PS0 + t * (PS1 + t * (PS2 + t * (PS3 + t * (PS4 + t * PS5)))));
    let mut q = 1.0 + t * (QS1 + t * (QS2 + t * (QS3 + t * QS4)));
    let s = t.sqrt();
    if ix >= 0x3FEF_3333 {
        // if |x| > 0.975
        w = p / q;
        t = PIO2_HI - (2.0 * (s + s * w) - PIO2_LO);
    } else {
        w = with_lo(s, 0);
        let c = (t - w * w) / (s + w);
        let r = p / q;
        p = 2.0 * s * r - (PIO2_LO - 2.0 * c);
        q = PIO4_HI - 2.0 * w;
        t = PIO4_HI - (p - q);
    }
    if hx > 0 {
        t
    } else {
        -t
    }
}

/// `FdLibm.Acos.compute` — `StrictMath.acos`.
pub fn acos(x: f64) -> f64 {
    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS;
    if ix >= 0x3ff0_0000 {
        // |x| >= 1
        if ((ix - 0x3ff0_0000) | lo(x)) == 0 {
            // |x| == 1
            return if hx > 0 { 0.0 } else { PI + 2.0 * PIO2_LO };
        }
        return nan_of(x); // acos(|x| > 1) is NaN
    }
    if ix < 0x3fe0_0000 {
        // |x| < 0.5
        if ix <= 0x3c60_0000 {
            return PIO2_HI + PIO2_LO;
        }
        let z = x * x;
        let p = z * (PS0 + z * (PS1 + z * (PS2 + z * (PS3 + z * (PS4 + z * PS5)))));
        let q = 1.0 + z * (QS1 + z * (QS2 + z * (QS3 + z * QS4)));
        let r = p / q;
        PIO2_HI - (x - (PIO2_LO - x * r))
    } else if hx < 0 {
        // x < -0.5
        let z = (1.0 + x) * 0.5;
        let p = z * (PS0 + z * (PS1 + z * (PS2 + z * (PS3 + z * (PS4 + z * PS5)))));
        let q = 1.0 + z * (QS1 + z * (QS2 + z * (QS3 + z * QS4)));
        let s = z.sqrt();
        let r = p / q;
        let w = r * s - PIO2_LO;
        PI - 2.0 * (s + w)
    } else {
        // x > 0.5
        let z = (1.0 - x) * 0.5;
        let s = z.sqrt();
        let df = with_lo(s, 0);
        let c = (z - df * df) / (s + df);
        let p = z * (PS0 + z * (PS1 + z * (PS2 + z * (PS3 + z * (PS4 + z * PS5)))));
        let q = 1.0 + z * (QS1 + z * (QS2 + z * (QS3 + z * QS4)));
        let r = p / q;
        let w = r * s + c;
        2.0 * (df + w)
    }
}

/// `FdLibm.Atan.compute` — `StrictMath.atan`.
pub fn atan(x: f64) -> f64 {
    const ATANHI: [f64; 4] = [
        f64::from_bits(0x3fdd_ac67_0561_bb4f),
        f64::from_bits(0x3fe9_21fb_5444_2d18),
        f64::from_bits(0x3fef_730b_d281_f69b),
        f64::from_bits(0x3ff9_21fb_5444_2d18),
    ];
    const ATANLO: [f64; 4] = [
        f64::from_bits(0x3c7a_2b7f_222f_65e2),
        f64::from_bits(0x3c81_a626_3314_5c07),
        f64::from_bits(0x3c70_0788_7af0_cbbd),
        f64::from_bits(0x3c91_a626_3314_5c07),
    ];
    const AT: [f64; 11] = [
        f64::from_bits(0x3fd5_5555_5555_550d),
        -f64::from_bits(0x3fc9_9999_9998_ebc4),
        f64::from_bits(0x3fc2_4924_9200_83ff),
        -f64::from_bits(0x3fbc_71c6_fe23_1671),
        f64::from_bits(0x3fb7_45cd_c54c_206e),
        -f64::from_bits(0x3fb3_b0f2_af74_9a6d),
        f64::from_bits(0x3fb1_0d66_a0d0_3d51),
        -f64::from_bits(0x3fad_de2d_52de_fd9a),
        f64::from_bits(0x3fa9_7b4b_2476_0deb),
        -f64::from_bits(0x3fa2_b444_2c6a_6c2f),
        f64::from_bits(0x3f90_ad3a_e322_da11),
    ];
    let mut x = x;
    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS;
    let id: i32;
    if ix >= 0x4410_0000 {
        // if |x| >= 2^66
        if ix > EXP_BITS || (ix == EXP_BITS && lo(x) != 0) {
            return x + x; // NaN
        }
        return if hx > 0 {
            ATANHI[3] + ATANLO[3]
        } else {
            -ATANHI[3] - ATANLO[3]
        };
    }
    if ix < 0x3fdc_0000 {
        // |x| < 0.4375
        if ix < 0x3e20_0000 && HUGE + x > 1.0 {
            // |x| < 2^-29, raise inexact
            return x;
        }
        id = -1;
    } else {
        x = x.abs();
        if ix < 0x3ff3_0000 {
            // |x| < 1.1875
            if ix < 0x3fe6_0000 {
                // 7/16 <= |x| < 11/16
                id = 0;
                x = (2.0 * x - 1.0) / (2.0 + x);
            } else {
                // 11/16 <= |x| < 19/16
                id = 1;
                x = (x - 1.0) / (x + 1.0);
            }
        } else if ix < 0x4003_8000 {
            // |x| < 2.4375
            id = 2;
            x = (x - 1.5) / (1.0 + 1.5 * x);
        } else {
            // 2.4375 <= |x| < 2^66
            id = 3;
            x = -1.0 / x;
        }
    }
    // end of argument reduction
    let z = x * x;
    let w = z * z;
    // break sum from i=0 to 10 aT[i]z**(i+1) into odd and even poly
    let s1 = z * (AT[0] + w * (AT[2] + w * (AT[4] + w * (AT[6] + w * (AT[8] + w * AT[10])))));
    let s2 = w * (AT[1] + w * (AT[3] + w * (AT[5] + w * (AT[7] + w * AT[9]))));
    if id < 0 {
        x - x * (s1 + s2)
    } else {
        let id = id as usize;
        let z = ATANHI[id] - ((x * (s1 + s2) - ATANLO[id]) - x);
        if hx < 0 {
            -z
        } else {
            z
        }
    }
}

/// `FdLibm.Atan2.compute` — `StrictMath.atan2(y, x)`.
pub fn atan2(y: f64, x: f64) -> f64 {
    const TINY: f64 = 1.0e-300;
    const PI_O_4: f64 = f64::from_bits(0x3fe9_21fb_5444_2d18);
    const PI_O_2: f64 = f64::from_bits(0x3ff9_21fb_5444_2d18);
    const PI_LO: f64 = f64::from_bits(0x3ca1_a626_3314_5c07);
    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS;
    let lx = lo(x);
    let hy = hi(y);
    let iy = hy & EXP_SIGNIF_BITS;
    let ly = lo(y);
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    if (hx.wrapping_sub(0x3ff0_0000) | lx) == 0 {
        // x = 1.0
        return atan(y);
    }
    let m = ((hy >> 31) & 1) | ((hx >> 30) & 2); // 2*sign(x) + sign(y)

    // when y = 0
    if (iy | ly) == 0 {
        match m {
            0 | 1 => return y,      // atan(+/-0, +anything)  = +/-0
            2 => return PI + TINY,  // atan(+0,   -anything)  =  pi
            3 => return -PI - TINY, // atan(-0,   -anything)  = -pi
            _ => {}
        }
    }
    // when x = 0
    if (ix | lx) == 0 {
        return if hy < 0 {
            -PI_O_2 - TINY
        } else {
            PI_O_2 + TINY
        };
    }
    // when x is INF
    if ix == EXP_BITS {
        if iy == EXP_BITS {
            match m {
                0 => return PI_O_4 + TINY,
                1 => return -PI_O_4 - TINY,
                2 => return 3.0 * PI_O_4 + TINY,
                3 => return -3.0 * PI_O_4 - TINY,
                _ => {}
            }
        } else {
            match m {
                0 => return 0.0,
                1 => return -0.0,
                2 => return PI + TINY,
                3 => return -PI - TINY,
                _ => {}
            }
        }
    }
    // when y is INF
    if iy == EXP_BITS {
        return if hy < 0 {
            -PI_O_2 - TINY
        } else {
            PI_O_2 + TINY
        };
    }
    // compute y/x
    let k = (iy - ix) >> 20;
    let z = if k > 60 {
        // |y/x| >  2**60
        PI_O_2 + 0.5 * PI_LO
    } else if hx < 0 && k < -60 {
        // |y|/x < -2**60
        0.0
    } else {
        // safe to do y/x
        atan((y / x).abs())
    };
    match m {
        0 => z,                // atan(+, +)
        1 => -z,               // atan(-, +)
        2 => PI - (z - PI_LO), // atan(+, -)
        _ => (z - PI_LO) - PI, // atan(-, -), case 3
    }
}

/// `FdLibm.Cbrt.compute` — `StrictMath.cbrt`.
pub fn cbrt(x: f64) -> f64 {
    const B1: i32 = 715094163; // B1 = (682-0.03306235651)*2**20
    const B2: i32 = 696219795; // B2 = (664-0.03306235651)*2**20
    const C: f64 = f64::from_bits(0x3fe1_5f15_f15f_15f1);
    const D: f64 = -f64::from_bits(0x3fe6_91de_2532_c834);
    const E: f64 = f64::from_bits(0x3ff6_a0ea_0ea0_ea0f);
    const F: f64 = f64::from_bits(0x3ff9_b6db_6db6_db6e);
    const G: f64 = f64::from_bits(0x3fd6_db6d_b6db_6db7);
    if x == 0.0 || !x.is_finite() {
        return x; // Handles signed zeros properly
    }
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let mut t = 0.0;
    // Rough cbrt to 5 bits
    if x < f64::from_bits(0x0010_0000_0000_0000) {
        // subnormal number
        t = f64::from_bits(0x4350_0000_0000_0000);
        t *= x;
        t = with_hi(t, hi(t) / 3 + B2);
    } else {
        let hx = hi(x);
        t = with_hi(t, hx / 3 + B1);
    }
    // New cbrt to 23 bits, may be implemented in single precision
    let mut r = t * t / x;
    let mut s = C + r * t;
    t *= G + F / (s + E + D / s);
    // Chopped to 20 bits and make it larger than cbrt(x)
    t = with_lo(t, 0);
    t = with_hi(t, hi(t) + 0x00000001);
    // One step newton iteration to 53 bits with error less than 0.667 ulps
    s = t * t; // t*t is exact
    r = x / s;
    let w = t + t;
    r = (r - t) / (w + r); // r-s is exact
    t += t * r;
    // Restore the original sign bit
    sign * t
}

/// `FdLibm.Hypot.compute` — `StrictMath.hypot`.
pub fn hypot(x: f64, y: f64) -> f64 {
    const TWO_MINUS_600: f64 = f64::from_bits(0x1a70_0000_0000_0000);
    const TWO_PLUS_600: f64 = f64::from_bits(0x6570_0000_0000_0000);
    let mut a = x.abs();
    let mut b = y.abs();
    if !a.is_finite() || !b.is_finite() {
        if a == f64::INFINITY || b == f64::INFINITY {
            return f64::INFINITY;
        }
        return a + b; // Propagate NaN significand bits
    }
    if b > a {
        std::mem::swap(&mut a, &mut b);
    }
    let mut ha = hi(a);
    let mut hb = hi(b);
    if (ha - hb) > 0x3c00000 {
        return a + b; // x / y > 2**60
    }
    let mut k = 0;
    if a > f64::from_bits(0x5f30_0000_ffff_ffff) {
        // a > ~2**500, scale a and b by 2**-600
        ha -= 0x25800000;
        hb -= 0x25800000;
        a *= TWO_MINUS_600;
        b *= TWO_MINUS_600;
        k += 600;
    }
    if b < f64::from_bits(0x20b0_0000_0000_0000) {
        // b < 2**-500
        if b < f64::MIN_POSITIVE {
            // subnormal b or 0
            if b == 0.0 {
                return a;
            }
            let t1 = f64::from_bits(0x7fd0_0000_0000_0000); // t1 = 2^1022
            b *= t1;
            a *= t1;
            k -= 1022;
        } else {
            // scale a and b by 2^600
            ha += 0x25800000; // a *= 2^600
            hb += 0x25800000; // b *= 2^600
            a *= TWO_PLUS_600;
            b *= TWO_PLUS_600;
            k -= 600;
        }
    }
    // medium size a and b
    let mut w = a - b;
    if w > b {
        let t1 = with_hi(0.0, ha);
        let t2 = a - t1;
        w = (t1 * t1 - (b * (-b) - t2 * (a + t1))).sqrt();
    } else {
        a += a;
        let y1 = with_hi(0.0, hb);
        let y2 = b - y1;
        let t1 = with_hi(0.0, ha + 0x00100000);
        let t2 = a - t1;
        w = (t1 * y1 - (w * (-w) - (t1 * y2 + t2 * b))).sqrt();
    }
    if k != 0 {
        power_of_two(k) * w
    } else {
        w
    }
}

/// `FdLibm.Pow.compute` — `StrictMath.pow`.
pub fn pow(x: f64, y: f64) -> f64 {
    // y == zero: x**0 = 1
    if y == 0.0 {
        return 1.0;
    }
    // +/-NaN return x + y to propagate NaN significands
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    let y_abs = y.abs();
    let mut x_abs = x.abs();
    // Special values of y
    if y == 2.0 {
        return x * x;
    } else if y == 0.5 {
        if x >= -f64::MAX {
            // Handle x == -infinity later
            return (x + 0.0).sqrt(); // Add 0.0 to properly handle x == -0.0
        }
    } else if y_abs == 1.0 {
        // y is  +/-1
        return if y == 1.0 { x } else { 1.0 / x };
    } else if y_abs == f64::INFINITY {
        // y is +/-infinity
        if x_abs == 1.0 {
            #[allow(clippy::eq_op)]
            let nan = y - y; // inf**+/-1 is NaN
            return nan;
        } else if x_abs > 1.0 {
            // (|x| > 1)**+/-inf = inf, 0
            return if y >= 0.0 { y } else { 0.0 };
        } else {
            // (|x| < 1)**-/+inf = inf, 0
            return if y < 0.0 { -y } else { 0.0 };
        }
    }

    let hx = hi(x);
    let mut ix = hx & EXP_SIGNIF_BITS;

    // When x < 0, determine if y is an odd integer: 0 not an integer, 1 an
    // odd int, 2 an even int.
    let mut y_is_int = 0;
    if hx < 0 {
        if y_abs >= f64::from_bits(0x4340_0000_0000_0000) {
            y_is_int = 2; // y is an even integer since ulp(2^53) = 2.0
        } else if y_abs >= 1.0 {
            let y_abs_as_long = y_abs as i64;
            if (y_abs_as_long as f64) == y_abs {
                y_is_int = 2 - (y_abs_as_long & 0x1) as i32;
            }
        }
    }

    // Special value of x
    if x_abs == 0.0 || x_abs == f64::INFINITY || x_abs == 1.0 {
        let mut z = x_abs; // x is +/-0, +/-inf, +/-1
        if y < 0.0 {
            z = 1.0 / z; // z = (1/|x|)
        }
        if hx < 0 {
            if ((ix - 0x3ff00000) | y_is_int) == 0 {
                z = nan_of(z); // (-1)**non-int is NaN
            } else if y_is_int == 1 {
                z *= -1.0; // (x < 0)**odd = -(|x|**odd)
            }
        }
        return z;
    }

    let mut n = (hx >> 31) + 1;

    // (x < 0)**(non-int) is NaN
    if (n | y_is_int) == 0 {
        return nan_of(x);
    }

    let mut s = 1.0; // s (sign of result -ve**odd) = -1 else = 1
    if (n | (y_is_int - 1)) == 0 {
        s = -1.0; // (-ve)**(odd int)
    }

    let mut p_h;
    let mut p_l;
    // |y| is huge
    let (t1, t2) = if y_abs > f64::from_bits(0x41e0_0000_ffff_ffff) {
        // if |y| > ~2**31
        const INV_LN2: f64 = f64::from_bits(0x3ff7_1547_652b_82fe); // 1/ln2
        const INV_LN2_H: f64 = f64::from_bits(0x3ff7_1547_6000_0000); // 21 bits of 1/ln2
        const INV_LN2_L: f64 = f64::from_bits(0x3e99_4ae0_bf85_ddf4); // 1/ln2 tail

        // Over/underflow if x is not close to one
        if x_abs < f64::from_bits(0x3fef_ffff_0000_0000) {
            // |x| < ~0.9999995231628418
            return if y < 0.0 { s * f64::INFINITY } else { s * 0.0 };
        }
        if x_abs > f64::from_bits(0x3ff0_0000_ffff_ffff) {
            // |x| > ~1.0
            return if y > 0.0 { s * f64::INFINITY } else { s * 0.0 };
        }
        // Now |1-x| is tiny <= 2**-20, sufficient to compute log(x) by
        // x - x^2/2 + x^3/3 - x^4/4
        let t = x_abs - 1.0; // t has 20 trailing zeros
        let w = (t * t) * (0.5 - t * (f64::from_bits(0x3fd5_5555_5555_5555) - t * 0.25));
        let u = INV_LN2_H * t; // INV_LN2_H has 21 sig. bits
        let v = t * INV_LN2_L - w * INV_LN2;
        let t1 = with_lo(u + v, 0);
        (t1, v - (t1 - u))
    } else {
        const CP: f64 = f64::from_bits(0x3fee_c709_dc3a_03fd); // 2/(3ln2)
        const CP_H: f64 = f64::from_bits(0x3fee_c709_e000_0000); // (float)cp
        const CP_L: f64 = -f64::from_bits(0x3e3e_2fe0_145b_01f5); // tail of CP_H

        n = 0;
        // Take care of subnormal numbers
        if ix < 0x00100000 {
            x_abs *= f64::from_bits(0x4340_0000_0000_0000); // 2^53 = 9007199254740992.0
            n -= 53;
            ix = hi(x_abs);
        }
        n += (ix >> 20) - 0x3ff;
        let j = ix & 0x000fffff;
        // Determine interval
        ix = j | 0x3ff00000; // Normalize ix
        let k;
        if j <= 0x3988E {
            k = 0; // |x| <sqrt(3/2)
        } else if j < 0xBB67A {
            k = 1; // |x| <sqrt(3)
        } else {
            k = 0;
            n += 1;
            ix -= 0x00100000;
        }
        x_abs = with_hi(x_abs, ix);

        // Compute ss = s_h + s_l = (x-1)/(x+1) or (x-1.5)/(x+1.5)
        const DP_H: f64 = f64::from_bits(0x3fe2_b803_4000_0000);
        const DP_L: f64 = f64::from_bits(0x3e4c_fdeb_43cf_d006);

        // Poly coefs for (3/2)*(log(x)-2s-2/3*s**3
        const L1: f64 = f64::from_bits(0x3fe3_3333_3333_3303);
        const L2: f64 = f64::from_bits(0x3fdb_6db6_db6f_abff);
        const L3: f64 = f64::from_bits(0x3fd5_5555_518f_264d);
        const L4: f64 = f64::from_bits(0x3fd1_7460_a91d_4101);
        const L5: f64 = f64::from_bits(0x3fcd_864a_93c9_db65);
        const L6: f64 = f64::from_bits(0x3fca_7e28_4a45_4eef);

        let bp_k = 1.0 + 0.5 * k as f64; // BP[0]=1.0, BP[1]=1.5
        let u = x_abs - bp_k;
        let v = 1.0 / (x_abs + bp_k);
        let ss = u * v;
        let s_h = with_lo(ss, 0);
        // t_h=x_abs + BP[k] High
        let t_h = with_hi(0.0, ((ix >> 1) | 0x20000000) + 0x00080000 + (k << 18));
        let t_l = x_abs - (t_h - bp_k);
        let s_l = v * ((u - s_h * t_h) - s_h * t_l);
        // Compute log(x_abs)
        let mut s2 = ss * ss;
        let mut r = s2 * s2 * (L1 + s2 * (L2 + s2 * (L3 + s2 * (L4 + s2 * (L5 + s2 * L6)))));
        r += s_l * (s_h + ss);
        s2 = s_h * s_h;
        let t_h = with_lo(3.0 + s2 + r, 0);
        let t_l = r - ((t_h - 3.0) - s2);
        // u+v = ss*(1+...)
        let u = s_h * t_h;
        let v = s_l * t_h + t_l * ss;
        // 2/(3log2)*(ss + ...)
        p_h = with_lo(u + v, 0);
        p_l = v - (p_h - u);
        let z_h = CP_H * p_h; // CP_H + CP_L = 2/(3*log2)
        let z_l = CP_L * p_h + p_l * CP + DP_L * k as f64;
        // log2(x_abs) = (ss + ..)*2/(3*log2) = n + DP_H + z_h + z_l
        let t = n as f64;
        let t1 = with_lo(((z_h + z_l) + DP_H * k as f64) + t, 0);
        (t1, z_l - (((t1 - t) - DP_H * k as f64) - z_h))
    };

    // Split up y into (y1 + y2) and compute (y1 + y2) * (t1 + t2)
    let y1 = with_lo(y, 0);
    p_l = (y - y1) * t1 + y * t2;
    p_h = y1 * t1;
    let mut z = p_l + p_h;
    let mut j = hi(z);
    let i = lo(z);
    if j >= 0x40900000 {
        // z >= 1024
        if ((j - 0x40900000) | i) != 0 {
            // if z > 1024
            return s * f64::INFINITY; // Overflow
        } else {
            // -(1024-log2(ovfl+.5ulp))
            const OVT: f64 = f64::from_bits(0x3c97_1547_652b_82fe); // 8.0085662595372944372e-0017
            if p_l + OVT > z - p_h {
                return s * f64::INFINITY; // Overflow
            }
        }
    } else if (j & EXP_SIGNIF_BITS) >= 0x4090cc00 {
        // z <= -1075
        if (j.wrapping_sub(0xc090cc00_u32 as i32) | i) != 0 {
            // z < -1075
            return s * 0.0; // Underflow
        } else if p_l <= z - p_h {
            return s * 0.0; // Underflow
        }
    }
    // Compute 2**(p_h+p_l)
    const P1: f64 = f64::from_bits(0x3fc5_5555_5555_553e);
    const P2: f64 = -f64::from_bits(0x3f66_c16c_16be_bd93);
    const P3: f64 = f64::from_bits(0x3f11_566a_af25_de2c);
    const P4: f64 = -f64::from_bits(0x3ebb_bd41_c5d2_6bf1);
    const P5: f64 = f64::from_bits(0x3e66_3769_72be_a4d0);
    const LG2: f64 = f64::from_bits(0x3fe6_2e42_fefa_39ef);
    const LG2_H: f64 = f64::from_bits(0x3fe6_2e43_0000_0000);
    const LG2_L: f64 = -f64::from_bits(0x3e20_5c61_0ca8_6c39);
    let i = j & EXP_SIGNIF_BITS;
    let mut k = (i >> 20) - 0x3ff;
    n = 0;
    if i > 0x3fe00000 {
        // if |z| > 0.5, set n = [z + 0.5]
        n = j + (0x00100000 >> (k + 1));
        k = ((n & EXP_SIGNIF_BITS) >> 20) - 0x3ff; // new k for n
        let t = with_hi(0.0, n & !(0x000fffff >> k));
        n = ((n & 0x000fffff) | 0x00100000) >> (20 - k);
        if j < 0 {
            n = -n;
        }
        p_h -= t;
    }
    let t = with_lo(p_l + p_h, 0);
    let u = t * LG2_H;
    let v = (p_l - (t - p_h)) * LG2 + t * LG2_L;
    z = u + v;
    let w = v - (z - u);
    let t = z * z;
    let t1 = z - t * (P1 + t * (P2 + t * (P3 + t * (P4 + t * P5))));
    let r = (z * t1) / (t1 - 2.0) - (w + z * w);
    z = 1.0 - (r - z);
    j = hi(z);
    j = j.wrapping_add(n << 20);
    if (j >> 20) <= 0 {
        z = scalb(z, n); // subnormal output
    } else {
        let z_hi = hi(z).wrapping_add(n << 20);
        z = with_hi(z, z_hi);
    }
    s * z
}

/// `FdLibm.Exp.compute` — `StrictMath.exp`.
pub fn exp(x: f64) -> f64 {
    const TWOM1000: f64 = f64::from_bits(0x0170_0000_0000_0000);
    const O_THRESHOLD: f64 = f64::from_bits(0x4086_2e42_fefa_39ef);
    const U_THRESHOLD: f64 = -f64::from_bits(0x4087_4910_d52d_3051);
    const LN2HI: f64 = f64::from_bits(0x3fe6_2e42_fee0_0000);
    const LN2LO: [f64; 2] = [
        f64::from_bits(0x3dea_39ef_3579_3c76),
        -f64::from_bits(0x3dea_39ef_3579_3c76),
    ];
    const INVLN2: f64 = f64::from_bits(0x3ff7_1547_652b_82fe);
    const P1: f64 = f64::from_bits(0x3fc5_5555_5555_553e);
    const P2: f64 = -f64::from_bits(0x3f66_c16c_16be_bd93);
    const P3: f64 = f64::from_bits(0x3f11_566a_af25_de2c);
    const P4: f64 = -f64::from_bits(0x3ebb_bd41_c5d2_6bf1);
    const P5: f64 = f64::from_bits(0x3e66_3769_72be_a4d0);

    let mut x = x;
    let mut hi_part = 0.0;
    let mut lo_part = 0.0;
    let mut k = 0;
    let mut hx = hi(x);
    let xsb = (hx >> 31) & 1; // sign bit of x
    hx &= EXP_SIGNIF_BITS; // high word of |x|

    // filter out non-finite argument
    if hx >= 0x40862E42 {
        // if |x| >= 709.78...
        if hx >= 0x7ff00000 {
            if ((hx & 0xfffff) | lo(x)) != 0 {
                return x + x; // NaN
            }
            return if xsb == 0 { x } else { 0.0 }; // exp(+-inf) = {inf, 0}
        }
        if x > O_THRESHOLD {
            return HUGE * HUGE; // overflow
        }
        if x < U_THRESHOLD {
            return TWOM1000 * TWOM1000; // underflow
        }
    }

    // argument reduction
    if hx > 0x3fd62e42 {
        // if  |x| > 0.5 ln2
        if hx < 0x3FF0A2B2 {
            // and |x| < 1.5 ln2
            hi_part = x - LN2HI * (1 - 2 * xsb) as f64; // +/- ln2HI
            lo_part = LN2LO[xsb as usize];
            k = 1 - xsb - xsb;
        } else {
            k = (INVLN2 * x + 0.5 * (1 - 2 * xsb) as f64) as i32;
            let t = k as f64;
            hi_part = x - t * LN2HI; // t*ln2HI is exact here
            lo_part = t * LN2LO[0];
        }
        x = hi_part - lo_part;
    } else if hx < 0x3e300000 {
        // when |x|<2**-28
        if HUGE + x > 1.0 {
            return 1.0 + x; // trigger inexact
        }
    } else {
        k = 0;
    }

    // x is now in primary range
    let t = x * x;
    let c = x - t * (P1 + t * (P2 + t * (P3 + t * (P4 + t * P5))));
    if k == 0 {
        return 1.0 - ((x * c) / (c - 2.0) - x);
    }
    let y = 1.0 - ((lo_part - (x * c) / (2.0 - c)) - hi_part);
    if k >= -1021 {
        with_hi(y, hi(y).wrapping_add(k << 20)) // add k to y's exponent
    } else {
        with_hi(y, hi(y).wrapping_add((k + 1000) << 20)) * TWOM1000
    }
}

/// `FdLibm.Log.compute` — `StrictMath.log`, the natural logarithm.
pub fn log(x: f64) -> f64 {
    const LN2_HI: f64 = f64::from_bits(0x3fe6_2e42_fee0_0000);
    const LN2_LO: f64 = f64::from_bits(0x3dea_39ef_3579_3c76);
    const LG1: f64 = f64::from_bits(0x3fe5_5555_5555_5593);
    const LG2: f64 = f64::from_bits(0x3fd9_9999_9997_fa04);
    const LG3: f64 = f64::from_bits(0x3fd2_4924_9422_9359);
    const LG4: f64 = f64::from_bits(0x3fcc_71c5_1d8e_78af);
    const LG5: f64 = f64::from_bits(0x3fc7_4664_96cb_03de);
    const LG6: f64 = f64::from_bits(0x3fc3_9a09_d078_c69f);
    const LG7: f64 = f64::from_bits(0x3fc2_f112_df3e_5244);
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
    let mut i = (hx + 0x9_5f64) & 0x10_0000;
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

/// `FdLibm.Log10.compute` — `StrictMath.log10`.
pub fn log10(x: f64) -> f64 {
    const IVLN10: f64 = f64::from_bits(0x3fdb_cb7b_1526_e50e);
    const LOG10_2HI: f64 = f64::from_bits(0x3fd3_4413_509f_6000);
    const LOG10_2LO: f64 = f64::from_bits(0x3d59_fef3_11f1_2b36);
    let mut x = x;
    let mut hx = hi(x);
    let lx = lo(x);
    let mut k = 0;
    if hx < 0x0010_0000 {
        // x < 2**-1022
        if ((hx & EXP_SIGNIF_BITS) | lx) == 0 {
            return -TWO54 / 0.0; // log(+-0)=-inf
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
    let i = ((k & SIGN_BIT) as u32 >> 31) as i32; // unsigned shift
    hx = (hx & 0x000f_ffff) | ((0x3ff - i) << 20);
    let y = (k + i) as f64;
    x = with_hi(x, hx); // replace high word of x with hx
    let z = y * LOG10_2LO + IVLN10 * log(x);
    z + y * LOG10_2HI
}

/// `FdLibm.Log1p.compute` — `StrictMath.log1p`.
pub fn log1p(x: f64) -> f64 {
    const LN2_HI: f64 = f64::from_bits(0x3fe6_2e42_fee0_0000);
    const LN2_LO: f64 = f64::from_bits(0x3dea_39ef_3579_3c76);
    const LP1: f64 = f64::from_bits(0x3fe5_5555_5555_5593);
    const LP2: f64 = f64::from_bits(0x3fd9_9999_9997_fa04);
    const LP3: f64 = f64::from_bits(0x3fd2_4924_9422_9359);
    const LP4: f64 = f64::from_bits(0x3fcc_71c5_1d8e_78af);
    const LP5: f64 = f64::from_bits(0x3fc7_4664_96cb_03de);
    const LP6: f64 = f64::from_bits(0x3fc3_9a09_d078_c69f);
    const LP7: f64 = f64::from_bits(0x3fc2_f112_df3e_5244);
    const TWO_THIRDS: f64 = f64::from_bits(0x3fe5_5555_5555_5555); // 0.66666666666666666

    let mut f = 0.0;
    let mut c = 0.0;
    let mut hu = 0;
    let hx = hi(x);
    let ax = hx & EXP_SIGNIF_BITS;

    let mut k = 1;
    if hx < 0x3FDA_827A {
        // x < 0.41422
        if ax >= 0x3ff0_0000 {
            // x <= -1.0
            return if x == -1.0 {
                f64::NEG_INFINITY // log1p(-1)=-inf
            } else {
                f64::NAN // log1p(x < -1) = NaN
            };
        }
        if ax < 0x3e20_0000 {
            // |x| < 2**-29
            if TWO54 + x > 0.0 && ax < 0x3c90_0000 {
                // |x| < 2**-54
                return x;
            }
            return x - x * x * 0.5;
        }
        if hx > 0 || hx <= 0xbfd2_bec3_u32 as i32 {
            // -0.2929 < x < 0.41422
            k = 0;
            f = x;
            hu = 1;
        }
    }
    if hx >= EXP_BITS {
        return x + x;
    }
    if k != 0 {
        let mut u;
        if hx < 0x4340_0000 {
            u = 1.0 + x;
            hu = hi(u); // high word of u
            k = (hu >> 20) - 1023;
            c = if k > 0 { 1.0 - (u - x) } else { x - (u - 1.0) }; // correction term
            c /= u;
        } else {
            u = x;
            hu = hi(u); // high word of u
            k = (hu >> 20) - 1023;
            c = 0.0;
        }
        hu &= 0x000f_ffff;
        if hu < 0x6_a09e {
            u = with_hi(u, hu | 0x3ff0_0000); // normalize u
        } else {
            k += 1;
            u = with_hi(u, hu | 0x3fe0_0000); // normalize u/2
            hu = (0x0010_0000 - hu) >> 2;
        }
        f = u - 1.0;
    }
    let hfsq = 0.5 * f * f;
    let kf = k as f64;
    if hu == 0 {
        // |f| < 2**-20
        if f == 0.0 {
            if k == 0 {
                return 0.0;
            }
            c += kf * LN2_LO;
            return kf * LN2_HI + c;
        }
        let r = hfsq * (1.0 - TWO_THIRDS * f);
        if k == 0 {
            return f - r;
        }
        return kf * LN2_HI - ((r - (kf * LN2_LO + c)) - f);
    }
    let s = f / (2.0 + f);
    let z = s * s;
    let r = z * (LP1 + z * (LP2 + z * (LP3 + z * (LP4 + z * (LP5 + z * (LP6 + z * LP7))))));
    if k == 0 {
        f - (hfsq - s * (hfsq + r))
    } else {
        kf * LN2_HI - ((hfsq - (s * (hfsq + r) + (kf * LN2_LO + c))) - f)
    }
}

/// `FdLibm.Expm1.compute` — `StrictMath.expm1`.
pub fn expm1(x: f64) -> f64 {
    const TINY: f64 = 1.0e-300;
    const O_THRESHOLD: f64 = f64::from_bits(0x4086_2e42_fefa_39ef);
    const LN2_HI: f64 = f64::from_bits(0x3fe6_2e42_fee0_0000);
    const LN2_LO: f64 = f64::from_bits(0x3dea_39ef_3579_3c76);
    const INVLN2: f64 = f64::from_bits(0x3ff7_1547_652b_82fe);
    const Q1: f64 = -f64::from_bits(0x3fa1_1111_1111_10f4);
    const Q2: f64 = f64::from_bits(0x3f5a_01a0_19fe_5585);
    const Q3: f64 = -f64::from_bits(0x3f14_ce19_9eaa_dbb7);
    const Q4: f64 = f64::from_bits(0x3ed0_cfca_86e6_5239);
    const Q5: f64 = -f64::from_bits(0x3e8a_fdb7_6e09_c32d);

    let mut x = x;
    let mut c = 0.0;
    let k;
    let mut hx = hi(x);
    let xsb = hx & SIGN_BIT; // sign bit of x
    hx &= EXP_SIGNIF_BITS; // high word of |x|

    // filter out huge and non-finite argument
    if hx >= 0x4043_687A {
        // if |x| >= 56*ln2
        if hx >= 0x4086_2E42 {
            // if |x| >= 709.78...
            if hx >= 0x7ff0_0000 {
                if ((hx & 0xf_ffff) | lo(x)) != 0 {
                    return x + x; // NaN
                }
                return if xsb == 0 { x } else { -1.0 }; // exp(+-inf)={inf,-1}
            }
            if x > O_THRESHOLD {
                return HUGE * HUGE; // overflow
            }
        }
        if xsb != 0 && x + TINY < 0.0 {
            // x < -56*ln2, return -1.0 with inexact
            return TINY - 1.0; // return -1
        }
    }

    // argument reduction
    if hx > 0x3fd6_2e42 {
        // if  |x| > 0.5 ln2
        let (hi_part, lo_part);
        if hx < 0x3FF0_A2B2 {
            // and |x| < 1.5 ln2
            if xsb == 0 {
                hi_part = x - LN2_HI;
                lo_part = LN2_LO;
                k = 1;
            } else {
                hi_part = x + LN2_HI;
                lo_part = -LN2_LO;
                k = -1;
            }
        } else {
            k = (INVLN2 * x + if xsb == 0 { 0.5 } else { -0.5 }) as i32;
            let t = k as f64;
            hi_part = x - t * LN2_HI; // t*ln2_hi is exact here
            lo_part = t * LN2_LO;
        }
        x = hi_part - lo_part;
        c = (hi_part - x) - lo_part;
    } else if hx < 0x3c90_0000 {
        // when |x| < 2**-54, return x
        let t = HUGE + x; // return x with inexact flags when x != 0
        return x - (t - (HUGE + x));
    } else {
        k = 0;
    }

    // x is now in primary range
    let hfx = 0.5 * x;
    let hxs = x * hfx;
    let r1 = 1.0 + hxs * (Q1 + hxs * (Q2 + hxs * (Q3 + hxs * (Q4 + hxs * Q5))));
    let mut t = 3.0 - r1 * hfx;
    let mut e = hxs * ((r1 - t) / (6.0 - x * t));
    if k == 0 {
        return x - (x * e - hxs); // c is 0
    }
    e = x * (e - c) - c;
    e -= hxs;
    if k == -1 {
        return 0.5 * (x - e) - 0.5;
    }
    if k == 1 {
        if x < -0.25 {
            return -2.0 * (e - (x + 0.5));
        }
        return 1.0 + 2.0 * (x - e);
    }
    let mut y;
    if k <= -2 || k > 56 {
        // suffice to return exp(x) - 1
        y = 1.0 - (e - x);
        y = with_hi(y, hi(y).wrapping_add(k << 20)); // add k to y's exponent
        return y - 1.0;
    }
    t = 1.0;
    if k < 20 {
        t = with_hi(t, 0x3ff0_0000 - (0x2_00000 >> k)); // t = 1-2^-k
        y = t - (e - x);
        y = with_hi(y, hi(y).wrapping_add(k << 20)); // add k to y's exponent
    } else {
        t = with_hi(t, (0x3ff - k) << 20); // 2^-k
        y = x - (e + t);
        y += 1.0;
        y = with_hi(y, hi(y).wrapping_add(k << 20)); // add k to y's exponent
    }
    y
}

/// `FdLibm.Sinh.compute` — `StrictMath.sinh`.
pub fn sinh(x: f64) -> f64 {
    const SHUGE: f64 = 1.0e307;
    let jx = hi(x);
    let ix = jx & EXP_SIGNIF_BITS;
    // x is INF or NaN
    if ix >= EXP_BITS {
        return x + x;
    }
    let h = if jx < 0 { -0.5 } else { 0.5 };
    // |x| in [0,22], return sign(x)*0.5*(E+E/(E+1)))
    if ix < 0x4036_0000 {
        // |x| < 22
        if ix < 0x3e30_0000 && SHUGE + x > 1.0 {
            // |x| < 2**-28, sinh(tiny) = tiny with inexact
            return x;
        }
        let t = expm1(x.abs());
        if ix < 0x3ff0_0000 {
            return h * (2.0 * t - t * t / (t + 1.0));
        }
        return h * (t + t / (t + 1.0));
    }
    // |x| in [22, log(maxdouble)] return 0.5*exp(|x|)
    if ix < 0x4086_2E42 {
        return h * exp(x.abs());
    }
    // |x| in [log(maxdouble), overflowthreshold]
    let lx = lo(x);
    if ix < 0x4086_33CE || (ix == 0x4086_33ce && (lx as u32) <= 0x8fb9_f87d) {
        let w = exp(0.5 * x.abs());
        let t = h * w;
        return t * w;
    }
    // |x| > overflowthreshold, sinh(x) overflow
    x * SHUGE
}

/// `FdLibm.Cosh.compute` — `StrictMath.cosh`.
pub fn cosh(x: f64) -> f64 {
    let ix = hi(x) & EXP_SIGNIF_BITS;
    // x is INF or NaN
    if ix >= EXP_BITS {
        return x * x;
    }
    // |x| in [0,0.5*ln2], return 1+expm1(|x|)^2/(2*exp(|x|))
    if ix < 0x3fd6_2e43 {
        let t = expm1(x.abs());
        let w = 1.0 + t;
        if ix < 0x3c80_0000 {
            return w; // cosh(tiny) = 1
        }
        return 1.0 + (t * t) / (w + w);
    }
    // |x| in [0.5*ln2, 22], return (exp(|x|) + 1/exp(|x|)/2
    if ix < 0x4036_0000 {
        let t = exp(x.abs());
        return 0.5 * t + 0.5 / t;
    }
    // |x| in [22, log(maxdouble)] return 0.5*exp(|x|)
    if ix < 0x4086_2E42 {
        return 0.5 * exp(x.abs());
    }
    // |x| in [log(maxdouble), overflowthreshold]
    let lx = lo(x);
    if ix < 0x4086_33CE || (ix == 0x4086_33ce && (lx as u32) <= 0x8fb9_f87d) {
        let w = exp(0.5 * x.abs());
        let t = 0.5 * w;
        return t * w;
    }
    // |x| > overflowthreshold, cosh(x) overflow
    HUGE * HUGE
}

/// `FdLibm.Tanh.compute` — `StrictMath.tanh`.
pub fn tanh(x: f64) -> f64 {
    const TINY: f64 = 1.0e-300;
    let jx = hi(x);
    let ix = jx & EXP_SIGNIF_BITS;
    // x is INF or NaN
    if ix >= EXP_BITS {
        return if jx >= 0 {
            1.0 / x + 1.0 // tanh(+-inf)=+-1
        } else {
            1.0 / x - 1.0 // tanh(NaN) = NaN
        };
    }
    let z;
    if ix < 0x4036_0000 {
        // |x| < 22
        if ix < 0x3c80_0000 {
            // |x| < 2**-55
            return x * (1.0 + x); // tanh(small) = small
        }
        if ix >= 0x3ff0_0000 {
            // |x| >= 1
            let t = expm1(2.0 * x.abs());
            z = 1.0 - 2.0 / (t + 2.0);
        } else {
            let t = expm1(-2.0 * x.abs());
            z = -t / (t + 2.0);
        }
    } else {
        // |x| > 22, return +-1
        z = 1.0 - TINY; // raised inexact flag
    }
    if jx >= 0 {
        z
    } else {
        -z
    }
}

/// `FdLibm.IEEEremainder.compute` — `StrictMath.IEEEremainder(x, p)`.
#[allow(clippy::eq_op)]
pub fn ieee_remainder(x: f64, p: f64) -> f64 {
    let mut x = x;
    let mut p = p;
    let mut hx = hi(x);
    let lx = lo(x);
    let mut hp = hi(p);
    let lp = lo(p);
    let sx = hx & SIGN_BIT;
    hp &= EXP_SIGNIF_BITS;
    hx &= EXP_SIGNIF_BITS;

    // purge off exception values
    if (hp | lp) == 0 {
        // p = 0
        return (x * p) / (x * p);
    }
    if hx >= EXP_BITS || (hp >= EXP_BITS && ((hp - EXP_BITS) | lp) != 0) {
        // x not finite, or p is NaN
        return (x * p) / (x * p);
    }
    if hp <= 0x7fdf_ffff {
        // now x < 2p
        x = fmod(x, p + p);
    }
    if ((hx - hp) | lx.wrapping_sub(lp)) == 0 {
        return 0.0 * x;
    }
    x = x.abs();
    p = p.abs();
    if hp < 0x0020_0000 {
        if x + x > p {
            x -= p;
            if x + x >= p {
                x -= p;
            }
        }
    } else {
        let p_half = 0.5 * p;
        if x > p_half {
            x -= p;
            if x >= p_half {
                x -= p;
            }
        }
    }
    with_hi(x, hi(x) ^ sx)
}

/// `FdLibm.IEEEremainder.__ieee754_fmod`: the exact `x mod y`.
#[allow(clippy::eq_op)]
fn fmod(x: f64, y: f64) -> f64 {
    let mut hx = hi(x);
    let mut lx = lo(x);
    let mut hy = hi(y);
    let mut ly = lo(y);
    let sx = hx & SIGN_BIT; // sign of x
    hx ^= sx; // |x|
    hy &= EXP_SIGNIF_BITS; // |y|

    // purge off exception values
    if (hy | ly) == 0
        || hx >= EXP_BITS
        || (hy | ((ly | ly.wrapping_neg()) as u32 >> 31) as i32) > EXP_BITS
    {
        // y = 0, or x not finite, or y is NaN
        return (x * y) / (x * y);
    }
    if hx <= hy {
        if hx < hy || (lx as u32) < (ly as u32) {
            return x; // |x| < |y| return x
        }
        if lx == ly {
            return signed_zero(sx); // |x| = |y| return x*0
        }
    }

    let ix = ilogb(hx, lx);
    let mut iy = ilogb(hy, ly);

    // set up {hx, lx}, {hy, ly} and align y to x
    if ix >= -1022 {
        hx = 0x0010_0000 | (0x000f_ffff & hx);
    } else {
        // subnormal x, shift x to normal
        let n = -1022 - ix;
        if n <= 31 {
            hx = (hx << n) | (lx as u32 >> (32 - n)) as i32;
            lx <<= n;
        } else {
            hx = lx << (n - 32);
            lx = 0;
        }
    }
    if iy >= -1022 {
        hy = 0x0010_0000 | (0x000f_ffff & hy);
    } else {
        // subnormal y, shift y to normal
        let n = -1022 - iy;
        if n <= 31 {
            hy = (hy << n) | (ly as u32 >> (32 - n)) as i32;
            ly <<= n;
        } else {
            hy = ly << (n - 32);
            ly = 0;
        }
    }

    // fix point fmod
    let mut n = ix - iy;
    while n != 0 {
        n -= 1;
        let mut hz = hx.wrapping_sub(hy);
        let lz = lx.wrapping_sub(ly);
        if (lx as u32) < (ly as u32) {
            hz = hz.wrapping_sub(1);
        }
        if hz < 0 {
            hx = hx.wrapping_add(hx).wrapping_add((lx as u32 >> 31) as i32);
            lx = lx.wrapping_add(lx);
        } else {
            if (hz | lz) == 0 {
                return signed_zero(sx); // return sign(x)*0
            }
            hx = hz.wrapping_add(hz).wrapping_add((lz as u32 >> 31) as i32);
            lx = lz.wrapping_add(lz);
        }
    }
    let mut hz = hx.wrapping_sub(hy);
    let lz = lx.wrapping_sub(ly);
    if (lx as u32) < (ly as u32) {
        hz = hz.wrapping_sub(1);
    }
    if hz >= 0 {
        hx = hz;
        lx = lz;
    }

    // convert back to floating value and restore the sign
    if (hx | lx) == 0 {
        return signed_zero(sx);
    }
    while hx < 0x0010_0000 {
        // normalize x
        hx = hx.wrapping_add(hx).wrapping_add((lx as u32 >> 31) as i32);
        lx = lx.wrapping_add(lx);
        iy -= 1;
    }
    if iy >= -1022 {
        // normalize output
        hx = (hx - 0x0010_0000) | ((iy + 1023) << 20);
        hi_lo(hx | sx, lx)
    } else {
        // subnormal output
        let n = -1022 - iy;
        if n <= 20 {
            lx = (lx as u32 >> n) as i32 | (hx << (32 - n));
            hx >>= n;
        } else if n <= 31 {
            lx = (hx << (32 - n)) | (lx as u32 >> n) as i32;
            hx = sx;
        } else {
            lx = hx >> (n - 32);
            hx = sx;
        }
        hi_lo(hx | sx, lx) * 1.0 // create necessary signal
    }
}

/// `+0.0 * (double) sign` — a zero carrying the sign of `sign`'s top bit.
fn signed_zero(sign: i32) -> f64 {
    0.0 * sign as f64
}

/// `FdLibm.IEEEremainder.ilogb` over a word pair.
fn ilogb(hz: i32, lz: i32) -> i32 {
    if hz < 0x0010_0000 {
        // subnormal z
        let mut iz;
        let mut i;
        if hz == 0 {
            iz = -1043;
            i = lz;
        } else {
            iz = -1022;
            i = hz << 11;
        }
        while i > 0 {
            iz -= 1;
            i <<= 1;
        }
        iz
    } else {
        (hz >> 20) - 1023
    }
}

/// `FdLibm.Asinh.compute` — `StrictMath.asinh`.
pub fn asinh(x: f64) -> f64 {
    const LN2: f64 = f64::from_bits(0x3fe6_2e42_fefa_39ef); // 6.93147180559945286227e-01
    let hx = hi(x);
    let ix = hx & 0x7fff_ffff;
    if ix >= 0x7ff0_0000 {
        return x + x; // x is inf or NaN
    }
    if ix < 0x3e30_0000 && HUGE + x > 1.0 {
        // |x| < 2**-28
        return x; // return x inexact except 0
    }
    let w = if ix > 0x41b0_0000 {
        // |x| > 2**28
        log(x.abs()) + LN2
    } else if ix > 0x4000_0000 {
        // 2**28 > |x| > 2.0
        let t = x.abs();
        log(2.0 * t + 1.0 / ((x * x + 1.0).sqrt() + t))
    } else {
        // 2.0 > |x| > 2**-28
        let t = x * x;
        log1p(x.abs() + t / (1.0 + (1.0 + t).sqrt()))
    };
    if hx > 0 {
        w
    } else {
        -w
    }
}

/// `FdLibm.Acosh.compute` — `StrictMath.acosh`.
pub fn acosh(x: f64) -> f64 {
    const LN2: f64 = f64::from_bits(0x3fe6_2e42_fefa_39ef); // 6.93147180559945286227e-01
    let hx = hi(x);
    if hx < 0x3ff0_0000 {
        // x < 1
        nan_of(x)
    } else if hx >= 0x41b0_0000 {
        // x > 2**28
        if hx >= 0x7ff0_0000 {
            x + x // x is inf of NaN
        } else {
            log(x) + LN2 // acosh(huge) = log(2x)
        }
    } else if ((hx - 0x3ff0_0000) | lo(x)) == 0 {
        0.0 // acosh(1) = 0
    } else if hx > 0x4000_0000 {
        // 2**28 > x > 2
        let t = x * x;
        log(2.0 * x - 1.0 / (x + (t - 1.0).sqrt()))
    } else {
        // 1 < x < 2
        let t = x - 1.0;
        log1p(t + (2.0 * t + t * t).sqrt())
    }
}

/// `FdLibm.Atanh.compute` — `StrictMath.atanh`.
pub fn atanh(x: f64) -> f64 {
    let hx = hi(x);
    let lx = lo(x);
    let ix = hx & 0x7fff_ffff;
    if (ix | ((lx | lx.wrapping_neg()) as u32 >> 31) as i32) > 0x3ff0_0000 {
        // |x| > 1
        return nan_of(x);
    }
    if ix == 0x3ff0_0000 {
        return x / 0.0;
    }
    if ix < 0x3e30_0000 && (HUGE + x) > 0.0 {
        return x; // x<2**-28
    }
    let x = with_hi(x, ix); // x <- |x|
    let t = if ix < 0x3fe0_0000 {
        // x < 0.5
        let t = x + x;
        0.5 * log1p(t + t * x / (1.0 - x))
    } else {
        0.5 * log1p((x + x) / (1.0 - x))
    };
    if hx >= 0 {
        t
    } else {
        -t
    }
}
