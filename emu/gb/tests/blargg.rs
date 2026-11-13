//! blargg's test ROMs. They print their results to the link port (and the
//! screen): the test's name, then "Passed", or "Failed" and what failed.
//! The sound tests report through cartridge RAM instead: A000 is $80 while
//! the test runs, then its result (0 for a pass); $DE $B0 $61 at A001-A003
//! shows the protocol is in use, and the result's text starts at A004.

use gb::{Gb, Model};

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/game-boy-test-roms/blargg/");

/// Runs a test ROM until it reports, for at most the given seconds: whether
/// it passed and its text, or None when the test ROMs have not been fetched.
fn run(path: &str, model: Model, seconds: u32) -> Option<(bool, String)> {
    let Ok(rom) = std::fs::read(format!("{ROMS}{path}")) else {
        eprintln!("skipped: run tests/roms/fetch.sh");
        return None;
    };
    let mut gb = Gb::new(&rom, model).unwrap();
    let serial = |gb: &Gb| String::from_utf8_lossy(gb.serial()).into_owned();
    let memory = |gb: &Gb| (0xa004..0xc000).map(|a| gb.peek(a)).take_while(|&b| b != 0).map(char::from).collect();
    for _ in 0..60 * seconds {
        gb.run_frame(0);
        if [gb.peek(0xa001), gb.peek(0xa002), gb.peek(0xa003)] == [0xde, 0xb0, 0x61] && gb.peek(0xa000) != 0x80 {
            return Some((gb.peek(0xa000) == 0, memory(&gb)));
        }
        let text = serial(&gb);
        if text.contains("Passed") {
            return Some((true, text));
        }
        if text.contains("Failed") {
            // The rest of the report follows.
            for _ in 0..60 {
                gb.run_frame(0);
            }
            return Some((false, serial(&gb)));
        }
    }
    Some((false, format!("no result after {seconds} s: {}{}", serial(&gb), memory(&gb))))
}

/// Runs ROMs in a directory on the DMG and fails with all their failures
/// together.
fn suite(dir: &str, roms: &[&str], seconds: u32) {
    suite_on(Model::Dmg, dir, roms, seconds);
}

fn suite_on(model: Model, dir: &str, roms: &[&str], seconds: u32) {
    let mut failed = Vec::new();
    for rom in roms {
        match run(&format!("{dir}{rom}.gb"), model, seconds) {
            None => return,
            Some((true, _)) => eprintln!("{dir}{rom}: passed"),
            Some((false, text)) => failed.push(format!("{rom}: {}", text.trim())),
        }
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

#[test]
fn cpu_instrs() {
    let roms = [
        "01-special",
        "02-interrupts",
        "03-op sp,hl",
        "04-op r,imm",
        "05-op rp",
        "06-ld r,r",
        "07-jr,jp,call,ret,rst",
        "08-misc instrs",
        "09-op r,r",
        "10-bit ops",
        "11-op a,(hl)",
    ];
    suite("cpu_instrs/individual/", &roms, 30);
}

#[test]
fn instr_timing() {
    suite("instr_timing/", &["instr_timing"], 10);
}

#[test]
fn mem_timing() {
    suite("mem_timing/individual/", &["01-read_timing", "02-write_timing", "03-modify_timing"], 10);
}

#[test]
fn dmg_sound() {
    let roms = [
        "01-registers",
        "02-len ctr",
        "03-trigger",
        "04-sweep",
        "05-sweep details",
        "06-overflow on trigger",
        "07-len sweep period sync",
        "08-len ctr during power",
        "09-wave read while on",
        "10-wave trigger while on",
        "11-regs after power",
        "12-wave write while on",
    ];
    suite("dmg_sound/rom_singles/", &roms, 30);
}

#[test]
fn cgb_sound() {
    let roms = [
        "01-registers",
        "02-len ctr",
        "03-trigger",
        "04-sweep",
        "05-sweep details",
        "06-overflow on trigger",
        "07-len sweep period sync",
        "08-len ctr during power",
        "09-wave read while on",
        "10-wave trigger while on",
        "11-regs after power",
        "12-wave",
    ];
    suite_on(Model::Cgb, "cgb_sound/rom_singles/", &roms, 30);
}
