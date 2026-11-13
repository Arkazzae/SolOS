//! Gekkio's Mooneye test suite. A test ends by running LD B,B; it passed if
//! B, C, D, E, H and L then hold 3, 5, 8, 13, 21 and 34.

use gb::{Gb, Model};

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/game-boy-test-roms/mooneye-test-suite/");

/// Runs a test ROM to its end (for at most two emulated minutes): whether
/// it passed, or None when the test ROMs have not been fetched.
fn run(path: &str, model: Model) -> Option<bool> {
    let Ok(rom) = std::fs::read(format!("{ROMS}{path}")) else {
        eprintln!("skipped: run tests/roms/fetch.sh");
        return None;
    };
    let mut gb = Gb::new(&rom, model).unwrap();
    for _ in 0..120 * 60 {
        gb.run_frame(0);
        if gb.breakpoint() {
            let r = gb.registers();
            return Some([r.b, r.c, r.d, r.e, r.h, r.l] == [3, 5, 8, 13, 21, 34]);
        }
    }
    Some(false)
}

/// Runs ROMs in a directory on the DMG and fails with all their failures
/// together.
fn suite(dir: &str, roms: &[&str]) {
    suite_on(Model::Dmg, dir, roms);
}

fn suite_on(model: Model, dir: &str, roms: &[&str]) {
    let mut failed = Vec::new();
    for rom in roms {
        match run(&format!("{dir}{rom}.gb"), model) {
            None => return,
            Some(true) => eprintln!("{dir}{rom}: passed"),
            Some(false) => failed.push(*rom),
        }
    }
    assert!(failed.is_empty(), "{dir}: {}", failed.join(", "));
}

#[test]
fn cpu() {
    let roms = [
        "add_sp_e_timing",
        "call_cc_timing",
        "call_cc_timing2",
        "call_timing",
        "call_timing2",
        "di_timing-GS",
        "ei_sequence",
        "ei_timing",
        "halt_ime0_ei",
        "halt_ime0_nointr_timing",
        "halt_ime1_timing",
        "halt_ime1_timing2-GS",
        "if_ie_registers",
        "intr_timing",
        "jp_cc_timing",
        "jp_timing",
        "ld_hl_sp_e_timing",
        "pop_timing",
        "push_timing",
        "rapid_di_ei",
        "ret_cc_timing",
        "ret_timing",
        "reti_intr_timing",
        "reti_timing",
        "rst_timing",
        "instr/daa",
        "interrupts/ie_push",
        "bits/reg_f",
    ];
    suite("acceptance/", &roms);
}

#[test]
fn boot() {
    let roms = ["boot_div-dmgABCmgb", "boot_hwio-dmgABCmgb", "boot_regs-dmgABC", "serial/boot_sclk_align-dmgABCmgb"];
    suite("acceptance/", &roms);
}

#[test]
fn memory() {
    let roms = [
        "bits/mem_oam",
        "bits/unused_hwio-GS",
        "oam_dma/basic",
        "oam_dma/reg_read",
        "oam_dma/sources-GS",
        "oam_dma_restart",
        "oam_dma_start",
        "oam_dma_timing",
    ];
    suite("acceptance/", &roms);
}

#[test]
fn timer() {
    let roms = [
        "div_timing",
        "timer/div_write",
        "timer/rapid_toggle",
        "timer/tim00",
        "timer/tim00_div_trigger",
        "timer/tim01",
        "timer/tim01_div_trigger",
        "timer/tim10",
        "timer/tim10_div_trigger",
        "timer/tim11",
        "timer/tim11_div_trigger",
        "timer/tima_reload",
        "timer/tima_write_reloading",
        "timer/tma_write_reloading",
    ];
    suite("acceptance/", &roms);
}

#[test]
fn ppu() {
    let roms = [
        "hblank_ly_scx_timing-GS",
        "intr_1_2_timing-GS",
        "intr_2_0_timing",
        "intr_2_mode0_timing",
        "intr_2_mode0_timing_sprites",
        "intr_2_mode3_timing",
        "intr_2_oam_ok_timing",
        "lcdon_timing-GS",
        "lcdon_write_timing-GS",
        "stat_irq_blocking",
        "stat_lyc_onoff",
        "vblank_stat_intr-GS",
    ];
    suite("acceptance/ppu/", &roms);
}

#[test]
fn mbc1() {
    let roms = [
        "bits_bank1",
        "bits_bank2",
        "bits_mode",
        "bits_ramg",
        "ram_64kb",
        "ram_256kb",
        "rom_512kb",
        "rom_1Mb",
        "rom_2Mb",
        "rom_4Mb",
        "rom_8Mb",
        "rom_16Mb",
    ];
    suite("emulator-only/mbc1/", &roms);
}

#[test]
fn mbc2() {
    let roms = ["bits_ramg", "bits_romb", "bits_unused", "ram", "rom_512kb", "rom_1Mb", "rom_2Mb"];
    suite("emulator-only/mbc2/", &roms);
}

#[test]
fn mbc5() {
    let roms = ["rom_512kb", "rom_1Mb", "rom_2Mb", "rom_4Mb", "rom_8Mb", "rom_16Mb", "rom_32Mb", "rom_64Mb"];
    suite("emulator-only/mbc5/", &roms);
}

/// The acceptance tests that pass on every model, on the Color: the parts
/// the two share.
#[test]
fn cgb_shared() {
    let roms = [
        "add_sp_e_timing",
        "call_cc_timing",
        "call_cc_timing2",
        "call_timing",
        "call_timing2",
        "ei_sequence",
        "ei_timing",
        "halt_ime0_ei",
        "halt_ime0_nointr_timing",
        "halt_ime1_timing",
        "if_ie_registers",
        "intr_timing",
        "jp_cc_timing",
        "jp_timing",
        "ld_hl_sp_e_timing",
        "pop_timing",
        "push_timing",
        "rapid_di_ei",
        "ret_cc_timing",
        "ret_timing",
        "reti_intr_timing",
        "reti_timing",
        "rst_timing",
        "instr/daa",
        "interrupts/ie_push",
        "bits/mem_oam",
        "bits/reg_f",
        "oam_dma/basic",
        "oam_dma/reg_read",
        "oam_dma_restart",
        "oam_dma_start",
        "oam_dma_timing",
        "div_timing",
        "timer/div_write",
        "timer/rapid_toggle",
        "timer/tim00",
        "timer/tim00_div_trigger",
        "timer/tim01",
        "timer/tim01_div_trigger",
        "timer/tim10",
        "timer/tim10_div_trigger",
        "timer/tim11",
        "timer/tim11_div_trigger",
        "timer/tima_reload",
        "timer/tima_write_reloading",
        "timer/tma_write_reloading",
        "ppu/intr_2_0_timing",
        "ppu/intr_2_mode0_timing",
        "ppu/intr_2_mode0_timing_sprites",
        "ppu/intr_2_mode3_timing",
        "ppu/intr_2_oam_ok_timing",
        "ppu/stat_irq_blocking",
        "ppu/stat_lyc_onoff",
    ];
    suite_on(Model::Cgb, "acceptance/", &roms);
}

/// SameSuite (Sameboy's tests, which end as Mooneye's do): the Color's VRAM
/// DMA and palettes.
#[test]
fn cgb_same_suite() {
    let roms = ["dma/gbc_dma_cont", "dma/gdma_addr_mask", "dma/hdma_lcd_off", "dma/hdma_mode0", "ppu/blocking_bgpi_increase"];
    suite_on(Model::Cgb, "../same-suite/", &roms);
}
