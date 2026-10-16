use super::*;

/// xorshift64*, biased towards interesting exponents and significands.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn f64(&mut self) -> u64 {
        let r = self.next();
        let exp = match r % 8 {
            0 => 0,                           // zero / subnormal
            1 => 0x7ff,                       // inf / NaN
            2 => 1 + (r >> 8) % 4,            // tiny normals
            3 => 0x7fe - (r >> 8) % 4,        // huge
            _ => 0x3ff - 60 + (r >> 8) % 120, // ordinary
        };
        let frac = match (r >> 20) % 4 {
            0 => 0,
            1 => self.next() & 0xf,
            2 => (1 << 52) - 1 - (self.next() & 0xf),
            _ => self.next(),
        } & ((1 << 52) - 1);
        ((r >> 63) << 63) | (exp << 52) | frac
    }

    fn f32(&mut self) -> u64 {
        let r = self.next();
        let exp = match r % 8 {
            0 => 0,
            1 => 0xff,
            2 => 1 + (r >> 8) % 4,
            3 => 0xfe - (r >> 8) % 4,
            _ => 127 - 30 + (r >> 8) % 60,
        };
        let frac = match (r >> 20) % 4 {
            0 => 0,
            1 => self.next() & 0xf,
            2 => (1 << 23) - 1 - (self.next() & 0xf),
            _ => self.next(),
        } & ((1 << 23) - 1);
        ((r >> 63) << 31) | (exp << 23) | frac
    }
}

fn same64(ours: u64, host: f64) -> bool {
    if host.is_nan() { ours == canonical_nan::<F64>() } else { ours == host.to_bits() }
}

fn same32(ours: u64, host: f32) -> bool {
    if host.is_nan() { ours == canonical_nan::<F32>() } else { ours == host.to_bits() as u64 }
}

#[test]
fn f64_matches_host_round_to_nearest() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..200_000 {
        let (a, b, c) = (rng.f64(), rng.f64(), rng.f64());
        let (x, y, z) = (f64::from_bits(a), f64::from_bits(b), f64::from_bits(c));
        let fl = &mut 0;
        assert!(same64(add::<F64>(a, b, Rm::Rne, fl), x + y), "add {a:#x} {b:#x}");
        assert!(same64(sub::<F64>(a, b, Rm::Rne, fl), x - y), "sub {a:#x} {b:#x}");
        assert!(same64(mul::<F64>(a, b, Rm::Rne, fl), x * y), "mul {a:#x} {b:#x}");
        assert!(same64(div::<F64>(a, b, Rm::Rne, fl), x / y), "div {a:#x} {b:#x}");
        assert!(same64(sqrt::<F64>(a, Rm::Rne, fl), x.sqrt()), "sqrt {a:#x}");
        assert!(same64(mul_add::<F64>(a, b, c, Rm::Rne, fl), x.mul_add(y, z)), "fma {a:#x} {b:#x} {c:#x}");
        assert!(same32(convert::<F64, F32>(a, Rm::Rne, fl), x as f32), "f64->f32 {a:#x}");
    }
}

#[test]
fn f32_matches_host_round_to_nearest() {
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    for _ in 0..200_000 {
        let (a, b, c) = (rng.f32(), rng.f32(), rng.f32());
        let (x, y, z) = (f32::from_bits(a as u32), f32::from_bits(b as u32), f32::from_bits(c as u32));
        let fl = &mut 0;
        assert!(same32(add::<F32>(a, b, Rm::Rne, fl), x + y), "add {a:#x} {b:#x}");
        assert!(same32(mul::<F32>(a, b, Rm::Rne, fl), x * y), "mul {a:#x} {b:#x}");
        assert!(same32(div::<F32>(a, b, Rm::Rne, fl), x / y), "div {a:#x} {b:#x}");
        assert!(same32(sqrt::<F32>(a, Rm::Rne, fl), x.sqrt()), "sqrt {a:#x}");
        assert!(same32(mul_add::<F32>(a, b, c, Rm::Rne, fl), x.mul_add(y, z)), "fma {a:#x} {b:#x} {c:#x}");
        assert!(same64(convert::<F32, F64>(a, Rm::Rne, fl), x as f64), "f32->f64 {a:#x}");
    }
}

fn f(x: f64) -> u64 {
    x.to_bits()
}

#[test]
fn flags_and_rounding_modes() {
    let mut fl = 0;
    assert_eq!(div::<F64>(f(1.0), f(3.0), Rm::Rtz, &mut fl), f(1.0 / 3.0));
    assert_eq!(fl, NX);
    fl = 0;
    assert_eq!(div::<F64>(f(1.0), f(3.0), Rm::Rup, &mut fl), f(1.0 / 3.0) + 1);
    fl = 0;
    assert_eq!(div::<F64>(f(-1.0), f(0.0), Rm::Rne, &mut fl), f(f64::NEG_INFINITY));
    assert_eq!(fl, DZ);
    fl = 0;
    assert_eq!(mul::<F64>(f(f64::MAX), f(2.0), Rm::Rtz, &mut fl), f(f64::MAX));
    assert_eq!(fl, OF | NX);
    fl = 0;
    assert_eq!(mul::<F64>(f(f64::MIN_POSITIVE), f(0.5), Rm::Rne, &mut fl), f(f64::MIN_POSITIVE / 2.0));
    assert_eq!(fl, 0, "exact subnormal result: no underflow");
    fl = 0;
    mul::<F64>(f(f64::MIN_POSITIVE), f(0.3), Rm::Rne, &mut fl);
    assert_eq!(fl, UF | NX);
    fl = 0;
    assert_eq!(sqrt::<F64>(f(-1.0), Rm::Rne, &mut fl), canonical_nan::<F64>());
    assert_eq!(fl, NV);
    fl = 0;
    assert_eq!(sub::<F64>(f(1.0), f(1.0), Rm::Rdn, &mut fl), f(-0.0));
    assert_eq!(fl, 0);
    // Ties: 2.5 -> 2 (even) / 3 (max magnitude).
    assert_eq!(round_to_int::<F64>(f(2.5), Rm::Rne, true, &mut fl), f(2.0));
    assert_eq!(round_to_int::<F64>(f(2.5), Rm::Rmm, true, &mut fl), f(3.0));
    assert_eq!(round_to_int::<F64>(f(-0.25), Rm::Rne, false, &mut 0), f(-0.0));
}

#[test]
fn integer_conversions() {
    let mut fl = 0;
    assert_eq!(to_int::<F64>(f(-1.5), true, 32, Rm::Rne, &mut fl), (-2i64) as u64);
    assert_eq!(fl, NX);
    fl = 0;
    assert_eq!(to_int::<F64>(f(3e9), true, 32, Rm::Rne, &mut fl), i32::MAX as u64);
    assert_eq!(fl, NV);
    fl = 0;
    assert_eq!(to_int::<F64>(f(3e9), false, 32, Rm::Rne, &mut fl), 3_000_000_000u32 as i32 as i64 as u64);
    assert_eq!(fl, 0, "fcvt.wu.d results are sign-extended");
    fl = 0;
    assert_eq!(to_int::<F64>(f(-0.5), false, 64, Rm::Rtz, &mut fl), 0);
    assert_eq!(fl, NX);
    fl = 0;
    assert_eq!(to_int::<F64>(canonical_nan::<F64>(), true, 64, Rm::Rne, &mut fl), i64::MAX as u64);
    assert_eq!(fl, NV);
    fl = 0;
    assert_eq!(from_int::<F32>(u64::MAX, false, Rm::Rne, &mut fl), (u64::MAX as f32).to_bits() as u64);
    assert_eq!(fl, NX);
    assert_eq!(from_int::<F64>((-7i64) as u64, true, Rm::Rne, &mut 0), f(-7.0));
}

#[test]
fn min_max_and_compare() {
    let qnan = canonical_nan::<F64>();
    let snan = 0x7ff0_0000_0000_0001;
    let mut fl = 0;
    assert_eq!(min_max::<F64>(qnan, f(1.0), false, false, &mut fl), f(1.0));
    assert_eq!(fl, 0);
    assert_eq!(min_max::<F64>(snan, f(1.0), false, false, &mut fl), f(1.0));
    assert_eq!(fl, NV);
    assert_eq!(min_max::<F64>(f(-0.0), f(0.0), false, false, &mut 0), f(-0.0));
    assert_eq!(min_max::<F64>(f(-0.0), f(0.0), true, false, &mut 0), f(0.0));
    assert_eq!(min_max::<F64>(qnan, f(1.0), true, true, &mut 0), qnan);
    fl = 0;
    assert!(!lt::<F64>(qnan, f(1.0), false, &mut fl));
    assert_eq!(fl, NV, "flt is signalling");
    fl = 0;
    assert!(!lt::<F64>(qnan, f(1.0), true, &mut fl));
    assert_eq!(fl, 0, "fltq is quiet");
    assert!(eq::<F64>(f(0.0), f(-0.0), &mut 0));
    assert!(le::<F64>(f(-0.0), f(0.0), false, &mut 0));
    assert_eq!(classify::<F64>(f(-0.0)), 1 << 3);
    assert_eq!(classify::<F64>(snan), 1 << 8);
}

#[test]
fn half_precision_and_fli() {
    assert_eq!(convert::<F32, F16>(1.0f32.to_bits() as u64, Rm::Rne, &mut 0), 0x3c00);
    assert_eq!(convert::<F16, F64>(0x7bff, Rm::Rne, &mut 0), f(65504.0));
    let mut fl = 0;
    assert_eq!(convert::<F64, F16>(f(1e6), Rm::Rne, &mut fl), 0x7c00);
    assert_eq!(fl, OF | NX);
    assert_eq!(fli::<F64>(0), f(-1.0));
    assert_eq!(fli::<F64>(1), f(f64::MIN_POSITIVE));
    assert_eq!(fli::<F32>(1), f32::MIN_POSITIVE.to_bits() as u64);
    assert_eq!(fli::<F32>(29), 65536f32.to_bits() as u64);
    assert_eq!(fli::<F64>(30), f(f64::INFINITY));
}

#[test]
fn modular_conversion() {
    let mut fl = 0;
    assert_eq!(to_i32_modular(f(4294967297.5), &mut fl), 1);
    assert_eq!(fl, NV);
    fl = 0;
    assert_eq!(to_i32_modular(f(-3.75), &mut fl), (-3i64) as u64);
    assert_eq!(fl, NX);
}

#[test]
fn round_to_odd() {
    // 1 + 2^-30 is not representable in binary32: truncate and force the lowest bit.
    let x = f(1.0 + 2f64.powi(-30));
    let mut fl = 0;
    assert_eq!(convert::<F64, F32>(x, Rm::Rod, &mut fl), 0x3f80_0001);
    assert_eq!(fl, NX);
    assert_eq!(convert::<F64, F32>(f(1.5), Rm::Rod, &mut 0), 1.5f32.to_bits() as u64, "exact stays even");
    fl = 0;
    assert_eq!(convert::<F64, F32>(f(1e300), Rm::Rod, &mut fl), f32::MAX.to_bits() as u64);
    assert_eq!(fl, OF | NX);
}
