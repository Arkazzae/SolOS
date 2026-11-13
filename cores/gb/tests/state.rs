//! Save states: a machine put in a saved state goes on exactly as the one
//! that saved it, picture and sound, with each memory bank controller; and
//! a state that does not fit is refused, leaving the machine as it was.

use gb::{Error, Gb, Model};

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/game-boy-test-roms/");

fn rom(path: &str) -> Option<Vec<u8>> {
    let rom = std::fs::read(format!("{ROMS}{path}")).ok();
    if rom.is_none() {
        eprintln!("skipped: run tests/roms/fetch.sh");
    }
    rom
}

/// A hash of each of the next frames' picture and sound, with a button
/// pressed now and then.
fn play(gb: &mut Gb, frames: u32) -> Vec<u64> {
    (0..frames)
        .map(|i| {
            gb.run_frame(if i % 8 < 4 { gb::A } else { 0 });
            let picture = gb.frame().iter().map(|&p| u64::from(p));
            let sound = gb.audio().iter().map(|&s| u64::from(s as u16));
            picture.chain(sound).fold(0xcbf2_9ce4_8422_2325u64, |h, x| (h ^ x).wrapping_mul(0x0100_0000_01b3))
        })
        .collect()
}

#[test]
fn round_trip() {
    for (path, model) in [
        ("dmg-acid2/dmg-acid2.gb", Model::Dmg),
        ("blargg/dmg_sound/dmg_sound.gb", Model::Dmg),
        ("blargg/cpu_instrs/cpu_instrs.gb", Model::Dmg),
        ("mooneye-test-suite/emulator-only/mbc2/ram.gb", Model::Dmg),
        ("mbc3-tester/mbc3-tester.gb", Model::Dmg),
        ("mooneye-test-suite/emulator-only/mbc5/rom_64Mb.gb", Model::Dmg),
        ("cgb-acid2/cgb-acid2.gbc", Model::Cgb),
        ("blargg/cgb_sound/cgb_sound.gb", Model::Cgb),
        // Double speed.
        ("blargg/interrupt_time/interrupt_time.gb", Model::Cgb),
    ] {
        let Some(data) = rom(path) else { return };
        let mut gb = Gb::new(&data, model).unwrap();
        for _ in 0..120 {
            gb.run_frame(0);
        }
        let state = gb.save_state();
        let expected = play(&mut gb, 60);

        // Another machine, somewhere else in the game, put in that state.
        let mut other = Gb::new(&data, model).unwrap();
        for _ in 0..7 {
            other.run_frame(gb::START);
        }
        other.load_state(&state).unwrap();
        assert_eq!(play(&mut other, 60), expected, "{path}");
        eprintln!("{path}: {} byte state", state.len());
    }
}

#[test]
fn refuses_what_does_not_fit() {
    let (Some(a), Some(b)) = (rom("dmg-acid2/dmg-acid2.gb"), rom("blargg/dmg_sound/dmg_sound.gb")) else { return };
    // A state of the same game on the other console.
    let color = Gb::new(&b, Model::Cgb).unwrap().save_state();
    assert_eq!(Gb::new(&b, Model::Dmg).unwrap().load_state(&color), Err(Error::BadState));

    let mut gb = Gb::new(&a, Model::Dmg).unwrap();
    gb.run_frame(0);
    let state = gb.save_state();
    let mut other = Gb::new(&b, Model::Dmg).unwrap();
    other.run_frame(0);
    let before = other.save_state();
    assert_eq!(other.load_state(&state), Err(Error::OtherGame));
    assert_eq!(gb.load_state(&state[..state.len() - 1]), Err(Error::BadState));
    assert_eq!(gb.load_state(b"nonsense"), Err(Error::BadState));
    assert_eq!(other.save_state(), before, "a refused state changed the machine");
}

/// What the battery keeps: an MBC3's clock is saved after the RAM as BGB
/// and VBA-M save it, and moves on by the time between saving and loading.
#[test]
fn clock_in_save_data() {
    let Some(data) = rom("rtc3test/rtc3test.gb") else { return };
    let saved = Gb::new(&data, Model::Dmg).unwrap().save_data(1_000).unwrap();
    assert_eq!(saved.len(), 48);
    assert_eq!(saved[40..], 1_000u64.to_le_bytes());

    let mut gb = Gb::new(&data, Model::Dmg).unwrap();
    gb.load_save_data(&saved, 1_000 + 2 * 86_400 + 3_661);
    let registers = |data: &[u8]| (0..5).map(|i| data[i * 4]).collect::<Vec<_>>();
    // Seconds, minutes, hours, days.
    assert_eq!(registers(&gb.save_data(0).unwrap()), [1, 1, 1, 2, 0]);

    // A 32-bit time, as some emulators write it.
    let mut gb = Gb::new(&data, Model::Dmg).unwrap();
    gb.load_save_data(&[&saved[..40], &1_000u32.to_le_bytes()].concat(), 1_059);
    assert_eq!(registers(&gb.save_data(0).unwrap()), [59, 0, 0, 0, 0]);
    gb.pass_time(1);
    assert_eq!(registers(&gb.save_data(0).unwrap()), [0, 1, 0, 0, 0]);
}
