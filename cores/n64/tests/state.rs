//! Save states: a machine put in a saved state goes on exactly as the one
//! that saved it, picture and CPU, from today's format and from version 3;
//! a state that does not fit is refused, leaving the machine as it was.

use n64::{Error, N64};

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/krom/");

fn rom(path: &str) -> Option<Vec<u8>> {
    let rom = std::fs::read(path).ok();
    if rom.is_none() {
        eprintln!("skipped: no {path} (run tests/roms/fetch.sh, or set N64_ROMS)");
    }
    rom
}

/// A hash of each of the next frames' picture and the CPU's registers, with
/// a button pressed now and then.
fn play(n64: &mut N64, frames: u32) -> Vec<u64> {
    (0..frames)
        .map(|i| {
            n64.run_frame(if i % 16 < 4 { n64::A | n64::START } else { 0 });
            let (pixels, width, height) = n64.frame();
            let r = n64.registers();
            let cpu = r.gpr.iter().copied().chain([r.hi, r.lo, r.pc, u64::from(r.status), u64::from(r.cause)]);
            let picture = pixels.iter().map(|&p| u64::from(p)).chain([width as u64, height as u64]);
            picture.chain(cpu).fold(0xcbf2_9ce4_8422_2325u64, |h, x| (h ^ x).wrapping_mul(0x0100_0000_01b3))
        })
        .collect()
}

fn roms() -> [String; 3] {
    let games = std::env::var("N64_ROMS").unwrap_or_else(|_| "/tmp/n64-roms".into());
    [
        // The FPU drawing, a line at a time.
        format!("{ROMS}CP1/Fractal/32BPP/320X240/Mandelbrot/Double/Mandelbrot32BPP320X240.N64"),
        // The timer's interrupt, and VI interrupts.
        format!("{ROMS}Exceptions/Compare/ExceptionCompareRegisters.n64"),
        // A game's threads, its interrupts, DMAs and the PIF.
        format!("{games}/Super Mario 64 (USA).z64"),
    ]
}

#[test]
fn round_trip() {
    for path in roms() {
        let Some(data) = rom(&path) else { continue };
        let mut n64 = N64::new(&data).unwrap();
        for _ in 0..20 {
            n64.run_frame(0);
        }
        let state = n64.save_state();
        let expected = play(&mut n64, 40);

        // Another machine, somewhere else in the game, put in that state.
        let mut other = N64::new(&data).unwrap();
        for _ in 0..7 {
            other.run_frame(n64::START);
        }
        other.load_state(&state).unwrap();
        assert_eq!(play(&mut other, 40), expected, "{path}");
        assert!(expected.windows(2).any(|w| w[0] != w[1]), "{path}: nothing changes");
        eprintln!("{path}: {} byte state", state.len());
    }
}

/// A state from before the named records (version 3, as the console's
/// Quick Resume states and the benchmarks' are) loads, and the machine goes
/// on as from today's state of the same moment, which saving it again
/// gives.
#[test]
fn loads_version_3() {
    for path in roms() {
        let Some(data) = rom(&path) else { continue };
        let mut n64 = N64::new(&data).unwrap();
        for _ in 0..20 {
            n64.run_frame(0);
        }
        let old = n64.save_state_version_3();
        let state = n64.save_state();
        assert_eq!((old[6], state[6]), (3, 4), "{path}");
        assert!(old[..6] == state[..6] && old[7..] == state[7..old.len()], "{path}: version 3's fields first");
        let expected = play(&mut n64, 40);

        let mut other = N64::new(&data).unwrap();
        for _ in 0..7 {
            other.run_frame(n64::START);
        }
        other.load_state(&old).unwrap();
        assert!(other.save_state() == state, "{path}: saved again");
        assert_eq!(play(&mut other, 40), expected, "{path}");
    }
}

#[test]
fn refuses_what_does_not_fit() {
    let (Some(a), Some(b)) = (
        rom(&format!("{ROMS}CPU/ADD/CPUADD.N64")),
        rom(&format!("{ROMS}CP1/Fractal/32BPP/320X240/Mandelbrot/Single/Mandelbrot32BPP320X240.N64")),
    ) else {
        return;
    };
    let mut n64 = N64::new(&a).unwrap();
    n64.run_frame(0);
    let state = n64.save_state();
    let mut other = N64::new(&b).unwrap();
    for _ in 0..3 {
        other.run_frame(0);
    }
    let before = other.registers();
    assert_eq!(other.load_state(&state), Err(Error::OtherGame));
    assert_eq!(other.load_state(&state[..state.len() / 2]), Err(Error::OtherGame));
    assert_eq!(other.load_state(b"SOLN64"), Err(Error::BadState));
    assert_eq!(other.registers(), before);
    // Its own state, cut short or with more after it, is refused too, as
    // are a later version and a version 3 state with anything after its
    // fields.
    let own = other.save_state();
    assert_eq!(other.load_state(&own[..own.len() - 1]), Err(Error::BadState));
    assert_eq!(other.load_state(&[&own[..], &[0]].concat()), Err(Error::BadState));
    let mut later = own.clone();
    later[6] += 1;
    assert_eq!(other.load_state(&later), Err(Error::BadState));
    let old = other.save_state_version_3();
    assert_eq!(other.load_state(&[&old[..], &[0; 4]].concat()), Err(Error::BadState));
    assert_eq!(other.registers(), before);
    other.load_state(&own).unwrap();
    other.load_state(&old).unwrap();
}
