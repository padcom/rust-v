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
const SUITES: &[&str] = &["rv64ui", "rv64um", "rv64ua", "rv64uc", "rv64uf", "rv64ud", "rv64mi", "rv64si"];

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
    while executed < MAX_INSNS {
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
