//! Runs a ROM headless and saves frames as PGM pictures, to look at the
//! core's output on a PC.
//!
//!   cargo run --release --example run-gb -- ROM FRAMES OUT.pnm [EVERY] [--wav SOUND.wav]
//!       [--press BUTTON@FRAME]... [--dmg]
//!
//! Saves the last frame to OUT.pnm (a PGM of shades, or on the Color a
//! PPM), or with EVERY one frame in that many as OUT-<frame>.pnm; --wav
//! saves the sound of all the frames; --press holds a button (a, b, select,
//! start, right, left, up, down) for a tenth of a second from a frame on.
//! A game made for the Color runs on the Color, unless --dmg. Prints what
//! the game sent out of the link port, and the CPU's registers at the end.

use std::io::Write;

fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    let wav = args.iter().position(|a| a == "--wav").map(|i| {
        let path = args.get(i + 1).expect("--wav takes a file").clone();
        args.drain(i..i + 2);
        path
    });
    let dmg = args.iter().position(|a| a == "--dmg").map(|i| args.remove(i)).is_some();
    let mut presses = Vec::new();
    while let Some(i) = args.iter().position(|a| a == "--press") {
        let press = args.get(i + 1).expect("--press takes BUTTON@FRAME").clone();
        args.drain(i..i + 2);
        let (name, frame) = press.split_once('@').expect("--press takes BUTTON@FRAME");
        let names = ["right", "left", "up", "down", "a", "b", "select", "start"];
        let bit = names.iter().position(|&n| n == name).unwrap_or_else(|| panic!("no button {name}"));
        presses.push((1u8 << bit, frame.parse::<u32>().expect("FRAME is a number")));
    }
    let mut sound = Vec::new();
    if args.len() < 4 {
        eprintln!("usage: run-gb ROM FRAMES OUT.pnm [EVERY]");
        std::process::exit(2);
    }
    let rom = std::fs::read(&args[1]).expect("cannot read the ROM");
    let frames: u32 = args[2].parse().expect("FRAMES is a number");
    let every: Option<u32> = args.get(4).map(|n| n.parse().expect("EVERY is a number"));
    let model = if dmg { gb::Model::Dmg } else { gb::Model::detect(&rom) };
    let mut gb = gb::Gb::new(&rom, model).unwrap_or_else(|e| panic!("{}: {e}", args[1]));
    let started = std::time::Instant::now();
    for frame in 1..=frames {
        let held = presses.iter().filter(|&&(_, at)| (at..at + 6).contains(&frame)).fold(0, |b, &(bit, _)| b | bit);
        gb.run_frame(held);
        sound.extend_from_slice(gb.audio());
        if let Some(every) = every
            && frame % every == 0
        {
            save(&gb, &args[3].replace(".pnm", &format!("-{frame:05}.pnm")));
        }
    }
    let elapsed = started.elapsed();
    eprintln!("{frames} frames in {:.2} s ({:.0} fps)", elapsed.as_secs_f64(), frames as f64 / elapsed.as_secs_f64());
    if every.is_none() {
        save(&gb, &args[3]);
    }
    if !gb.serial().is_empty() {
        eprintln!("serial: {}", String::from_utf8_lossy(gb.serial()));
    }
    eprintln!("{:02x?}", gb.registers());
    if let Some(path) = wav {
        save_wav(&sound, &path);
        let peak = sound.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        eprintln!("{} samples, peak {peak}", sound.len());
    }
}

/// Stereo 16-bit PCM.
fn save_wav(samples: &[i16], path: &str) {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).expect("cannot create the sound"));
    let bytes = (samples.len() * 2) as u32;
    let rate = gb::SAMPLE_HZ;
    out.write_all(b"RIFF").unwrap();
    out.write_all(&(36 + bytes).to_le_bytes()).unwrap();
    out.write_all(b"WAVEfmt ").unwrap();
    for field in [16u32, 1 | 2 << 16, rate, rate * 4, 4 | 16 << 16] {
        out.write_all(&field.to_le_bytes()).unwrap();
    }
    out.write_all(b"data").unwrap();
    out.write_all(&bytes.to_le_bytes()).unwrap();
    for s in samples {
        out.write_all(&s.to_le_bytes()).unwrap();
    }
}

/// The DMG's four shades as greys, lightest first; the Color's colours
/// with their five bits a channel widened to eight.
fn save(gb: &gb::Gb, path: &str) {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).expect("cannot create the picture"));
    if gb.color() {
        write!(out, "P6\n{} {}\n255\n", gb::WIDTH, gb::HEIGHT).unwrap();
        let channel = |c: u16| (c << 3 | c >> 2) as u8;
        for &rgb in gb.frame() {
            out.write_all(&[channel(rgb & 31), channel(rgb >> 5 & 31), channel(rgb >> 10 & 31)]).unwrap();
        }
    } else {
        write!(out, "P5\n{} {}\n255\n", gb::WIDTH, gb::HEIGHT).unwrap();
        let grey: Vec<u8> = gb.frame().iter().map(|&shade| [0xff, 0xaa, 0x55, 0x00][usize::from(shade)]).collect();
        out.write_all(&grey).unwrap();
    }
}
