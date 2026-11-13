//! Test ROMs judged by the picture they end on, compared with one taken
//! from a real Game Boy (or an emulator that passes) by a hash of its
//! shades, or on the Color of its RGB555 colours. The hashes are of the
//! reference PNGs in the test ROM collection.

use gb::{Gb, HEIGHT, Model, WIDTH};
use std::io::Write;

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/game-boy-test-roms/");

/// FNV-1a over the shades or colours.
fn hash(frame: &[u16]) -> u64 {
    frame.iter().fold(0xcbf2_9ce4_8422_2325, |h, &pixel| (h ^ u64::from(pixel)).wrapping_mul(0x100_0000_01b3))
}

/// Runs each ROM on the DMG for its seconds and fails with all whose
/// picture differs, saving those pictures (as PGM or PPM) next to the test
/// binaries.
fn suite(roms: &[(&str, u32, u64)]) {
    suite_on(Model::Dmg, roms);
}

fn suite_on(model: Model, roms: &[(&str, u32, u64)]) {
    let roms: Vec<_> = roms.iter().map(|&(path, seconds, expected)| (path, &[][..], seconds, expected)).collect();
    suite_pressing(model, &roms);
}

/// The same, pressing buttons first: each for a twelfth of a second, half
/// a second apart.
fn suite_pressing(model: Model, roms: &[(&str, &[u8], u32, u64)]) {
    let mut failed = Vec::new();
    for &(path, buttons, seconds, expected) in roms {
        let Ok(rom) = std::fs::read(format!("{ROMS}{path}")) else {
            eprintln!("skipped: run tests/roms/fetch.sh");
            return;
        };
        let mut gb = Gb::new(&rom, model).unwrap();
        for &button in buttons {
            for frame in 0..30 {
                gb.run_frame(if frame < 5 { button } else { 0 });
            }
        }
        for _ in 0..60 * seconds {
            gb.run_frame(0);
        }
        if hash(gb.frame()) == expected {
            eprintln!("{path}: passed");
            continue;
        }
        let out = format!("{}/{}.pnm", env!("CARGO_TARGET_TMPDIR"), path.replace('/', "_"));
        let mut file = std::fs::File::create(&out).unwrap();
        if gb.color() {
            write!(file, "P6\n{WIDTH} {HEIGHT}\n255\n").unwrap();
            for &rgb in gb.frame() {
                // Five bits a channel, widened as the reference pictures are.
                let channel = |c: u16| (c << 3 | c >> 2) as u8;
                file.write_all(&[channel(rgb & 31), channel(rgb >> 5 & 31), channel(rgb >> 10 & 31)]).unwrap();
            }
        } else {
            write!(file, "P5\n{WIDTH} {HEIGHT}\n255\n").unwrap();
            file.write_all(&gb.frame().map(|shade| [0xff, 0xaa, 0x55, 0x00][usize::from(shade)])).unwrap();
        }
        failed.push(format!("{path} (picture in {out})"));
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

#[test]
fn acid2() {
    suite(&[("dmg-acid2/dmg-acid2.gb", 1, 0xf272_a8ff_e3db_4c16)]);
}

#[test]
fn blargg() {
    suite(&[
        ("blargg/cpu_instrs/cpu_instrs.gb", 55, 0xb041_ee23_90ac_be1e),
        ("blargg/halt_bug.gb", 2, 0xed0d_43f0_501a_9e36),
        ("blargg/instr_timing/instr_timing.gb", 1, 0xef5e_88f0_8b19_8a44),
        ("blargg/mem_timing/mem_timing.gb", 3, 0x01be_4704_9dcf_cc1f),
        ("blargg/mem_timing-2/mem_timing.gb", 4, 0x208a_abc7_bf85_822d),
    ]);
}

#[test]
fn dmg_sound() {
    suite(&[("blargg/dmg_sound/dmg_sound.gb", 36, 0xe9bc_0cbd_9c8b_7e30)]);
}

#[test]
fn others() {
    suite(&[
        ("mbc3-tester/mbc3-tester.gb", 2, 0x9854_d81b_8cb3_2645),
        ("little-things-gb/firstwhite.gb", 1, 0xeca4_7f65_4990_2b25),
        ("scribbltests/lycscx/lycscx.gb", 1, 0x1a39_c3f2_274e_f325),
        ("scribbltests/lycscy/lycscy.gb", 1, 0xeca4_7f65_4990_2b25),
        ("scribbltests/palettely/palettely.gb", 1, 0x8cd9_9a8b_6c49_a425),
        ("scribbltests/scxly/scxly.gb", 1, 0x9660_d073_9ccd_0325),
        ("scribbltests/statcount/statcount-auto.gb", 5, 0x65a7_0ef7_783a_b5e0),
    ]);
}

#[test]
fn mbc3_clock() {
    use gb::{A, DOWN};
    let rom = "rtc3test/rtc3test.gb";
    suite_pressing(Model::Dmg, &[
        (rom, &[0, A], 14, 0x80b4_df81_2ac1_0ec9),
        (rom, &[0, DOWN, A], 9, 0x86cc_16a0_0812_0438),
        (rom, &[0, DOWN, DOWN, A], 27, 0x84de_72bb_6aa9_c915),
    ]);
}

#[test]
fn cgb() {
    suite_on(Model::Cgb, &[
        ("cgb-acid2/cgb-acid2.gbc", 1, 0x50b6_c6d1_5ae0_c9bc),
        ("blargg/cgb_sound/cgb_sound.gb", 37, 0xdfd8_eac0_c7b7_e6bd),
        ("blargg/interrupt_time/interrupt_time.gb", 2, 0x8a45_7773_dfc9_cac9),
    ]);
}
