//! Reading RDRAM from outside, as achievements do after every frame.

use n64::N64;

/// A test ROM of the repository's, run a while: `peek_rdram` gives the bytes `peek` gives, whole RDRAM
/// over, and nothing past its 8 MB.
#[test]
fn rdram_as_peek_gives_it() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/dillon/basic.z64");
    let Ok(data) = std::fs::read(path) else {
        eprintln!("skipped: run tests/roms/fetch.sh");
        return;
    };
    let mut n64 = N64::new(&data).unwrap();
    for _ in 0..3 {
        n64.run_frame(0);
    }
    let mut nonzero = 0;
    for addr in (0..0x80_0000u32).step_by(7) {
        let byte = n64.peek_rdram(addr).expect("inside RDRAM");
        assert_eq!(byte, n64.peek(addr), "{addr:x}");
        nonzero += usize::from(byte != 0);
    }
    assert!(nonzero > 0, "a game has run: RDRAM is not all zero");
    assert_eq!((n64.peek_rdram(0x7f_ffff).is_some(), n64.peek_rdram(0x80_0000)), (true, None));
}
