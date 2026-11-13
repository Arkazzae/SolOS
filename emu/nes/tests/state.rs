//! Save states: a machine put in a saved state goes on exactly as the one
//! that saved it, picture and sound, on boards with each mapper; and a
//! state that does not fit is refused, leaving the machine as it was.

use nes::{Error, Nes};

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/nes-test-roms/");

fn rom(path: &str) -> Option<Vec<u8>> {
    let rom = std::fs::read(format!("{ROMS}{path}")).ok();
    if rom.is_none() {
        eprintln!("skipped: run tests/roms/fetch.sh");
    }
    rom
}

/// A hash of each of the next frames' picture and sound, with a pad
/// pressing right now and then.
fn play(nes: &mut Nes, frames: u32) -> Vec<u64> {
    (0..frames)
        .map(|i| {
            nes.run_frame([if i % 8 < 4 { nes::RIGHT } else { 0 }, 0]);
            let picture = nes.frame().iter().map(|&p| p as u64);
            let sound = nes.audio().iter().map(|&s| s as u16 as u64);
            picture.chain(sound).fold(0xcbf2_9ce4_8422_2325u64, |h, x| (h ^ x).wrapping_mul(0x0100_0000_01b3))
        })
        .collect()
}

#[test]
fn round_trip() {
    for path in [
        "spritecans-2011/spritecans.nes",
        "other/Retrocoders - Years behind.NES",
        "other/PCM.demo.wgraphics.nes",
        "other/quantum_disco_brothers_by_wAMMA.nes",
        "other/BladeBuster.nes",
        "other/oam3.nes",
    ] {
        let Some(data) = rom(path) else { return };
        let mut nes = Nes::new(&data).unwrap();
        for _ in 0..120 {
            nes.run_frame([0, 0]);
        }
        let state = nes.save_state();
        let expected = play(&mut nes, 60);

        // Another machine, somewhere else in the game, put in that state.
        let mut other = Nes::new(&data).unwrap();
        for _ in 0..7 {
            other.run_frame([nes::START, 0]);
        }
        other.load_state(&state).unwrap();
        assert_eq!(play(&mut other, 60), expected, "{path}");
        eprintln!("{path}: {} byte state", state.len());
    }
}

#[test]
fn refuses_what_does_not_fit() {
    let (Some(a), Some(b)) = (rom("spritecans-2011/spritecans.nes"), rom("other/oam3.nes")) else { return };
    let mut nes = Nes::new(&a).unwrap();
    nes.run_frame([0, 0]);
    let state = nes.save_state();
    let mut other = Nes::new(&b).unwrap();
    other.run_frame([0, 0]);
    let before = other.save_state();
    assert_eq!(other.load_state(&state), Err(Error::OtherGame));
    assert_eq!(nes.load_state(&state[..state.len() - 1]), Err(Error::BadState));
    assert_eq!(nes.load_state(b"nonsense"), Err(Error::BadState));
    assert_eq!(other.save_state(), before, "a refused state changed the machine");
}
