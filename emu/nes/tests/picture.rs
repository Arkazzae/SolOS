//! Test ROMs that show their result only in the picture.

use nes::Nes;

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/nes-test-roms/");

/// blargg's nmi_sync demo turns $2001's greyscale on and off with writes
/// timed from the NMI, which draws a line where the black backdrop goes
/// white, between reference lines of sprites from x 80. On the console its
/// left end is at 80 on one frame and 81 on the next (the frames with
/// rendering on are a dot shorter every other time).
#[test]
fn nmi_sync() {
    let Ok(rom) = std::fs::read(format!("{ROMS}nmi_sync/demo_ntsc.nes")) else {
        eprintln!("skipped: run tests/roms/fetch.sh");
        return;
    };
    let mut nes = Nes::new(&rom).unwrap();
    for _ in 0..60 {
        nes.run_frame([0, 0]);
    }
    // Where a line of the picture first turns white.
    let left = |nes: &Nes, y: usize| nes.frame()[y * nes::WIDTH..][..nes::WIDTH].iter().position(|&p| p == 0x30);
    let mut ends = Vec::new();
    for _ in 0..4 {
        nes.run_frame([0, 0]);
        assert_eq!(left(&nes, 119), Some(80), "the reference line");
        ends.push(left(&nes, 121));
    }
    ends.sort();
    ends.dedup();
    assert_eq!(ends, [Some(80), Some(81)], "the drawn line's left end");
}
