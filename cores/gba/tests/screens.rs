//! ROMs judged by the picture they show after some frames: jsmolka's PPU
//! tests and the demos of Tonc, which between them use every mode,
//! affine backgrounds and sprites, windows, blending, mosaic, raster
//! effects from interrupts and DMA, the timers and the BIOS calls. No
//! pictures from a real GBA come with them: the hashes are of this core's
//! pictures, each looked at against what the demo's source says it draws,
//! so the test guards against changes.

use gba::{Gba, HEIGHT, WIDTH};
use std::io::Write;

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/");

/// FNV-1a over the colours.
fn hash(frame: &[u16]) -> u64 {
    frame.iter().fold(0xcbf2_9ce4_8422_2325, |h, &pixel| (h ^ u64::from(pixel)).wrapping_mul(0x100_0000_01b3))
}

/// Runs each ROM for its frames, pressing its buttons first (each for a
/// tenth of a second, half a second apart), and fails with all whose
/// picture differs, saving those pictures (as PPM) next to the test
/// binaries.
fn suite(roms: &[(&str, &[u16], u32, u64)]) {
    let mut failed = Vec::new();
    for &(path, buttons, frames, expected) in roms {
        let Ok(rom) = std::fs::read(format!("{ROMS}{path}")) else {
            eprintln!("skipped: run tests/roms/fetch.sh");
            return;
        };
        let mut gba = Gba::new(&rom).unwrap();
        for &button in buttons {
            for frame in 0..30 {
                gba.run_frame(if frame < 6 { button } else { 0 });
            }
        }
        for _ in 0..frames {
            gba.run_frame(0);
        }
        let got = hash(gba.frame());
        if got == expected {
            eprintln!("{path}: passed");
            continue;
        }
        let out = format!("{}/{}.ppm", env!("CARGO_TARGET_TMPDIR"), path.replace('/', "_"));
        let mut file = std::fs::File::create(&out).unwrap();
        write!(file, "P6\n{WIDTH} {HEIGHT}\n255\n").unwrap();
        let channel = |c: u16| (c << 3 | c >> 2) as u8;
        for &rgb in gba.frame() {
            file.write_all(&[channel(rgb & 31), channel(rgb >> 5 & 31), channel(rgb >> 10 & 31)]).unwrap();
        }
        failed.push(format!("{path}: {got:#018x} (picture in {out})"));
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

#[test]
fn jsmolka_ppu() {
    suite(&[
        ("gba-tests/ppu/hello.gba", &[], 30, 0x18f1_4f77_fc98_b882),
        ("gba-tests/ppu/shades.gba", &[], 30, 0x11f9_b3df_d964_5b25),
        ("gba-tests/ppu/stripes.gba", &[], 30, 0x982a_8c04_ca48_c225),
    ]);
}

#[test]
fn tonc() {
    use gba::START;
    suite(&[
        // Tiled backgrounds, scrolling, a big map.
        ("tonc/bin/bigmap.gba", &[], 240, 0x985e_e32c_9ca5_866f),
        ("tonc/bin/brin_demo.gba", &[], 240, 0x35b2_b04d_9ab9_e0a5),
        ("tonc/bin/cbb_demo.gba", &[], 240, 0x3db4_614c_7eb9_b083),
        ("tonc/bin/sbb_reg.gba", &[], 240, 0xc61d_4303_cbc1_d272),
        // Bitmap modes and page flipping.
        ("tonc/bin/bm_modes.gba", &[], 240, 0xfb59_88a5_3720_27a6),
        ("tonc/bin/m3_demo.gba", &[], 240, 0x3913_9d5b_f7d4_353b),
        ("tonc/bin/txt_bm.gba", &[], 240, 0x34a8_dd89_5dc4_e07b),
        // Affine backgrounds and sprites, and mode 7 from interrupts.
        ("tonc/bin/sbb_aff.gba", &[], 240, 0x774f_41a7_c41b_0fd5),
        ("tonc/bin/obj_aff.gba", &[], 240, 0xbba8_adc9_6b42_0498),
        ("tonc/bin/m7_demo.gba", &[], 240, 0x932a_e730_c111_1e0f),
        ("tonc/bin/m7_ex.gba", &[], 240, 0xd52a_751d_1424_a412),
        // Sprites, priorities, windows, blending, mosaic.
        ("tonc/bin/obj_demo.gba", &[], 240, 0x938d_18e5_6e1d_06cc),
        ("tonc/bin/oacombo.gba", &[], 240, 0x8d79_460e_a977_2f47),
        ("tonc/bin/prio_demo.gba", &[], 240, 0x8120_b8da_048a_8364),
        ("tonc/bin/win_demo.gba", &[], 240, 0x8272_fc33_541b_5579),
        ("tonc/bin/bld_demo.gba", &[], 240, 0x903c_a241_4afe_42bc),
        ("tonc/bin/mos_demo.gba", &[], 240, 0xf1b3_a80b_82eb_7aef),
        // A window shaped line by line by HBlank DMA, and interrupts.
        ("tonc/bin/dma_demo.gba", &[], 240, 0x38b0_bab0_9842_02ee),
        ("tonc/bin/irq_demo.gba", &[], 240, 0xd939_d13c_7fd2_5bae),
        // BIOS calls: sine, square root, arctangent and division drawn,
        // the octants of ArcTan2, VBlankIntrWait.
        ("tonc/bin/swi_demo.gba", &[], 240, 0x8a81_64da_fc12_a376),
        ("tonc/bin/octtest.gba", &[], 240, 0xbd6b_9b5c_08bf_65bc),
        ("tonc/bin/swi_vsync.gba", &[], 240, 0xc855_f1a8_5481_0a11),
        // Timers counting seconds once started, cascaded.
        ("tonc/bin/tmr_demo.gba", &[0, START], 240, 0x75d2_727d_f2ad_4d11),
        // Text: tiles, sprites, and the timings of copies measured by a
        // timer.
        ("tonc/bin/txt_obj.gba", &[], 240, 0xb157_2e2f_3026_4a76),
        ("tonc/bin/txt_se1.gba", &[], 240, 0xa475_4bfd_393d_af54),
        ("tonc/bin/txt_se2.gba", &[], 240, 0x863c_c98f_8cb5_f576),
    ]);
}
