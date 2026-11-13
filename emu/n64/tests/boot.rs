//! The boot: a game's own boot code (IPL3) runs from the state the PIF
//! leaves, sets up RDRAM, loads the game's first megabyte and jumps to it;
//! then the game draws, through its own RSP microcode and the RDP. Run on
//! the user's own cartridges, which are not in the repository: set N64_ROMS
//! to a directory holding them (by default /tmp/n64-roms).

use n64::N64;

fn rom(name: &str) -> Option<Vec<u8>> {
    let dir = std::env::var("N64_ROMS").unwrap_or_else(|_| "/tmp/n64-roms".into());
    let rom = std::fs::read(format!("{dir}/{name}")).ok();
    if rom.is_none() {
        eprintln!("skipped: no {name} in {dir} (set N64_ROMS)");
    }
    rom
}

/// Runs a game's boot code until it reaches the game's entry point, for
/// at most two emulated seconds; fails if it does not, and then checks
/// what the boot code leaves: the first megabyte of the game in place, and
/// the size of RDRAM where libultra reads it (osMemSize).
fn boot(name: &str) -> Option<N64> {
    let data = rom(name)?;
    let mut n64 = N64::new(&data).unwrap();
    let entry = u64::from(n64.entry()) | 0xffff_ffff_0000_0000;
    let mut steps = 0u64;
    while n64.registers().pc != entry {
        n64.step();
        steps += 1;
        assert!(steps < 200_000_000, "{name}: did not reach {entry:x}, at {:x?}", n64.registers());
    }
    eprintln!("{name}: at the entry point after {steps} instructions");
    let base = n64.entry() & 0x1fff_ffff;
    for i in (0..0x10_0000).step_by(4) {
        let loaded: Vec<u8> = (0..4).map(|b| n64.peek(base + i + b)).collect();
        assert_eq!(loaded[..], data[0x1000 + i as usize..0x1004 + i as usize], "{name}: RDRAM {:x}", base + i);
    }
    let size: u32 = (0..4).map(|b| u32::from(n64.peek(0x318 + b)) << (24 - 8 * b)).sum();
    assert_eq!(size, 0x80_0000, "{name}: osMemSize");
    Some(n64)
}

#[test]
fn super_mario_64() {
    boot("Super Mario 64 (USA).z64");
}

#[test]
fn mario_kart_64() {
    boot("Mario Kart 64 (USA).z64");
}

#[test]
fn goldeneye_007() {
    boot("GoldenEye 007 (USA).z64");
}

/// CIC 6103: the boot code loads the game 1 MB below the header's address.
#[test]
fn paper_mario() {
    boot("Paper Mario (USA).z64");
}

#[test]
fn zelda_ocarina_of_time() {
    boot("The Legend of Zelda - Ocarina of Time (USA) (Rev 2).z64");
}

/// CIC 6105: the boot code starts the RSP on a loop that waits for the CPU
/// (giving up after so many turns), then copies part of the boot code into
/// RDRAM, a word of which Donkey Kong 64 waits for at 2F_E1C0. That takes
/// the RSP's pipeline to be timed: an instruction a cycle, the loop gives
/// up before the CPU is done.
#[test]
fn donkey_kong_64() {
    let Some(n64) = boot("Donkey Kong 64 (USA).z64") else { return };
    let word: Vec<u8> = (0..4).map(|b| n64.peek(0x2f_e1c0 + b)).collect();
    assert_eq!(word, [0xad, 0x17, 0x00, 0x14]);
}

/// Super Mario 64 draws its intro: the logo at frame 200 and Mario's head
/// on the title screen at frame 600, each a full picture of many colors;
/// and it plays sound, through its audio microcode and the AI.
#[test]
fn super_mario_64_draws() {
    let Some(data) = rom("Super Mario 64 (USA).z64") else { return };
    let mut n64 = N64::new(&data).unwrap();
    let mut energy = 0f64;
    let mut samples = 0usize;
    for (frames, scene) in [(200, "logo"), (400, "title")] {
        for _ in 0..frames {
            n64.run_frame(0);
            energy += n64.audio().iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>();
            samples += n64.audio().len();
        }
        let (pixels, width, height) = n64.frame();
        assert_eq!((width, height), (320, 237), "{scene}");
        let lit = pixels.iter().filter(|&&p| p != 0).count();
        let mut colors = pixels.to_vec();
        colors.sort_unstable();
        colors.dedup();
        eprintln!("{scene}: {lit} lit pixels, {} colors", colors.len());
        assert!(lit > pixels.len() / 5 && colors.len() > 200, "{scene}: {lit} lit pixels, {} colors", colors.len());
    }
    // Ten seconds at 48 kHz, and loud enough to be music and voices.
    let rms = (energy / samples as f64).sqrt();
    eprintln!("{samples} samples, rms {rms:.0}");
    assert!(samples > 2 * 48_000 * 9 && rms > 500.0, "{samples} samples, rms {rms:.0}");
}
