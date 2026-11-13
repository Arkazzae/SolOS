//! blargg's test ROMs. They report through memory: $6000 is $80 while a
//! test runs, $81 when it wants the reset button pressed, then its result
//! (0 for a pass); $DE $B0 $61 at $6001-$6003 shows the protocol is in use,
//! and the result's text starts at $6004. His older tests (2005) only show
//! the result, and keep its code at $F8: 1 for a pass, or the number of the
//! check that failed, explained in each suite's readme.

use nes::Nes;

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/nes-test-roms/");

/// Runs a test ROM to its result: its code and text, or None when the test
/// ROMs have not been fetched.
fn run(path: &str) -> Option<(u8, String)> {
    let Ok(rom) = std::fs::read(format!("{ROMS}{path}")) else {
        eprintln!("skipped: run tests/roms/fetch.sh");
        return None;
    };
    let mut nes = Nes::new(&rom).unwrap();
    let text = |nes: &Nes| (0x6004..0x7000).map(|a| nes.peek(a)).take_while(|&b| b != 0).map(char::from).collect();
    let mut reset_at = None;
    // A minute of emulated time.
    for frame in 0..60 * 60 {
        nes.run_frame([0, 0]);
        if [nes.peek(0x6001), nes.peek(0x6002), nes.peek(0x6003)] != [0xde, 0xb0, 0x61] {
            continue;
        }
        match nes.peek(0x6000) {
            0x80 => {}
            // Reset, a tenth of a second after it asks.
            0x81 => {
                if frame >= *reset_at.get_or_insert(frame + 6) {
                    nes.reset();
                    reset_at = None;
                }
            }
            code => return Some((code, text(&nes))),
        }
    }
    panic!("{path}: no result after a minute: {}", text(&nes));
}

/// Runs one of the older tests for some seconds, then reads its result.
fn run_old(path: &str, seconds: u32) -> Option<(u8, String)> {
    let Ok(rom) = std::fs::read(format!("{ROMS}{path}")) else {
        eprintln!("skipped: run tests/roms/fetch.sh");
        return None;
    };
    let mut nes = Nes::new(&rom).unwrap();
    for _ in 0..60 * seconds {
        nes.run_frame([0, 0]);
    }
    match nes.peek(0xf8) {
        1 => Some((0, "passed".into())),
        code => Some((code, format!("check {code} failed"))),
    }
}

/// Runs ROMs in a directory and fails with all their failures together.
fn suite(dir: &str, roms: &[&str]) {
    suite_with(dir, roms, run);
}

/// The same for the older tests, given the time they take.
fn suite_old(dir: &str, roms: &[&str], seconds: u32) {
    suite_with(dir, roms, |path| run_old(path, seconds));
}

fn suite_with(dir: &str, roms: &[&str], run: impl Fn(&str) -> Option<(u8, String)>) {
    let mut failed = Vec::new();
    for rom in roms {
        match run(&format!("{dir}/{rom}.nes")) {
            None => return,
            Some((0, text)) => eprintln!("{dir}/{rom}: {}", text.trim().replace('\n', " ")),
            Some((code, text)) => failed.push(format!("{rom} ({code}): {}", text.trim())),
        }
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

#[test]
fn instr_test_v5() {
    suite(
        "instr_test-v5/rom_singles",
        &[
            "01-basics", "02-implied", "03-immediate", "04-zero_page", "05-zp_xy", "06-absolute", "07-abs_xy",
            "08-ind_x", "09-ind_y", "10-branches", "11-stack", "12-jmp_jsr", "13-rts", "14-rti", "15-brk",
            "16-special",
        ],
    );
}

#[test]
fn ppu_vbl_nmi() {
    suite(
        "ppu_vbl_nmi/rom_singles",
        &[
            "01-vbl_basics", "02-vbl_set_time", "03-vbl_clear_time", "04-nmi_control", "05-nmi_timing",
            "06-suppression", "07-nmi_on_timing", "08-nmi_off_timing", "09-even_odd_frames", "10-even_odd_timing",
        ],
    );
}

#[test]
fn ppu_misc() {
    suite("oam_read", &["oam_read"]);
    suite("ppu_open_bus", &["ppu_open_bus"]);
}

#[test]
fn cpu_misc() {
    suite("instr_misc/rom_singles", &["01-abs_x_wrap", "02-branch_wrap", "03-dummy_reads"]);
    suite("cpu_dummy_writes", &["cpu_dummy_writes_oam", "cpu_dummy_writes_ppumem"]);
    suite("cpu_exec_space", &["test_cpu_exec_space_ppuio"]);
    suite("cpu_reset", &["ram_after_reset", "registers"]);
}

#[test]
fn sprite_hit() {
    suite_old(
        "sprite_hit_tests_2005.10.05",
        &[
            "01.basics", "02.alignment", "03.corners", "04.flip", "05.left_clip", "06.right_edge", "07.screen_bottom",
            "08.double_height", "09.timing_basics", "10.timing_order", "11.edge_timing",
        ],
        15,
    );
}

#[test]
fn sprite_overflow() {
    suite_old("sprite_overflow_tests", &["1.Basics", "2.Details", "3.Timing", "4.Obscure", "5.Emulator"], 15);
}

#[test]
fn vbl_nmi_timing() {
    suite_old(
        "vbl_nmi_timing",
        &[
            "1.frame_basics", "2.vbl_timing", "3.even_odd_frames", "4.vbl_clear_timing", "5.nmi_suppression",
            "6.nmi_disable", "7.nmi_timing",
        ],
        20,
    );
}

#[test]
fn ppu_2005() {
    suite_old(
        "blargg_ppu_tests_2005.09.15b",
        &["palette_ram", "sprite_ram", "vbl_clear_time", "vram_access"],
        10,
    );
}

#[test]
fn mappers() {
    // The full suites are MMC1 boards.
    suite("instr_test-v5", &["all_instrs"]);
    suite("ppu_vbl_nmi", &["ppu_vbl_nmi"]);
    // MMC3 as most boards have it (revision B); 6-MMC3_alt is the other.
    suite("mmc3_test_2/rom_singles", &["1-clocking", "2-details", "3-A12_clocking", "4-scanline_timing", "5-MMC3"]);
}

#[test]
fn apu() {
    suite(
        "apu_test/rom_singles",
        &["1-len_ctr", "2-len_table", "3-irq_flag", "4-jitter", "5-len_timing", "6-irq_flag_timing", "7-dmc_basics",
            "8-dmc_rates"],
    );
    suite_old(
        "blargg_apu_2005.07.30",
        &["01.len_ctr", "02.len_table", "03.irq_flag", "04.clock_jitter", "05.len_timing_mode0",
            "06.len_timing_mode1", "07.irq_flag_timing", "08.irq_timing", "09.reset_timing", "10.len_halt_timing",
            "11.len_reload_timing"],
        10,
    );
}

#[test]
fn cpu_timing() {
    suite("instr_timing/rom_singles", &["1-instr_timing", "2-branch_timing"]);
    suite("instr_misc/rom_singles", &["04-dummy_reads_apu"]);
    suite("cpu_exec_space", &["test_cpu_exec_space_apu"]);
    suite("cpu_interrupts_v2/rom_singles", &["1-cli_latency", "2-nmi_and_brk", "3-nmi_and_irq", "4-irq_and_dma",
        "5-branch_delays_irq"]);
}
