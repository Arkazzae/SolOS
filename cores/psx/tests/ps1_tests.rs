//! ps1-tests (JaCzekanski), judged against a real console: what a test
//! prints against its psx.log, and what a GPU test leaves in VRAM against
//! its vram.png (as a hash of the picture's colours, cut to the five bits
//! a channel VRAM holds). Tests that do not match yet are left out; the
//! list is in docs/psx.md.

use psx::{Psx, VRAM_WIDTH};

const TESTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/ps1-tests/");

fn load(test: &str) -> Option<Psx> {
    let name = test.rsplit('/').next().unwrap();
    let Ok(exe) = std::fs::read(format!("{TESTS}{test}/{name}.exe")) else {
        eprintln!("skipped: run tests/roms/fetch.sh");
        return None;
    };
    Some(Psx::with_exe(&exe).unwrap())
}

/// What a test prints until it prints "Done." (or in 20 seconds).
fn output(test: &str) -> Option<String> {
    let mut psx = load(test)?;
    for _ in 0..1200 {
        psx.run_frame();
        if psx.tty().windows(5).any(|w| w == b"Done.") {
            break;
        }
    }
    Some(String::from_utf8_lossy(psx.tty()).into_owned())
}

/// Runs each test and fails with all whose output differs from the
/// console's. The console's log has "% " before the lines, the core's the
/// lines PSn00bSDK's ResetGraph prints; neither counts, nor do empty
/// lines.
fn logs(tests: &[&str]) {
    let mut failed = Vec::new();
    for &test in tests {
        let Some(got) = output(test) else { return };
        let expected = std::fs::read_to_string(format!("{TESTS}{test}/psx.log")).unwrap();
        let expected: Vec<&str> = expected
            .lines()
            .map(|l| l.trim_end_matches('\r'))
            .map(|l| l.strip_prefix("% ").unwrap_or(l))
            .filter(|l| !l.is_empty())
            .collect();
        let got: Vec<&str> = got.lines().filter(|l| !l.is_empty() && !l.starts_with("ResetGraph:")).collect();
        if got == expected {
            eprintln!("{test}: passed");
        } else {
            failed.push(format!("{test}:\n{}", got.join("\n")));
        }
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

/// FNV-1a over the colours of the top left of VRAM, the mask bit left out.
fn hash(vram: &[u16], width: usize, height: usize) -> u64 {
    let rows = vram.chunks(VRAM_WIDTH).take(height);
    rows.flat_map(|row| &row[..width])
        .fold(0xcbf2_9ce4_8422_2325, |h, &pixel| (h ^ u64::from(pixel & 0x7fff)).wrapping_mul(0x100_0000_01b3))
}

#[test]
fn cpu() {
    logs(&["cpu/cop", "cpu/code-in-io"]);
}

/// All the GTE's commands and registers, 1150 cases the console passes
/// (its log is of an older version of the test, which printed less).
#[test]
fn gte() {
    let Some(got) = output("gte/test-all") else { return };
    assert!(got.contains("Passed tests: 1150\nFailed tests: 0\n"), "{got}");
}

#[test]
fn gpu_registers() {
    logs(&["gpu/gp0-e1", "gpu/mask-bit"]);
}

/// The VRAM a GPU test leaves after two seconds, all of it or (where the
/// console's had other things in it) the 320 x 240 the test draws in;
/// with the GPU drawing on its own thread and without.
#[test]
fn gpu_vram() {
    let tests = [
        ("clipping", 1024, 512, 0xd284_8b88_ad63_76a5),
        ("quad", 1024, 512, 0x6f3c_6523_7723_6b65),
        ("rectangles", 1024, 512, 0x36ae_789d_0f51_9b54),
        ("texture-flip", 1024, 512, 0x714c_f7b7_ac9b_668f),
        ("texture-overflow", 1024, 512, 0x7096_f4fd_f06e_3ed9),
        ("transparency", 320, 240, 0x42d2_ac76_6c75_3f25),
        ("triangle", 1024, 512, 0xa1c5_58a4_6650_7d56),
        ("uv-interpolation", 1024, 512, 0xb618_25f4_d5bb_a173),
        ("vram-to-vram-overlap", 320, 240, 0x5360_4f3e_4103_163a),
    ];
    let mut failed = Vec::new();
    for (name, width, height, expected) in tests {
        for thread in [true, false] {
            let Some(mut psx) = load(&format!("gpu/{name}")) else { return };
            psx.set_gpu_thread(thread);
            for _ in 0..120 {
                psx.run_frame();
            }
            let got = hash(psx.vram(), width, height);
            if got == expected {
                eprintln!("gpu/{name} (thread {thread}): passed");
            } else {
                failed.push(format!("gpu/{name} (thread {thread}): {got:#018x}"));
            }
        }
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}
