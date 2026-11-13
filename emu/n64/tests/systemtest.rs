//! Lemmy's n64-systemtest (MIT; version 2.1.0 as RustyN64 builds it, 3721
//! cases): the CPU, COP0 and the TLB, the FPU, the RSP and the RCP's
//! registers, memories and DMAs, each case checked against what consoles
//! do. It boots through its own IPL3 and prints, through the IS-Viewer,
//! each test as it starts ("Running ..."), each case that fails ("Test
//! '...' failed: ...", or "Test '...' with '...' failed" for a test run on
//! several arguments) and, at the end, how many failed of how many.
//!
//! The cases that fail must be exactly the ones listed here, each with
//! why: a new failure fails this test, and so does a case that newly
//! passes, which then comes off the list. It runs twice, on the reference
//! (the interpreter, the scalar RSP, the reference RDP) and with every
//! fast path the build has, which must fail the same cases.

use std::collections::BTreeSet;

use n64::N64;

const ROM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/n64-systemtest/n64-systemtest.z64");

/// The core has no instruction or data cache: CACHE does nothing and
/// every access goes to memory, so no line is ever stale, dirty or
/// invalid (docs/n64.md, Reference core).
const NO_CACHES: &str = "not modelled on purpose: there are no caches, CACHE does nothing";

/// Status.RE runs user mode in the other byte order; the tests' user code
/// is written for it, so without it that code runs as other instructions.
const NO_REVERSE_ENDIAN: &str = "open: Status.RE (user mode in the other byte order) is not emulated";

/// The cases that fail, as `failures` names them, in the order the suite
/// runs them, and why.
const FAILING: &[(&str, &str)] = &[
    ("CACHE icache index invalidate clears valid (TagLo via load tag)", NO_CACHES),
    ("CACHE icache hit invalidate clears valid when line hits", NO_CACHES),
    ("ICACHE: uncached patch does not affect fetched line until invalidate", NO_CACHES),
    ("CACHE icache hit writeback (0x18) pushes line to RAM", NO_CACHES),
    ("DCACHE: cached LW stale after uncached SW until invalidate", NO_CACHES),
    ("DCACHE: hit writeback pushes dirty line to RAM", NO_CACHES),
    ("CACHE dcache index writeback invalidate clears valid (TagLo via load tag)", NO_CACHES),
    ("CACHE dcache create dirty exclusive (0x0D) then store/writeback", NO_CACHES),
    ("CACHE dcache hit writeback invalidate (0x15)", NO_CACHES),
    ("RE user mode execute from uncached and cached code pages", NO_REVERSE_ENDIAN),
    ("RE user mode loads: cached/uncached + COP1", NO_REVERSE_ENDIAN),
    ("RE user mode stores: cached/uncached + COP1", NO_REVERSE_ENDIAN),
    ("RE unmapped access keeps raw BadVAddr (no xor tweak)", NO_REVERSE_ENDIAN),
    ("RE user mode partial loads matrix (all offsets)", NO_REVERSE_ENDIAN),
    ("RE user mode partial stores matrix (all offsets)", NO_REVERSE_ENDIAN),
];

/// How many cases the suite has, and at least how many tests it starts
/// (it starts 950; one that hangs or stops early starts fewer).
const CASES: usize = 3721;
const TESTS: usize = 900;

/// Runs the suite to its last line and returns what it printed. The
/// suite takes about 200 fields.
fn run(rom: &[u8], fast: bool) -> String {
    let mut n64 = N64::new(rom).unwrap();
    n64.set_jit(fast);
    n64.set_rsp_acceleration(fast);
    n64.set_rdp_acceleration(fast);
    let end = b"% success rate)";
    for _ in 0..20_000 {
        n64.run_frame(0);
        if n64.isviewer().windows(end.len()).any(|w| w == end) {
            return String::from_utf8_lossy(n64.isviewer()).into_owned();
        }
    }
    let text = String::from_utf8_lossy(n64.isviewer()).into_owned();
    let last: Vec<&str> = text.lines().rev().take(5).collect();
    panic!("n64-systemtest did not finish in 20000 fields; it printed last:\n{}", last.join("\n"));
}

/// The cases that failed, each as the test's name and, for a test run on
/// several arguments, " with " and the arguments, on one line; and for
/// each, the line the suite printed about it.
fn failures(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for (at, _) in text.match_indices("Test '") {
        if at > 0 && text.as_bytes()[at - 1] != b'\n' {
            continue;
        }
        let rest = &text[at + "Test '".len()..];
        let line = rest.lines().next().unwrap_or_default();
        let ends = ["' failed", "' with '", "' with unknown arguments"];
        let Some(name_end) = ends.iter().filter_map(|e| rest.find(e)).min() else { continue };
        let (name, after) = rest.split_at(name_end);
        let case = if let Some(args) = after.strip_prefix("' with '") {
            // The arguments, printed as Rust prints a tuple, can span lines.
            let args = &args[..args.find("' failed").unwrap_or(args.len())];
            format!("{name} with {}", args.split_whitespace().collect::<Vec<_>>().join(" "))
        } else if after.starts_with("' with unknown arguments") {
            format!("{name} with unknown arguments")
        } else {
            name.to_string()
        };
        found.push((case, format!("Test '{line}")));
    }
    found
}

/// The counts on the suite's last line: "Failed N of M tests".
fn counts(text: &str) -> (usize, usize) {
    let line = text.lines().find(|l| l.contains("% success rate)")).unwrap();
    let words: Vec<&str> = line[line.find("Failed ").unwrap()..].split_whitespace().collect();
    (words[1].parse().unwrap(), words[3].parse().unwrap())
}

fn check(text: &str, run: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let (failed, cases) = counts(text);
    let started = text.lines().filter(|l| l.starts_with("Running ")).count();
    if cases != CASES {
        problems.push(format!("{run}: the suite has {cases} cases, not {CASES}: not the ROM fetch.sh fetches"));
    }
    if started < TESTS {
        problems.push(format!("{run}: only {started} tests started"));
    }
    let found = failures(text);
    if found.len() != failed {
        problems.push(format!("{run}: the suite counts {failed} failures but {} were read", found.len()));
    }
    let known: BTreeSet<&str> = FAILING.iter().map(|&(name, _)| name).collect();
    let failing: BTreeSet<&str> = found.iter().map(|(case, _)| case.as_str()).collect();
    for (case, line) in &found {
        if !known.contains(case.as_str()) {
            problems.push(format!("{run}: newly fails: {line}"));
        }
    }
    for &(name, why) in FAILING {
        if !failing.contains(name) {
            problems.push(format!("{run}: newly passes (remove it from the list): {name} ({why})"));
        }
    }
    eprintln!("{run}: {started} tests started, {failed} of {cases} cases failed");
    problems
}

#[test]
fn systemtest() {
    let Ok(rom) = std::fs::read(ROM) else {
        eprintln!("skipped: run tests/roms/fetch.sh");
        return;
    };
    let mut problems = check(&run(&rom, false), "reference");
    problems.extend(check(&run(&rom, true), "fast paths"));
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}
