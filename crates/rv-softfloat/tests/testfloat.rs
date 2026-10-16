//! Checks every operation against Berkeley TestFloat vectors generated with RISC-V-specialized
//! SoftFloat (`scripts/build-testfloat-vectors.sh`). Results and flags must match exactly.
//!
//! Ignored by default because it needs the vectors:
//! `cargo test --release -p rv-softfloat -- --ignored`

use std::path::PathBuf;

use rv_softfloat::*;

fn rounding(name: &str) -> Rm {
    match name {
        "near_even" => Rm::Rne,
        "minMag" => Rm::Rtz,
        "min" => Rm::Rdn,
        "max" => Rm::Rup,
        "near_maxMag" => Rm::Rmm,
        _ => panic!("unknown rounding mode {name}"),
    }
}

/// Evaluates `func` on `inputs`, returning (result, flags). `None` if the function is not one
/// RISC-V needs.
fn eval(func: &str, rm: Rm, inputs: &[u64]) -> Option<(u64, u8)> {
    let mut fl = 0;
    let fl_ref = &mut fl;
    let a = inputs[0];
    let b = inputs.get(1).copied().unwrap_or(0);
    let c = inputs.get(2).copied().unwrap_or(0);
    macro_rules! by_format {
        ($fmt:ty, $op:expr) => {{
            let op: &str = $op;
            match op {
                "add" => add::<$fmt>(a, b, rm, fl_ref),
                "sub" => sub::<$fmt>(a, b, rm, fl_ref),
                "mul" => mul::<$fmt>(a, b, rm, fl_ref),
                "div" => div::<$fmt>(a, b, rm, fl_ref),
                "sqrt" => sqrt::<$fmt>(a, rm, fl_ref),
                "mulAdd" => mul_add::<$fmt>(a, b, c, rm, fl_ref),
                "eq" => eq::<$fmt>(a, b, fl_ref) as u64,
                "lt" => lt::<$fmt>(a, b, false, fl_ref) as u64,
                "le" => le::<$fmt>(a, b, false, fl_ref) as u64,
                "lt_quiet" => lt::<$fmt>(a, b, true, fl_ref) as u64,
                "le_quiet" => le::<$fmt>(a, b, true, fl_ref) as u64,
                "roundToInt" => round_to_int::<$fmt>(a, rm, false, fl_ref),
                "roundToInt_exact" => round_to_int::<$fmt>(a, rm, true, fl_ref),
                "to_i32" => to_int::<$fmt>(a, true, 32, rm, fl_ref) & 0xffff_ffff,
                "to_ui32" => to_int::<$fmt>(a, false, 32, rm, fl_ref) & 0xffff_ffff,
                "to_i64" => to_int::<$fmt>(a, true, 64, rm, fl_ref),
                "to_ui64" => to_int::<$fmt>(a, false, 64, rm, fl_ref),
                _ => return None,
            }
        }};
    }
    let result = match func {
        f if f.starts_with("f32_") && !f.starts_with("f32_to_f") => by_format!(F32, &f[4..]),
        f if f.starts_with("f64_") && !f.starts_with("f64_to_f") => by_format!(F64, &f[4..]),
        "f32_to_f64" => convert::<F32, F64>(a, rm, fl_ref),
        "f64_to_f32" => convert::<F64, F32>(a, rm, fl_ref),
        "f32_to_f16" => convert::<F32, F16>(a, rm, fl_ref),
        "f64_to_f16" => convert::<F64, F16>(a, rm, fl_ref),
        "f16_to_f32" => convert::<F16, F32>(a, rm, fl_ref),
        "f16_to_f64" => convert::<F16, F64>(a, rm, fl_ref),
        f => {
            // Integer to float: i32/ui32/i64/ui64 _to_ f32/f64.
            let (int, fmt) = f.split_once("_to_")?;
            let value = match int {
                "i32" => a as u32 as i32 as i64 as u64,
                "ui32" => a as u32 as u64,
                "i64" | "ui64" => a,
                _ => return None,
            };
            let signed = int.starts_with('i');
            match fmt {
                "f32" => from_int::<F32>(value, signed, rm, fl_ref),
                "f64" => from_int::<F64>(value, signed, rm, fl_ref),
                _ => return None,
            }
        }
    };
    Some((result, fl))
}

#[test]
#[ignore = "needs scripts/build-testfloat-vectors.sh"]
fn testfloat_vectors() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../third_party/testfloat/vectors");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e} (run scripts/build-testfloat-vectors.sh)", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();

    let (mut checked, mut skipped_files) = (0u64, Vec::new());
    let mut failures = Vec::new();
    for path in &files {
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let (func, rm) = match stem.split_once('-') {
            Some((f, r)) => (f, rounding(r)),
            None => (stem, Rm::Rne),
        };
        let text = std::fs::read_to_string(path).unwrap();
        let mut file_failures = 0;
        for line in text.lines() {
            let fields: Vec<u64> =
                line.split_whitespace().map(|t| u64::from_str_radix(t, 16).unwrap()).collect();
            let (inputs, expected, expected_flags) =
                (&fields[..fields.len() - 2], fields[fields.len() - 2], fields[fields.len() - 1] as u8);
            let Some((got, got_flags)) = eval(func, rm, inputs) else {
                skipped_files.push(stem.to_string());
                break;
            };
            checked += 1;
            if got != expected || got_flags != expected_flags {
                file_failures += 1;
                if file_failures <= 3 {
                    failures.push(format!(
                        "{stem}: {line} -> got {got:X} {got_flags:02X}, expected {expected:X} {expected_flags:02X}"
                    ));
                }
            }
        }
        if file_failures > 3 {
            failures.push(format!("{stem}: ... {file_failures} failures in total"));
        }
    }
    println!("checked {checked} cases from {} files; skipped {:?}", files.len(), skipped_files);
    assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n"));
}
