//! Original Sol OS diagnostic cartridge for the Master System and the Game Gear, hand-assembled Z80. No game or
//! firmware assets.
//!
//! It sets the VDP up with an empty background, makes colour 0 red (so the whole picture is red), starts a PSG
//! tone, prints "OK\n" on the SDSC debug console and halts with the frame interrupt on. The interrupt handler
//! counts frames in RAM at $C000 and, while pad 1's left fire button is down, makes colour 0 green. The NMI
//! handler (the Pause button) prints "P". `rom_reporting_v` also prints the V counter the interrupt handler reads
//! on each frame interrupt.

#![allow(dead_code)]

const OUT_CONTROL: u8 = 0xbf;
const OUT_DATA: u8 = 0xbe;

struct Asm(Vec<u8>);

impl Asm {
    fn put(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
    /// LD A,n; OUT (port),A
    fn out(&mut self, port: u8, value: u8) {
        self.put(&[0x3e, value, 0xd3, port]);
    }
    fn vdp_register(&mut self, register: u8, value: u8) {
        self.out(OUT_CONTROL, value);
        self.out(OUT_CONTROL, 0x80 | register);
    }
    /// Sets colour RAM entry 0: one byte on the Master System, a pair of bytes on the Game Gear.
    fn colour_zero(&mut self, gg: bool, sms: u8, gg_word: u16) {
        self.out(OUT_CONTROL, 0x00);
        self.out(OUT_CONTROL, 0xc0);
        if gg {
            self.out(OUT_DATA, gg_word as u8);
            self.out(OUT_DATA, (gg_word >> 8) as u8);
        } else {
            self.out(OUT_DATA, sms);
        }
    }
}

pub const RED_SMS: u8 = 0x03;
pub const GREEN_SMS: u8 = 0x0c;
pub const RED_GG: u16 = 0x000f;
pub const GREEN_GG: u16 = 0x00f0;
pub const RED: u16 = 0xf800;
pub const GREEN: u16 = 0x07e0;

/// The 32 KiB cartridge. `region_code` goes into the header's region nibble (4 SMS export, 6 GG export).
pub fn rom(gg: bool, region_code: u8) -> Vec<u8> {
    build(gg, region_code, false)
}

/// The same cartridge whose frame interrupt handler also prints the V counter it reads, a byte a frame.
pub fn rom_reporting_v(gg: bool, region_code: u8) -> Vec<u8> {
    build(gg, region_code, true)
}

fn build(gg: bool, region_code: u8, report_v: bool) -> Vec<u8> {
    let mut rom = vec![0; 0x8000];

    // $0000: reset.
    rom[0..9].copy_from_slice(&[0xf3, 0xed, 0x56, 0x31, 0xf0, 0xdf, 0xc3, 0x00, 0x01]); // DI; IM 1; LD SP,$DFF0; JP $0100
    // $0038: the VDP's interrupt: JP $0200.
    rom[0x38..0x3b].copy_from_slice(&[0xc3, 0x00, 0x02]);
    // $0066: NMI: print "P", RETN.
    rom[0x66..0x6c].copy_from_slice(&[0x3e, b'P', 0xd3, 0xfd, 0xed, 0x45]);

    let mut main = Asm(Vec::new());
    for (register, value) in [
        (0, 0x04),
        (1, 0x00),
        (2, 0xff),
        (5, 0xff),
        (6, 0xfb),
        (7, 0x00),
        (8, 0),
        (9, 0),
        (10, 0xff),
    ] {
        main.vdp_register(register, value);
    }
    main.colour_zero(gg, RED_SMS, RED_GG);
    // PSG: tone 0, period $0FE, full volume.
    main.out(0x7f, 0x8e);
    main.out(0x7f, 0x0f);
    main.out(0x7f, 0x90);
    for &c in b"OK\n" {
        main.out(0xfd, c);
    }
    // Display on, frame interrupt on.
    main.vdp_register(1, 0x60);
    main.put(&[0xfb, 0x76, 0x18, 0xfd]); // EI; loop: HALT; JR loop
    rom[0x100..0x100 + main.0.len()].copy_from_slice(&main.0);

    let mut pressed = Asm(Vec::new());
    pressed.colour_zero(gg, GREEN_SMS, GREEN_GG);
    let mut released = Asm(Vec::new());
    released.colour_zero(gg, RED_SMS, RED_GG);
    let mut handler = Asm(Vec::new());
    handler.put(&[
        0xf5, // PUSH AF
        0xdb, 0xbf, // IN A,($BF)  acknowledge the interrupt
    ]);
    if report_v {
        handler.put(&[0xdb, 0x7e, 0xd3, 0xfd]); // IN A,($7E); OUT ($FD),A
    }
    handler.put(&[
        0x3a,
        0x00,
        0xc0, // LD A,($C000)
        0x3c, // INC A
        0x32,
        0x00,
        0xc0, // LD ($C000),A
        0xdb,
        0xdc, // IN A,($DC)
        0xe6,
        0x10, // AND $10
        0x20,
        (pressed.0.len() + 2) as u8, // JR NZ,released
    ]);
    handler.put(&pressed.0);
    handler.put(&[0x18, released.0.len() as u8]); // JR done
    handler.put(&released.0);
    handler.put(&[0xf1, 0xfb, 0xc9]); // POP AF; EI; RET
    rom[0x200..0x200 + handler.0.len()].copy_from_slice(&handler.0);

    // The header.
    rom[0x7ff0..0x7ff8].copy_from_slice(b"TMR SEGA");
    rom[0x7fff] = (region_code << 4) | 0x0c;
    rom
}

pub fn sms_rom() -> Vec<u8> {
    rom(false, 4)
}
pub fn gg_rom() -> Vec<u8> {
    rom(true, 6)
}
