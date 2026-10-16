//! IEEE 754 binary16/32/64 arithmetic with every RISC-V rounding mode, exact exception flags
//! and RISC-V NaN behaviour (operations that produce a NaN return the canonical NaN).
//!
//! Values are passed as raw bit patterns in a `u64`. Algorithms follow Berkeley SoftFloat 3e,
//! but share one generic core: finite operands are unpacked to `sig * 2^exp` with `sig` held in
//! a `u128`, operations compute an exact (or exact-plus-sticky-bit) intermediate, and
//! [`round_pack`] performs the single rounding step. Tininess is detected after rounding, as
//! RISC-V requires.

#![cfg_attr(not(test), no_std)]

/// Exception flags, in RISC-V `fflags` bit order.
pub mod flags {
    /// Inexact.
    pub const NX: u8 = 1;
    /// Underflow.
    pub const UF: u8 = 2;
    /// Overflow.
    pub const OF: u8 = 4;
    /// Divide by zero.
    pub const DZ: u8 = 8;
    /// Invalid operation.
    pub const NV: u8 = 16;
}

use flags::*;

/// Rounding modes, numbered as in RISC-V `frm`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Rm {
    /// Round to nearest, ties to even.
    Rne = 0,
    /// Round towards zero.
    Rtz = 1,
    /// Round down (towards -inf).
    Rdn = 2,
    /// Round up (towards +inf).
    Rup = 3,
    /// Round to nearest, ties to max magnitude.
    Rmm = 4,
}

impl Rm {
    pub const fn from_bits(bits: u64) -> Option<Rm> {
        Some(match bits {
            0 => Rm::Rne,
            1 => Rm::Rtz,
            2 => Rm::Rdn,
            3 => Rm::Rup,
            4 => Rm::Rmm,
            _ => return None,
        })
    }
}

/// An IEEE 754 binary interchange format.
pub trait Format {
    const EXP_BITS: u32;
    const FRAC_BITS: u32;
}

pub enum F16 {}
pub enum F32 {}
pub enum F64 {}

impl Format for F16 {
    const EXP_BITS: u32 = 5;
    const FRAC_BITS: u32 = 10;
}
impl Format for F32 {
    const EXP_BITS: u32 = 8;
    const FRAC_BITS: u32 = 23;
}
impl Format for F64 {
    const EXP_BITS: u32 = 11;
    const FRAC_BITS: u32 = 52;
}

#[inline(always)]
fn sign_bit<F: Format>() -> u64 {
    1 << (F::EXP_BITS + F::FRAC_BITS)
}
#[inline(always)]
fn exp_max<F: Format>() -> u64 {
    (1 << F::EXP_BITS) - 1
}
#[inline(always)]
fn frac_mask<F: Format>() -> u64 {
    (1 << F::FRAC_BITS) - 1
}
#[inline(always)]
fn bias<F: Format>() -> i32 {
    (1 << (F::EXP_BITS - 1)) - 1
}
/// Smallest normal exponent.
#[inline(always)]
fn emin<F: Format>() -> i32 {
    1 - bias::<F>()
}

#[inline(always)]
fn sign_of<F: Format>(x: u64) -> bool {
    x & sign_bit::<F>() != 0
}

#[inline(always)]
fn with_sign<F: Format>(sign: bool) -> u64 {
    if sign { sign_bit::<F>() } else { 0 }
}

pub fn canonical_nan<F: Format>() -> u64 {
    (exp_max::<F>() << F::FRAC_BITS) | (1 << (F::FRAC_BITS - 1))
}

pub fn infinity<F: Format>(sign: bool) -> u64 {
    with_sign::<F>(sign) | (exp_max::<F>() << F::FRAC_BITS)
}

fn max_finite<F: Format>(sign: bool) -> u64 {
    infinity::<F>(sign) - 1
}

pub fn is_nan<F: Format>(x: u64) -> bool {
    (x >> F::FRAC_BITS) & exp_max::<F>() == exp_max::<F>() && x & frac_mask::<F>() != 0
}

pub fn is_snan<F: Format>(x: u64) -> bool {
    is_nan::<F>(x) && x & (1 << (F::FRAC_BITS - 1)) == 0
}

fn is_inf<F: Format>(x: u64) -> bool {
    x & !sign_bit::<F>() == infinity::<F>(false)
}

fn is_zero<F: Format>(x: u64) -> bool {
    x & !sign_bit::<F>() == 0
}

/// A finite value `(-1)^sign * sig * 2^exp`.
#[derive(Clone, Copy, Debug)]
struct Unpacked {
    sign: bool,
    exp: i32,
    sig: u128,
}

fn unpack<F: Format>(x: u64) -> Unpacked {
    let exp_field = ((x >> F::FRAC_BITS) & exp_max::<F>()) as i32;
    let frac = (x & frac_mask::<F>()) as u128;
    let shift = F::FRAC_BITS as i32;
    if exp_field == 0 {
        Unpacked { sign: sign_of::<F>(x), exp: emin::<F>() - shift, sig: frac }
    } else {
        Unpacked {
            sign: sign_of::<F>(x),
            exp: exp_field - bias::<F>() - shift,
            sig: frac | (1 << F::FRAC_BITS),
        }
    }
}

/// `sig >> n`, ORing any bits shifted out into the result's least significant bit.
fn shift_right_jam(sig: u128, n: u32) -> u128 {
    if n == 0 {
        sig
    } else if n >= 128 {
        (sig != 0) as u128
    } else {
        (sig >> n) | ((sig & ((1 << n) - 1)) != 0) as u128
    }
}

/// Rounds `sig / 2^shift` to an integer. Returns the rounded value and whether it is inexact.
/// If `sig` carries a sticky bit, `shift` must be at least 2.
fn round_shift(sig: u128, shift: i32, sign: bool, rm: Rm) -> (u128, bool) {
    if shift <= 0 {
        return (sig << -shift, false);
    }
    let (q, rem, half_cmp) = if shift >= 128 {
        // Everything is shifted out. Compare `sig` with the half point 2^(shift-1).
        let cmp = if shift == 128 { sig.cmp(&(1 << 127)) } else { core::cmp::Ordering::Less };
        (0u128, sig, cmp)
    } else {
        let rem = sig & ((1u128 << shift) - 1);
        (sig >> shift, rem, rem.cmp(&(1u128 << (shift - 1))))
    };
    let inexact = rem != 0;
    use core::cmp::Ordering::*;
    let up = match rm {
        Rm::Rne => half_cmp == Greater || (half_cmp == Equal && q & 1 == 1),
        Rm::Rmm => half_cmp != Less,
        Rm::Rtz => false,
        Rm::Rdn => sign && inexact,
        Rm::Rup => !sign && inexact,
    };
    (q + up as u128, inexact)
}

/// Rounds the finite value `(-1)^sign * sig * 2^exp` to format `F`, raising flags.
fn round_pack<F: Format>(sign: bool, exp: i32, sig: u128, rm: Rm, fl: &mut u8) -> u64 {
    if sig == 0 {
        return with_sign::<F>(sign);
    }
    let m = F::FRAC_BITS as i32;
    let top = exp + (127 - sig.leading_zeros() as i32);
    let normal_lsb = top - m;
    let lsb = normal_lsb.max(emin::<F>() - m);
    let (q, inexact) = round_shift(sig, lsb - exp, sign, rm);

    if inexact {
        *fl |= NX;
        if top < emin::<F>() {
            // Tiny after rounding: rounding to full precision with an unbounded exponent
            // would still give a result below the smallest normal.
            let tiny = top < emin::<F>() - 1 || {
                let (q_unbounded, _) = round_shift(sig, normal_lsb - exp, sign, rm);
                q_unbounded < (1u128 << (m + 1))
            };
            if tiny {
                *fl |= UF;
            }
        }
    }

    // Biased encoding: a carry out of the significand bumps the exponent field naturally.
    let enc = (((lsb - (emin::<F>() - m)) as u128) << m) + q;
    if enc >= (exp_max::<F>() as u128) << m {
        *fl |= OF | NX;
        let to_inf = match rm {
            Rm::Rne | Rm::Rmm => true,
            Rm::Rtz => false,
            Rm::Rdn => sign,
            Rm::Rup => !sign,
        };
        return if to_inf { infinity::<F>(sign) } else { max_finite::<F>(sign) };
    }
    with_sign::<F>(sign) | enc as u64
}

/// Handles NaN operands: raises NV for signalling NaNs and returns the canonical NaN.
fn propagate_nan<F: Format>(ops: &[u64], fl: &mut u8) -> Option<u64> {
    if ops.iter().any(|&x| is_nan::<F>(x)) {
        if ops.iter().any(|&x| is_snan::<F>(x)) {
            *fl |= NV;
        }
        return Some(canonical_nan::<F>());
    }
    None
}

fn invalid<F: Format>(fl: &mut u8) -> u64 {
    *fl |= NV;
    canonical_nan::<F>()
}

/// Exact sum of two non-zero finite values, rounded.
fn add_unpacked<F: Format>(a: Unpacked, b: Unpacked, rm: Rm, fl: &mut u8) -> u64 {
    // Normalise both significands to bit 125, leaving two bits of headroom for the sum, then
    // align the smaller operand with a sticky shift. That leaves >= 70 guard bits.
    let norm = |u: Unpacked| {
        let shift = u.sig.leading_zeros() as i32 - 2;
        Unpacked { sign: u.sign, exp: u.exp - shift, sig: u.sig << shift }
    };
    let (mut a, mut b) = (norm(a), norm(b));
    if a.exp < b.exp {
        core::mem::swap(&mut a, &mut b);
    }
    let bsig = shift_right_jam(b.sig, (a.exp - b.exp) as u32);
    if a.sign == b.sign {
        return round_pack::<F>(a.sign, a.exp, a.sig + bsig, rm, fl);
    }
    match a.sig.cmp(&bsig) {
        core::cmp::Ordering::Equal => with_sign::<F>(rm == Rm::Rdn),
        core::cmp::Ordering::Greater => round_pack::<F>(a.sign, a.exp, a.sig - bsig, rm, fl),
        core::cmp::Ordering::Less => round_pack::<F>(b.sign, a.exp, bsig - a.sig, rm, fl),
    }
}

/// Sign of an exact zero sum `x + y` where both are zero or cancel exactly.
fn zero_sum_sign(sa: bool, sb: bool, rm: Rm) -> bool {
    if sa == sb { sa } else { rm == Rm::Rdn }
}

pub fn add<F: Format>(a: u64, b: u64, rm: Rm, fl: &mut u8) -> u64 {
    if let Some(nan) = propagate_nan::<F>(&[a, b], fl) {
        return nan;
    }
    let (sa, sb) = (sign_of::<F>(a), sign_of::<F>(b));
    match (is_inf::<F>(a), is_inf::<F>(b)) {
        (true, true) if sa != sb => return invalid::<F>(fl),
        (true, _) => return a,
        (_, true) => return b,
        _ => {}
    }
    match (is_zero::<F>(a), is_zero::<F>(b)) {
        (true, true) => with_sign::<F>(zero_sum_sign(sa, sb, rm)),
        (true, false) => b,
        (false, true) => a,
        (false, false) => add_unpacked::<F>(unpack::<F>(a), unpack::<F>(b), rm, fl),
    }
}

pub fn sub<F: Format>(a: u64, b: u64, rm: Rm, fl: &mut u8) -> u64 {
    let nb = if is_nan::<F>(b) { b } else { b ^ sign_bit::<F>() };
    add::<F>(a, nb, rm, fl)
}

pub fn mul<F: Format>(a: u64, b: u64, rm: Rm, fl: &mut u8) -> u64 {
    if let Some(nan) = propagate_nan::<F>(&[a, b], fl) {
        return nan;
    }
    let sign = sign_of::<F>(a) != sign_of::<F>(b);
    let (ia, ib) = (is_inf::<F>(a), is_inf::<F>(b));
    let (za, zb) = (is_zero::<F>(a), is_zero::<F>(b));
    if (ia && zb) || (ib && za) {
        return invalid::<F>(fl);
    }
    if ia || ib {
        return infinity::<F>(sign);
    }
    if za || zb {
        return with_sign::<F>(sign);
    }
    let (ua, ub) = (unpack::<F>(a), unpack::<F>(b));
    round_pack::<F>(sign, ua.exp + ub.exp, ua.sig * ub.sig, rm, fl)
}

pub fn div<F: Format>(a: u64, b: u64, rm: Rm, fl: &mut u8) -> u64 {
    if let Some(nan) = propagate_nan::<F>(&[a, b], fl) {
        return nan;
    }
    let sign = sign_of::<F>(a) != sign_of::<F>(b);
    let (ia, ib) = (is_inf::<F>(a), is_inf::<F>(b));
    let (za, zb) = (is_zero::<F>(a), is_zero::<F>(b));
    if (ia && ib) || (za && zb) {
        return invalid::<F>(fl);
    }
    if ia {
        return infinity::<F>(sign);
    }
    if zb {
        *fl |= DZ;
        return infinity::<F>(sign);
    }
    if ib || za {
        return with_sign::<F>(sign);
    }
    let (ua, ub) = (unpack::<F>(a), unpack::<F>(b));
    // Put the dividend's top bit at 125 so the quotient has at least 72 significant bits.
    let shift = ua.sig.leading_zeros() as i32 - 2;
    let num = ua.sig << shift;
    let q = num / ub.sig;
    let sticky = !num.is_multiple_of(ub.sig) as u128;
    round_pack::<F>(sign, ua.exp - shift - ub.exp, q | sticky, rm, fl)
}

/// Integer square root of `n`, rounded down.
fn isqrt(n: u128) -> u128 {
    if n == 0 {
        return 0;
    }
    // Start from an over-estimate and run Newton's method, which then decreases monotonically.
    let bits = 128 - n.leading_zeros();
    let mut x: u128 = 1 << bits.div_ceil(2);
    loop {
        let y = (x + n / x) >> 1;
        if y >= x {
            return x;
        }
        x = y;
    }
}

pub fn sqrt<F: Format>(a: u64, rm: Rm, fl: &mut u8) -> u64 {
    if let Some(nan) = propagate_nan::<F>(&[a], fl) {
        return nan;
    }
    if is_zero::<F>(a) {
        return a;
    }
    if sign_of::<F>(a) {
        return invalid::<F>(fl);
    }
    if is_inf::<F>(a) {
        return a;
    }
    let u = unpack::<F>(a);
    // Shift the top bit to 124 or 125 so that the exponent becomes even; the root then has
    // 63 significant bits.
    let mut shift = u.sig.leading_zeros() as i32 - 3;
    if (u.exp - shift) % 2 != 0 {
        shift += 1;
    }
    let n = u.sig << shift;
    let root = isqrt(n);
    let sticky = (root * root != n) as u128;
    round_pack::<F>(false, (u.exp - shift) / 2, root | sticky, rm, fl)
}

/// Fused `a * b + c` with a single rounding.
pub fn mul_add<F: Format>(a: u64, b: u64, c: u64, rm: Rm, fl: &mut u8) -> u64 {
    let (ia, ib, ic) = (is_inf::<F>(a), is_inf::<F>(b), is_inf::<F>(c));
    let (za, zb, zc) = (is_zero::<F>(a), is_zero::<F>(b), is_zero::<F>(c));
    // inf * 0 is invalid even when the addend is a quiet NaN.
    let inf_times_zero = (ia && zb) || (ib && za);
    if let Some(nan) = propagate_nan::<F>(&[a, b, c], fl) {
        if inf_times_zero {
            *fl |= NV;
        }
        return nan;
    }
    if inf_times_zero {
        return invalid::<F>(fl);
    }
    let sp = sign_of::<F>(a) != sign_of::<F>(b);
    let sc = sign_of::<F>(c);
    if ia || ib {
        if ic && sc != sp {
            return invalid::<F>(fl);
        }
        return infinity::<F>(sp);
    }
    if ic {
        return c;
    }
    if za || zb {
        return if zc { with_sign::<F>(zero_sum_sign(sp, sc, rm)) } else { c };
    }
    let (ua, ub) = (unpack::<F>(a), unpack::<F>(b));
    let product = Unpacked { sign: sp, exp: ua.exp + ub.exp, sig: ua.sig * ub.sig };
    if zc {
        return round_pack::<F>(product.sign, product.exp, product.sig, rm, fl);
    }
    add_unpacked::<F>(product, unpack::<F>(c), rm, fl)
}

/// Total order key for non-NaN values: compares like the real numbers, with -0 == +0.
fn order_key<F: Format>(x: u64) -> i128 {
    let mag = (x & !sign_bit::<F>()) as i128;
    if sign_of::<F>(x) { -mag } else { mag }
}

/// Quiet equality (NV only for signalling NaNs).
pub fn eq<F: Format>(a: u64, b: u64, fl: &mut u8) -> bool {
    if is_nan::<F>(a) || is_nan::<F>(b) {
        if is_snan::<F>(a) || is_snan::<F>(b) {
            *fl |= NV;
        }
        return false;
    }
    order_key::<F>(a) == order_key::<F>(b)
}

/// `a < b`. Signalling (NV for any NaN) unless `quiet`.
pub fn lt<F: Format>(a: u64, b: u64, quiet: bool, fl: &mut u8) -> bool {
    if is_nan::<F>(a) || is_nan::<F>(b) {
        if !quiet || is_snan::<F>(a) || is_snan::<F>(b) {
            *fl |= NV;
        }
        return false;
    }
    order_key::<F>(a) < order_key::<F>(b)
}

/// `a <= b`. Signalling (NV for any NaN) unless `quiet`.
pub fn le<F: Format>(a: u64, b: u64, quiet: bool, fl: &mut u8) -> bool {
    if is_nan::<F>(a) || is_nan::<F>(b) {
        if !quiet || is_snan::<F>(a) || is_snan::<F>(b) {
            *fl |= NV;
        }
        return false;
    }
    order_key::<F>(a) <= order_key::<F>(b)
}

/// Minimum or maximum, with -0 < +0. With `nan_propagating` (Zfa `fminm`/`fmaxm`) any NaN
/// operand gives a NaN; otherwise (`fmin`/`fmax`, IEEE minimumNumber) a single NaN is ignored.
pub fn min_max<F: Format>(a: u64, b: u64, max: bool, nan_propagating: bool, fl: &mut u8) -> u64 {
    let (na, nb) = (is_nan::<F>(a), is_nan::<F>(b));
    if na || nb {
        if is_snan::<F>(a) || is_snan::<F>(b) {
            *fl |= NV;
        }
        return match (na, nb, nan_propagating) {
            (true, false, false) => b,
            (false, true, false) => a,
            _ => canonical_nan::<F>(),
        };
    }
    let (ka, kb) = (order_key::<F>(a), order_key::<F>(b));
    if ka == kb {
        // Equal values; only the zeros' signs can differ.
        return if max { a & b } else { a | b };
    }
    if (ka < kb) != max { a } else { b }
}

/// The RISC-V `fclass` mask.
pub fn classify<F: Format>(x: u64) -> u64 {
    let sign = sign_of::<F>(x);
    let exp_field = (x >> F::FRAC_BITS) & exp_max::<F>();
    let bit = if is_nan::<F>(x) {
        if is_snan::<F>(x) { 8 } else { 9 }
    } else if is_inf::<F>(x) {
        if sign { 0 } else { 7 }
    } else if is_zero::<F>(x) {
        if sign { 3 } else { 4 }
    } else if exp_field == 0 {
        if sign { 2 } else { 5 }
    } else if sign {
        1
    } else {
        6
    };
    1 << bit
}

/// Converts to a `width`-bit (32 or 64) integer. 32-bit results are sign-extended to 64 bits,
/// as RISC-V stores them in registers. Out-of-range inputs and NaNs saturate and raise NV.
pub fn to_int<F: Format>(x: u64, signed: bool, width: u32, rm: Rm, fl: &mut u8) -> u64 {
    let (min, max): (i128, i128) =
        if signed { (-(1 << (width - 1)), (1 << (width - 1)) - 1) } else { (0, (1 << width) - 1) };
    let finish = |v: i128| if width == 32 { v as i32 as i64 as u64 } else { v as u64 };
    if is_nan::<F>(x) {
        *fl |= NV;
        return finish(max);
    }
    let sign = sign_of::<F>(x);
    if is_inf::<F>(x) {
        *fl |= NV;
        return finish(if sign { min } else { max });
    }
    let u = unpack::<F>(x);
    let (mag, inexact) = if u.exp >= 0 {
        if u.exp > 64 { (u128::MAX, false) } else { (u.sig << u.exp, false) }
    } else {
        round_shift(u.sig, -u.exp, sign, rm)
    };
    let value = if sign { -(mag.min(1 << 100) as i128) } else { mag.min(1 << 100) as i128 };
    if value < min || value > max {
        *fl |= NV;
        return finish(if sign { min } else { max });
    }
    if inexact {
        *fl |= NX;
    }
    finish(value)
}

/// Converts an integer (`value` interpreted as signed if `signed`) to format `F`.
pub fn from_int<F: Format>(value: u64, signed: bool, rm: Rm, fl: &mut u8) -> u64 {
    let negative = signed && (value as i64) < 0;
    let mag = if negative { (value as i64).unsigned_abs() } else { value };
    round_pack::<F>(negative, 0, mag as u128, rm, fl)
}

/// Converts between formats.
pub fn convert<From: Format, To: Format>(x: u64, rm: Rm, fl: &mut u8) -> u64 {
    if is_nan::<From>(x) {
        if is_snan::<From>(x) {
            *fl |= NV;
        }
        return canonical_nan::<To>();
    }
    let sign = sign_of::<From>(x);
    if is_inf::<From>(x) {
        return infinity::<To>(sign);
    }
    if is_zero::<From>(x) {
        return with_sign::<To>(sign);
    }
    let u = unpack::<From>(x);
    round_pack::<To>(sign, u.exp, u.sig, rm, fl)
}

/// Rounds to an integral value in the same format. NX is raised only if `exact` is set.
pub fn round_to_int<F: Format>(x: u64, rm: Rm, exact: bool, fl: &mut u8) -> u64 {
    if let Some(nan) = propagate_nan::<F>(&[x], fl) {
        return nan;
    }
    if is_inf::<F>(x) || is_zero::<F>(x) {
        return x;
    }
    let u = unpack::<F>(x);
    if u.exp >= 0 {
        return x;
    }
    let (q, inexact) = round_shift(u.sig, -u.exp, u.sign, rm);
    if inexact && exact {
        *fl |= NX;
    }
    // Integral values are exactly representable, so this does not round again.
    round_pack::<F>(u.sign, 0, q, rm, &mut 0)
}

/// Zfa `fcvtmod.w.d`: truncates to an integer and returns it modulo 2^32, sign-extended.
pub fn to_i32_modular(x: u64, fl: &mut u8) -> u64 {
    if is_nan::<F64>(x) || is_inf::<F64>(x) {
        *fl |= NV;
        return 0;
    }
    let u = unpack::<F64>(x);
    let (mag, inexact) = if u.exp >= 0 {
        // Only the low 32 bits matter for the result; keep enough to detect overflow.
        if u.exp >= 64 { (1u128 << 64, false) } else { (u.sig << u.exp, false) }
    } else {
        round_shift(u.sig, -u.exp, u.sign, Rm::Rtz)
    };
    let in_range = if u.sign { mag <= 1 << 31 } else { mag < 1 << 31 };
    if !in_range {
        *fl |= NV;
    } else if inexact {
        *fl |= NX;
    }
    let low = mag as u32;
    let value = if u.sign { low.wrapping_neg() } else { low };
    value as i32 as i64 as u64
}

/// Zfa `fli` constants, as binary64 values, indexed by the instruction's rs1 field.
/// Entry 1 (the minimum normal) depends on the format and is handled by [`fli`].
const FLI_TABLE: [f64; 32] = [
    -1.0,
    0.0, // minimum positive normal, format dependent
    1.0 / 65536.0,
    1.0 / 32768.0,
    1.0 / 256.0,
    1.0 / 128.0,
    0.0625,
    0.125,
    0.25,
    0.3125,
    0.375,
    0.4375,
    0.5,
    0.625,
    0.75,
    0.875,
    1.0,
    1.25,
    1.5,
    1.75,
    2.0,
    2.5,
    3.0,
    4.0,
    8.0,
    16.0,
    128.0,
    256.0,
    32768.0,
    65536.0,
    f64::INFINITY,
    f64::NAN,
];

/// Zfa `fli.fmt` immediate number `index`.
pub fn fli<F: Format>(index: u32) -> u64 {
    match index & 31 {
        1 => 1 << F::FRAC_BITS,
        31 => canonical_nan::<F>(),
        i => convert::<F64, F>(FLI_TABLE[i as usize].to_bits(), Rm::Rne, &mut 0),
    }
}

#[cfg(test)]
mod tests;
