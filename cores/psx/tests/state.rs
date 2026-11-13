//! Save states: a machine put back from one runs on exactly as it did,
//! and a state of another game, or cut short, is refused.

use psx::{Error, Psx};

const TESTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/ps1-tests/");

fn load(test: &str) -> Option<Psx> {
    let name = test.rsplit('/').next().unwrap();
    let Ok(exe) = std::fs::read(format!("{TESTS}{test}/{name}.exe")) else {
        eprintln!("skipped: run tests/roms/fetch.sh");
        return None;
    };
    Some(Psx::with_exe(&exe).unwrap())
}

/// The VRAM and the registers after some frames.
fn run(psx: &mut Psx, frames: u32) -> (Vec<u16>, psx::Registers) {
    for _ in 0..frames {
        psx.run_frame();
    }
    (psx.vram().to_vec(), psx.registers())
}

#[test]
fn round_trip() {
    let Some(mut psx) = load("gpu/animated-triangle") else { return };
    run(&mut psx, 30);
    let state = psx.save_state();
    let first = run(&mut psx, 45);
    psx.load_state(&state).unwrap();
    assert_eq!(run(&mut psx, 45), first);
    // A state saved after loading one is the same as the one loaded.
    psx.load_state(&state).unwrap();
    assert_eq!(psx.save_state(), state);
}

#[test]
fn refuses_what_does_not_fit() {
    let (Some(mut a), Some(mut b)) = (load("gpu/animated-triangle"), load("gpu/triangle")) else { return };
    let state = b.save_state();
    assert_eq!(a.load_state(&state), Err(Error::OtherGame));
    let own = a.save_state();
    assert_eq!(a.load_state(&own[..own.len() - 1]), Err(Error::BadState));
    assert_eq!(a.load_state(b"SOLGBA"), Err(Error::BadState));
}

/// The Multitap goes after the rest of a state: one saved before it (the
/// state without its slots) still loads, its slots as at power-on, and a
/// whole one brings the slots' modes back.
#[test]
fn states_from_before_the_multitap_still_load() {
    let Some(mut psx) = load("gpu/animated-triangle") else { return };
    run(&mut psx, 5);
    let before = psx.save_state();
    psx.set_multitap(Some(0), 4);
    let whole = psx.save_state();
    // The Multitap's part is the same plugged in or not: what it adds is its tail.
    assert_eq!(whole, before);
    let mut other = load("gpu/animated-triangle").unwrap();
    run(&mut other, 5);
    let empty = other.save_state();
    let tail = empty.len() - (0..empty.len()).rev().find(|&cut| other.clone().load_state(&empty[..cut]).is_ok()).unwrap();
    assert!(tail > 0);
    assert_eq!(psx.load_state(&whole[..whole.len() - tail]), Ok(()));
    assert_eq!(psx.load_state(&whole), Ok(()));
    assert_eq!(psx.save_state(), whole);
}
