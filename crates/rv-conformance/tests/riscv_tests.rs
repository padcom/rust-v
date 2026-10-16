//! Runs the official riscv-tests ISA suite (built by `scripts/build-riscv-tests.sh`).
//!
//! Every test runs twice: interpreted, and with the JIT compiling every block on first use.
//! Ignored by default because it needs the suite to be built first:
//! `cargo test -p rv-conformance --release -- --ignored`
//! Set `RISCV_TESTS_FILTER=<substring>` to run a subset.

use std::path::PathBuf;
use std::sync::Arc;

use rv_core::{Bus, Hart, HartConfig, HartShared, HostClock};

const RAM_BASE: u64 = 0x8000_0000;
const RAM_SIZE: u64 = 32 << 20;
const MAX_INSNS: u64 = 20_000_000;
const SUITES: &[&str] = &[
    "rv64ui",
    "rv64um",
    "rv64ua",
    "rv64uc",
    "rv64uf",
    "rv64ud",
    "rv64uzba",
    "rv64uzbb",
    "rv64uzbs",
    "rv64uzicond",
    "rv64mi",
    "rv64si",
    "rv64mzicbo",
    "rv64ssvnapot",
];

/// Outcome of a single test binary.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Pass,
    /// The test reported failure of the given sub-test number.
    Fail(u64),
    Timeout {
        pc: u64,
    },
}

fn tests_dir() -> PathBuf {
    match std::env::var_os("RISCV_TESTS_DIR") {
        Some(dir) => dir.into(),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../third_party/riscv-tests/isa"),
    }
}

fn run_test(path: &std::path::Path, jit: bool) -> (Outcome, String) {
    run_elf(path, jit, MAX_INSNS)
}

fn run_elf(path: &std::path::Path, jit: bool, max_insns: u64) -> (Outcome, String) {
    let bytes = std::fs::read(path).unwrap();
    let elf = rv_elf::Elf::parse(&bytes).unwrap();
    let tohost = elf.symbol("tohost").unwrap().expect("test has no tohost symbol");

    let bus = Arc::new(Bus::new(RAM_BASE, RAM_SIZE));
    for seg in elf.segments().unwrap() {
        bus.load_image(seg.paddr, seg.data);
    }
    let mut hart = Hart::new(
        HartConfig::default(),
        bus.clone(),
        Arc::new(HartShared::default()),
        Arc::new(HostClock::new(10_000_000)),
    );
    hart.reset(elf.entry);
    if jit {
        hart.compiler = Some(Box::new(rv_jit::Compiler::new().unwrap()));
        hart.jit_threshold = 1;
    }

    let mut console = String::new();
    let mut executed = 0;
    while executed < max_insns {
        hart.run(1000);
        executed += 1000;
        let value = bus.read(tohost, 8).unwrap();
        if value == 0 {
            continue;
        }
        if value >> 48 == 0x0101 {
            // HTIF console putchar.
            console.push(value as u8 as char);
            bus.write(tohost, 8, 0);
            continue;
        }
        let code = value >> 1;
        return (if code == 0 { Outcome::Pass } else { Outcome::Fail(code) }, console);
    }
    (Outcome::Timeout { pc: hart.state.pc }, console)
}

#[test]
#[ignore = "needs scripts/build-riscv-tests.sh"]
fn riscv_tests_interpreter() {
    run_suite(false);
}

#[test]
#[ignore = "needs scripts/build-riscv-tests.sh"]
fn riscv_tests_jit() {
    run_suite(true);
}

fn run_suite(jit: bool) {
    let dir = tests_dir();
    let filter = std::env::var("RISCV_TESTS_FILTER").unwrap_or_default();
    let mut tests: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e} (run scripts/build-riscv-tests.sh)", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_str().unwrap();
            p.extension().is_none()
                && SUITES
                    .iter()
                    .any(|s| name.starts_with(&format!("{s}-p-")) || name.starts_with(&format!("{s}-v-")))
                && name.contains(&filter)
        })
        .collect();
    tests.sort();
    assert!(!tests.is_empty(), "no tests found in {}", dir.display());

    let mut failures = Vec::new();
    for path in &tests {
        let name = path.file_name().unwrap().to_str().unwrap();
        let (outcome, console) = run_test(path, jit);
        if outcome != Outcome::Pass {
            failures.push(format!("{name}: {outcome:?} {}", console.trim()));
        }
    }
    let mode = if jit { "JIT" } else { "interpreter" };
    println!("{mode}: {} of {} riscv-tests passed", tests.len() - failures.len(), tests.len());
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
}

/// Extensions the generated vector tests cover that rust-v does not implement (vector crypto,
/// Zvbc and bfloat16), as test-name prefixes.
const VECTOR_UNSUPPORTED: &[&str] = &[
    "vaes",
    "vsm3",
    "vsm4",
    "vsha2",
    "vghsh",
    "vgmul",
    "vclmul",
    "vbf",
    "vfwmaccbf16",
    "vfncvtbf16",
    "vfwcvtbf16",
];

fn vector_tests_dir() -> PathBuf {
    match std::env::var_os("VECTOR_TESTS_DIR") {
        Some(dir) => dir.into(),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../third_party/vector-tests/riscv-vector-tests/out/v128x64machine/bin/stage2"),
    }
}

/// The chipsalliance riscv-vector-tests suite (VLEN=128), with expected values from Spike.
/// Build it with `scripts/build-vector-tests.sh`.
fn run_vector_suite(jit: bool) {
    let dir = vector_tests_dir();
    let filter = std::env::var("RISCV_TESTS_FILTER").unwrap_or_default();
    let mut tests: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e} (run scripts/build-vector-tests.sh)", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_str().unwrap();
            !VECTOR_UNSUPPORTED.iter().any(|u| name.starts_with(u)) && name.contains(&filter)
        })
        .collect();
    tests.sort();
    assert!(!tests.is_empty(), "no tests found in {}", dir.display());

    let failures: Vec<String> = std::thread::scope(|scope| {
        let chunks: Vec<_> = tests.chunks(tests.len().div_ceil(8)).collect();
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .filter_map(|path| {
                            let name = path.file_name().unwrap().to_str().unwrap();
                            let (outcome, _) = run_elf(path, jit, 200_000_000);
                            (outcome != Outcome::Pass).then(|| format!("{name}: {outcome:?}"))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    let mode = if jit { "JIT" } else { "interpreter" };
    println!("{mode}: {} of {} vector tests passed", tests.len() - failures.len(), tests.len());
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
}

#[test]
#[ignore = "needs scripts/build-vector-tests.sh"]
fn vector_tests_interpreter() {
    run_vector_suite(false);
}

#[test]
#[ignore = "needs scripts/build-vector-tests.sh"]
fn vector_tests_jit() {
    run_vector_suite(true);
}

/// Debug aid: `VECTOR_DUMP=<elf> cargo test ... -- --ignored dump_result` runs one test and
/// prints the first 64 bytes of its `resultdata`.
#[test]
#[ignore = "debugging aid"]
fn dump_result() {
    let Some(path) = std::env::var_os("VECTOR_DUMP") else { return };
    let bytes = std::fs::read(&path).unwrap();
    let elf = rv_elf::Elf::parse(&bytes).unwrap();
    let tohost = elf.symbol("tohost").unwrap().unwrap();
    let result = elf.symbol("resultdata").unwrap().unwrap();
    let bus = Arc::new(Bus::new(RAM_BASE, RAM_SIZE));
    for seg in elf.segments().unwrap() {
        bus.load_image(seg.paddr, seg.data);
    }
    let mut hart = Hart::new(
        HartConfig::default(),
        bus.clone(),
        Arc::new(HartShared::default()),
        Arc::new(HostClock::new(10_000_000)),
    );
    hart.reset(elf.entry);
    for _ in 0..200_000 {
        hart.run(1000);
        if bus.read(tohost, 8).unwrap() != 0 {
            break;
        }
    }
    let words: Vec<String> =
        (0..8).map(|i| format!("{:016x}", bus.read(result + 8 * i, 8).unwrap())).collect();
    println!("tohost={:#x} resultdata: {}", bus.read(tohost, 8).unwrap(), words.join(" "));
    let a0 = hart.state.x[10];
    let near: Vec<String> =
        (0..6).map(|i| format!("{:016x}", bus.read(a0 - 24 + 8 * i, 8).unwrap_or(0))).collect();
    println!("a0={a0:#x} t0={:#x} around a0-24: {}", hart.state.x[5], near.join(" "));
}
