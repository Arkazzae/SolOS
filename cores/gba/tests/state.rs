//! Save states and saves: a machine put in a saved state goes on exactly as
//! the one that saved it, picture and sound; a state that does not fit is
//! refused, leaving the machine as it was; and what a game keeps in its
//! save memory (SRAM, Flash) comes back from the host's file.

use gba::{Error, Gba};

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/");

fn rom(path: &str) -> Option<Vec<u8>> {
    let rom = std::fs::read(format!("{ROMS}{path}")).ok();
    if rom.is_none() {
        eprintln!("skipped: run tests/roms/fetch.sh");
    }
    rom
}

/// A hash of each of the next frames' picture and sound, with a button
/// pressed now and then.
fn play(gba: &mut Gba, frames: u32) -> Vec<u64> {
    (0..frames)
        .map(|i| {
            gba.run_frame(if i % 16 < 4 { gba::A | gba::START } else { 0 });
            let picture = gba.frame().iter().map(|&p| u64::from(p));
            let sound = gba.audio().iter().map(|&s| u64::from(s as u16));
            picture.chain(sound).fold(0xcbf2_9ce4_8422_2325u64, |h, x| (h ^ x).wrapping_mul(0x0100_0000_01b3))
        })
        .collect()
}

#[test]
fn round_trip() {
    for path in [
        // Sound from the Game Boy channels, and from DMA through the FIFOs.
        "tonc/bin/snd1_demo.gba",
        "alyosha/fifo_dma/fifo.gba",
        // Interrupts every line, HBlank DMA, timers, BIOS calls waiting.
        "tonc/bin/m7_ex.gba",
        "tonc/bin/dma_demo.gba",
        "tonc/bin/tmr_demo.gba",
        "tonc/bin/swi_vsync.gba",
        // SRAM and Flash.
        "gba-tests/save/sram.gba",
        "gba-tests/save/flash128.gba",
    ] {
        let Some(data) = rom(path) else { return };
        let mut gba = Gba::new(&data).unwrap();
        for _ in 0..90 {
            gba.run_frame(0);
        }
        let state = gba.save_state();
        let expected = play(&mut gba, 60);

        // Another machine, somewhere else in the game, put in that state.
        let mut other = Gba::new(&data).unwrap();
        for _ in 0..7 {
            other.run_frame(gba::START);
        }
        other.load_state(&state).unwrap();
        assert_eq!(play(&mut other, 60), expected, "{path}");
        assert!(expected.windows(2).any(|w| w[0] != w[1]), "{path}: nothing changes");
        eprintln!("{path}: {} byte state", state.len());
    }
}

#[test]
fn refuses_what_does_not_fit() {
    let (Some(a), Some(b)) = (rom("tonc/bin/hello.gba"), rom("tonc/bin/m7_ex.gba")) else { return };
    let mut gba = Gba::new(&a).unwrap();
    gba.run_frame(0);
    let state = gba.save_state();
    let mut other = Gba::new(&b).unwrap();
    other.run_frame(0);
    let before = other.save_state();
    assert_eq!(other.load_state(&state), Err(Error::OtherGame));
    assert_eq!(gba.load_state(&state[..state.len() - 1]), Err(Error::BadState));
    assert_eq!(gba.load_state(b"nonsense"), Err(Error::BadState));
    assert_eq!(other.save_state(), before, "a refused state changed the machine");
}

/// jsmolka's save tests write the save memory and read it back; what they
/// leave there is the host's to keep, and a new machine given it reads
/// the same. A game without save memory has none to keep.
#[test]
fn save_data() {
    for (path, size) in [
        ("gba-tests/save/sram.gba", 0x8000),
        ("gba-tests/save/flash64.gba", 0x10000),
        ("gba-tests/save/flash128.gba", 0x20000),
    ] {
        let Some(data) = rom(path) else { return };
        let mut gba = Gba::new(&data).unwrap();
        for _ in 0..30 {
            gba.run_frame(0);
        }
        let saved = gba.save_data().unwrap();
        assert_eq!(saved.len(), size, "{path}");
        assert!(saved.iter().any(|&b| b != 0xff), "{path}: nothing written");
        let mut other = Gba::new(&data).unwrap();
        other.load_save_data(&saved);
        assert_eq!(other.save_data().unwrap(), saved, "{path}");
    }
    let Some(data) = rom("gba-tests/save/none.gba") else { return };
    assert_eq!(Gba::new(&data).unwrap().save_data(), None);
}
