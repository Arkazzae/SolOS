//! Runs a game through the module's functions, natively, and prints a hash
//! of every frame's picture and sound: web/tools/check.ts runs the same game
//! in the WebAssembly build and compares, so the browser's machine is the
//! PC's bit for bit.
//!
//!     cargo run --release -p solweb --example check -- nes game.nes 600

use std::time::{Duration, Instant};

/// FNV-1a, 32 bits: what tools/check.ts computes in JavaScript too.
fn fnv(mut hash: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        hash = (hash ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    hash
}

/// The buttons pressed at a frame: Start now and then, so that a game gets
/// past its title, and the same in check.ts.
pub fn pad_at(frame: u32) -> u32 {
    if frame % 120 >= 100 && frame % 120 < 106 { solweb::pad::START } else { 0 }
}

/// The clock both builds give: a fixed moment, so that a cartridge's clock
/// and PICO-8's rnd() come out the same.
pub const CLOCK: f64 = 1_700_000_000.0;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, sys, path, frames] = &args[..] else {
        eprintln!("usage: check <console> <game> <frames>");
        std::process::exit(2);
    };
    let rom = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let frames: u32 = frames.parse().expect("frames");
    unsafe {
        if solweb::open(sys.as_ptr(), sys.len(), rom.as_ptr(), rom.len()) != 0 {
            let why = std::slice::from_raw_parts(solweb::out(), solweb::out_len());
            panic!("{}", String::from_utf8_lossy(why));
        }
    }
    solweb::set_clock(CLOCK, 0);
    let (mut picture_hash, mut audio_hash) = (0x811c_9dc5u32, 0x811c_9dc5u32);
    // The time the page would take: a frame and its picture, not the hashing.
    let mut spent = Duration::ZERO;
    for frame in 0..frames {
        let start = Instant::now();
        solweb::run_frame(pad_at(frame), 0, 0, 0);
        let (width, height) = (solweb::width() as usize, solweb::height() as usize);
        let picture = unsafe { std::slice::from_raw_parts(solweb::picture(), width * height * 4) };
        spent += start.elapsed();
        picture_hash = fnv(picture_hash, picture);
        let audio = unsafe { std::slice::from_raw_parts(solweb::audio().cast::<u8>(), solweb::audio_len() * 2) };
        audio_hash = fnv(audio_hash, audio);
    }
    let seconds = spent.as_secs_f64();
    let state = solweb::save_state();
    println!(
        "{{\"picture\":{picture_hash},\"audio\":{audio_hash},\"state\":{state},\"width\":{},\"height\":{},\"fps\":{:.1}}}",
        solweb::width(),
        solweb::height(),
        f64::from(frames) / seconds
    );
}
