//! Test ROMs of the CPU, the memory map and the BIOS. jsmolka's run their
//! tests one after another and end in a loop (`b .`) with the number of
//! the first that failed in r12, or 0; FuzzARM runs 10000 random
//! instructions against results taken from a GBA and ends in the same
//! loop only when all match; ARMWrestler draws a page of results per
//! group, "BAD" in red for each that fails.

use gba::Gba;

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/");

fn rom(path: &str) -> Option<Vec<u8>> {
    let rom = std::fs::read(format!("{ROMS}{path}")).ok();
    if rom.is_none() {
        eprintln!("skipped: run tests/roms/fetch.sh");
    }
    rom
}

/// Whether the CPU has come to `b .`, in ARM state.
fn done(gba: &Gba) -> bool {
    let pc = gba.registers().r[15];
    (0..4).map(|i| u32::from(gba.peek(pc + i)) << (8 * i)).sum::<u32>() == 0xeaff_fffe
}

/// Runs each ROM for up to the given frames; fails with those that did not
/// end in the loop, or ended with a test number in r12.
fn suite(roms: &[&str], frames: u32) {
    let mut failed = Vec::new();
    for path in roms {
        let Some(data) = rom(path) else { return };
        let mut gba = Gba::new(&data).unwrap();
        for _ in 0..frames {
            gba.run_frame(0);
            if done(&gba) {
                break;
            }
        }
        let r12 = gba.registers().r[12];
        if !done(&gba) {
            failed.push(format!("{path}: did not finish, at {:08x}", gba.registers().r[15]));
        } else if r12 != 0 {
            failed.push(format!("{path}: test {r12} failed"));
        } else {
            eprintln!("{path}: passed");
        }
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

#[test]
fn jsmolka() {
    suite(
        &[
            "gba-tests/arm/arm.gba",
            "gba-tests/thumb/thumb.gba",
            "gba-tests/memory/memory.gba",
            "gba-tests/bios/bios.gba",
            "gba-tests/nes/nes.gba",
            "gba-tests/unsafe/unsafe.gba",
        ],
        60,
    );
}

#[test]
fn fuzzarm() {
    suite(
        &[
            "fuzzarm/ARM_DataProcessing.gba",
            "fuzzarm/ARM_Any.gba",
            "fuzzarm/THUMB_DataProcessing.gba",
            "fuzzarm/THUMB_Any.gba",
            "fuzzarm/FuzzARM.gba",
        ],
        1500,
    );
}

/// ARMWrestler's menu has six groups, each a few pages long: START opens
/// the group picked with DOWN and turns its pages. Every page is checked
/// for red, the colour it writes "BAD" in.
#[test]
fn armwrestler() {
    const BAD: u16 = 0x0018;
    const OK: u16 = 0x0300;
    let Some(data) = rom("armwrestler/armwrestler-gba-fixed.gba") else { return };
    let mut failed = Vec::new();
    for group in 0..6 {
        let mut gba = Gba::new(&data).unwrap();
        let press = |gba: &mut Gba, button: u16, frames: u32| {
            for frame in 0..frames {
                gba.run_frame(if frame < 6 { button } else { 0 });
            }
        };
        press(&mut gba, 0, 60);
        for _ in 0..group {
            press(&mut gba, gba::DOWN, 20);
        }
        let mut oks = 0;
        for page in 0..5 {
            press(&mut gba, gba::START, 60);
            if gba.frame().contains(&BAD) {
                failed.push(format!("group {group}, page {page}"));
            }
            oks += gba.frame().iter().filter(|&&p| p == OK).count();
        }
        assert!(oks > 0, "group {group} showed no results");
    }
    assert!(failed.is_empty(), "BAD on: {}", failed.join(", "));
}
