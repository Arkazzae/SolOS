//! Original Sol OS diagnostic cartridge. No game or firmware assets.
pub fn rom() -> Vec<u8> {
    let mut rom = vec![0; 0x4000];
    rom[0..4].copy_from_slice(&0x00fffff0u32.to_be_bytes());
    rom[4..8].copy_from_slice(&0x200u32.to_be_bytes());
    for at in (8..0x100).step_by(4) {
        rom[at..at + 4].copy_from_slice(&0x700u32.to_be_bytes());
    }
    rom[0x78..0x7c].copy_from_slice(&0x600u32.to_be_bytes());
    rom[0x100..0x110].copy_from_slice(b"SEGA MEGA DRIVE ");
    rom[0x120..0x150].fill(b' ');
    rom[0x150..0x180].fill(b' ');
    rom[0x150..0x163].copy_from_slice(b"SOL OS MD TEST CART");
    rom[0x1f0..0x200].fill(b' ');
    rom[0x1f0..0x1f3].copy_from_slice(b"JUE");
    fn emit(rom: &mut [u8], at: &mut usize, words: &[u16]) {
        for word in words {
            rom[*at..*at + 2].copy_from_slice(&word.to_be_bytes());
            *at += 2;
        }
    }
    let mut at = 0x200;
    emit(&mut rom, &mut at, &[0x46fc, 0x2700]); // MOVE #$2700,SR
    emit(&mut rom, &mut at, &[0x23fc, 0x5345, 0x4741, 0x00a1, 0x4000]); // TMSS
    for reg in [
        0x8004, 0x8164, 0x8230, 0x8407, 0x8578, 0x8701, 0x8c81, 0x8f02,
    ] {
        emit(&mut rom, &mut at, &[0x33fc, reg, 0x00c0, 0x0004]);
    }
    // CRAM colour 1 = red, displayed as backdrop through transparent tiles.
    emit(&mut rom, &mut at, &[0x23fc, 0xc002, 0x0000, 0x00c0, 0x0004]);
    emit(&mut rom, &mut at, &[0x33fc, 0x000e, 0x00c0, 0x0000]);
    for (value, addr) in [(0x40, 0x0009), (0x40, 0x0003)] {
        emit(&mut rom, &mut at, &[0x13fc, value, 0x00a1, addr]);
    }
    for value in [0x85, 0x08, 0x94] {
        emit(&mut rom, &mut at, &[0x13fc, value, 0x00c0, 0x0011]);
    }
    emit(&mut rom, &mut at, &[0x4e72, 0x2000, 0x60fa]); // STOP; BRA STOP
    at = 0x600;
    emit(&mut rom, &mut at, &[0x5239, 0x00ff, 0x0000]); // ADDQ.B #1,$ff0000
    emit(&mut rom, &mut at, &[0x1039, 0x00a1, 0x0003]); // MOVE.B pad,D0
    emit(&mut rom, &mut at, &[0x13c0, 0x00ff, 0x0002]); // MOVE.B D0,$ff0002
    emit(
        &mut rom,
        &mut at,
        &[0x323c, 0x000e, 0x0800, 0x0004, 0x6604, 0x323c, 0x00e0],
    );
    emit(&mut rom, &mut at, &[0x23fc, 0xc002, 0x0000, 0x00c0, 0x0004]);
    emit(&mut rom, &mut at, &[0x33c1, 0x00c0, 0x0000, 0x4e73]); // MOVE.W D1,CRAM; RTE
    at = 0x700;
    emit(
        &mut rom,
        &mut at,
        &[0x13fc, 0x00ee, 0x00ff, 0x0004, 0x4e72, 0x2700],
    );
    rom
}
