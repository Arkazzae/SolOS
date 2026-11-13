//! Work RAM and cartridge RAM as a whole (what achievements read): a program of its own writes into the
//! banks of an MBC5 cartridge with RAM and into the Color's work RAM banks, then switches them off or away.

use gb::{Gb, Model};

/// 32 KB of ROM for an MBC5 with RAM and a battery (32 KB of it), for the Color too, whose program
/// writes: A000 in RAM bank 0 and A000 in RAM bank 2, C000, D000 in work RAM bank 1 and D000 in bank 3;
/// then it switches the work RAM bank to 5, turns the cartridge RAM off and waits.
fn rom() -> Vec<u8> {
    let mut rom = vec![0; 0x8000];
    rom[0x100..0x104].copy_from_slice(&[0x00, 0xc3, 0x50, 0x01]); // nop; jp 0150
    rom[0x143] = 0x80;
    rom[0x147] = 0x1b;
    rom[0x149] = 3;
    let program: &[u8] = &[
        0x3e, 0x0a, 0xea, 0x00, 0x00, // ld a,0A; ld (0000),a: the RAM on
        0x3e, 0x11, 0xea, 0x00, 0xa0, // ld a,11; ld (A000),a
        0x3e, 0x02, 0xea, 0x00, 0x40, // ld a,2; ld (4000),a: RAM bank 2
        0x3e, 0x22, 0xea, 0x00, 0xa0, // ld a,22; ld (A000),a
        0x3e, 0x33, 0xea, 0x00, 0xc0, // ld a,33; ld (C000),a
        0x3e, 0x44, 0xea, 0x00, 0xd0, // ld a,44; ld (D000),a: work RAM bank 1
        0x3e, 0x03, 0xea, 0x70, 0xff, // ld a,3; ld (FF70),a: work RAM bank 3
        0x3e, 0x55, 0xea, 0x00, 0xd0, // ld a,55; ld (D000),a
        0x3e, 0x05, 0xea, 0x70, 0xff, // ld a,5; ld (FF70),a: work RAM bank 5
        0x3e, 0x00, 0xea, 0x00, 0x00, // ld a,0; ld (0000),a: the RAM off
        0x18, 0xfe, // jr -2
    ];
    rom[0x150..0x150 + program.len()].copy_from_slice(program);
    rom
}

#[test]
fn memory_as_a_whole() {
    let mut gb = Gb::new(&rom(), Model::Cgb).unwrap();
    gb.run_frame(0);
    let (work, cart) = (gb.work_ram(), gb.cart_ram());
    assert_eq!((work.len(), cart.len()), (0x8000, 0x8000));
    // Bank 0 of work RAM at C000, the banks the game wrote at D000 in their places.
    assert_eq!((work[0], work[0x1000], work[0x3000]), (0x33, 0x44, 0x55));
    assert_eq!((cart[0], cart[0x4000]), (0x11, 0x22));
    // The CPU's view is of whichever bank is switched in, and of nothing while the cartridge RAM is off.
    assert_eq!((gb.peek(0xc000), gb.peek(0xd000)), (0x33, 0));
    assert_eq!(gb.peek(0xa000), 0xff);

    // The original has 8 KB of work RAM, the Color's banks do not exist.
    let mut dmg = Gb::new(&rom(), Model::Dmg).unwrap();
    dmg.run_frame(0);
    assert_eq!(dmg.work_ram().len(), 0x2000);
    assert_eq!((dmg.work_ram()[0], dmg.work_ram()[0x1000]), (0x33, 0x55));
    assert_eq!(dmg.cart_ram()[0x4000], 0x22);

    // No RAM on the cartridge: nothing.
    let mut plain = rom();
    plain[0x147] = 0;
    plain[0x149] = 0;
    assert!(Gb::new(&plain, Model::Cgb).unwrap().cart_ram().is_empty());
}
