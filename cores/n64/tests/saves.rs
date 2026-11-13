//! Saves: each game's save memory, found by the list of games, and a game
//! keeping its saves in it from one session to the next. Run on the user's
//! own cartridges, which are not in the repository: set N64_ROMS to a
//! directory holding them (by default /tmp/n64-roms).

use n64::N64;

fn rom(name: &str) -> Option<Vec<u8>> {
    let dir = std::env::var("N64_ROMS").unwrap_or_else(|_| "/tmp/n64-roms".into());
    let rom = std::fs::read(format!("{dir}/{name}")).ok();
    if rom.is_none() {
        eprintln!("skipped: no {name} in {dir} (set N64_ROMS)");
    }
    rom
}

/// Each of the user's games gets its save memory, by its size: EEPROMs of
/// 4 and 16 kbit, SRAM and FlashRAM.
#[test]
fn save_memory_by_game() {
    for (name, size) in [
        ("Super Mario 64 (USA).z64", 0x200),
        ("Mario Kart 64 (USA).z64", 0x200),
        ("GoldenEye 007 (USA).z64", 0x200),
        ("Donkey Kong 64 (USA).z64", 0x800),
        ("The Legend of Zelda - Ocarina of Time (USA) (Rev 2).z64", 0x8000),
        ("Paper Mario (USA).z64", 0x2_0000),
    ] {
        let Some(data) = rom(name) else { continue };
        let n64 = N64::new(&data).unwrap();
        assert_eq!(n64.save_data().map(|s| s.len()), Some(size), "{name}");
    }
}

/// The sum SM64 signs each block of its save with: of all its bytes but
/// the sum's own two.
fn sm64_checksum(block: &[u8]) -> [u8; 2] {
    let sum: u32 = block[..block.len() - 2].iter().map(|&b| u32::from(b)).sum();
    (sum as u16).to_be_bytes()
}

/// Super Mario 64 keeps its saves in a 4 kbit EEPROM: four files and the
/// menu's data, each block twice, each signed with a magic number and a
/// sum. From a blank EEPROM it writes them all fresh at boot. Next session
/// it reads them: a file whose second copy is broken it writes again from
/// the first. The EEPROM travels in save states too.
#[test]
fn super_mario_64_saves() {
    let Some(data) = rom("Super Mario 64 (USA).z64") else { return };
    let mut n64 = N64::new(&data).unwrap();
    for _ in 0..300 {
        n64.run_frame(0);
    }
    let save = n64.save_data().unwrap();
    for block in 0..8 {
        assert_eq!(&save[block * 0x38 + 0x34..][..2], b"DA", "file block {block}");
    }
    assert_eq!(&save[0x1c0 + 0x1c..][..2], b"HI", "menu data");
    assert_eq!(&save[0x1c0 + 0x1e..][..2], &sm64_checksum(&save[0x1c0..0x1e0]), "menu data");

    // File A (blocks 0 and 1, of 0x38 bytes): there, with the seven stars
    // of the first course; its second copy with a bad sum.
    let mut edited = save.clone();
    let file = &mut edited[..0x38];
    file[0x0b] = 1;
    file[0x0c] = 0x7f;
    let sum = sm64_checksum(file);
    file[0x36..].copy_from_slice(&sum);
    edited[0x38 + 0x36] ^= 0xff;

    let mut n64 = N64::new(&data).unwrap();
    n64.load_save_data(&edited);
    for _ in 0..300 {
        n64.run_frame(0);
    }
    let save = n64.save_data().unwrap();
    assert_eq!(&save[..0x38], &edited[..0x38], "file A");
    assert_eq!(&save[0x38..0x70], &edited[..0x38], "file A, written again");

    let mut other = N64::new(&data).unwrap();
    other.load_state(&n64.save_state()).unwrap();
    assert_eq!(other.save_data(), Some(save));
}
