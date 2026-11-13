use super::*;
#[path = "../tests/support/mod.rs"]
mod support;

#[test]
fn work_ram_mirrors_bus_grant_and_z80_reset() {
    let mut m = Md::new(&support::rom()).unwrap();
    m.hw.write16(0xff0100, 0x1234);
    assert_eq!(m.hw.read16(0xe00100), 0x1234);
    m.hw.write8(0x01ff0101, 0x56);
    assert_eq!(m.hw.read16(0xff0100), 0x1256);
    assert_eq!(m.hw.read8(0xa10001), 0x80);
    m.hw.write8(0xa00000, 0x76);
    m.hw.write16(0xa11200, 0x0100);
    assert_eq!(m.hw.read16(0xa11100), 0x0100);
    assert_eq!(m.hw.read8(0xa00000), 0xff);
    m.hw.write16(0xa11100, 0x0100);
    assert_eq!(m.hw.read16(0xa11100), 0);
    assert_eq!(m.hw.read8(0xa00000), 0x76);
}

#[test]
fn cartridge_dma_reads_rom_and_mirrored_ram_and_stalls_cpu() {
    let mut m = Md::new(&support::rom()).unwrap();
    m.hw.vdp.write_control(0x8114); // mode5 + DMA, display off
    m.hw.vdp.write_control(0x8f02);
    m.hw.vdp.write_control(0x9302);
    m.hw.vdp.write_control(0x9400);
    m.hw.vdp.write_control(0x9500);
    m.hw.vdp.write_control(0x9601); // source $200
    m.hw.vdp.write_control(0x9700);
    m.hw.write32(0xc00004, 0x40000080);
    assert!(m.hw.dma_wait > 0);
    m.hw.write32(0xc00004, 0x00000000);
    assert_eq!(m.hw.read16(0xc00000), 0x46fc);
    assert_eq!(m.hw.read16(0xc00000), 0x2700);
    m.hw.write16(0xe00000, 0xabcd);
    for word in [0x9301, 0x9400, 0x9500, 0x9600, 0x977f] {
        m.hw.vdp.write_control(word);
    }
    m.hw.write32(0xc00004, 0x40040080);
    m.hw.write32(0xc00004, 0x00040000);
    assert_eq!(m.hw.read16(0xc00000), 0xabcd);
    let state = m.save_state();
    let mut resumed = Md::new(&support::rom()).unwrap();
    resumed.load_state(&state).unwrap();
    m.run_frame([0, 0]);
    resumed.run_frame([0, 0]);
    assert_eq!(m.save_state(), resumed.save_state());
}

#[test]
fn sram_byte_lane_write_protect_and_bank_registers() {
    let mut rom = support::rom();
    rom[0x1b0..0x1b4].copy_from_slice(b"RA\xf8\x20");
    rom[0x1b4..0x1b8].copy_from_slice(&0x200001u32.to_be_bytes());
    rom[0x1b8..0x1bc].copy_from_slice(&0x2000ffu32.to_be_bytes());
    let mut m = Md::new(&rom).unwrap();
    m.hw.write16(0x200000, 0x1234);
    assert_eq!(m.save_data().unwrap()[0], 0x34);
    assert_eq!(m.hw.read16(0x200000), 0xff34);
    m.hw.write8(0xa130f1, 3);
    m.hw.write8(0x200001, 0x99);
    assert_eq!(m.save_data().unwrap()[0], 0x34);
    m.hw.write8(0xa130f1, 0);
    assert_eq!(m.hw.read8(0x200001), 0xff);
    let mut rom = support::rom();
    rom.resize(0x480000, 0);
    rom[0x80000] = 0x11;
    rom[0x400000] = 0x88;
    let mut m = Md::new(&rom).unwrap();
    assert_eq!(m.hw.read8(0x80000), 0x11);
    m.hw.write8(0xa130f3, 8);
    assert_eq!(m.hw.read8(0x80000), 0x88);
    assert_eq!(m.hw.read16(0x100), 0x5345);
}

#[test]
fn corrupted_device_fields_are_rejected_transactionally() {
    let mut m = Md::new(&support::rom()).unwrap();
    m.run_frame([0, 0]);
    let expected = m.save_state();
    let mut bad = m.clone();
    bad.hw.zbank = 1;
    assert_eq!(m.load_state(&bad.save_state()), Err(Error::BadState));
    assert_eq!(m.save_state(), expected);
    let mut bad = m.clone();
    bad.hw.dma_wait = u32::MAX;
    assert_eq!(m.load_state(&bad.save_state()), Err(Error::BadState));
    assert_eq!(m.save_state(), expected);
}
