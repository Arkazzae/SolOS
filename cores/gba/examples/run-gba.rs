//! Runs a ROM headless and saves frames as PPM pictures, to look at the
//! core's output on a PC.
//!
//!   cargo run --release --example run-gba -- ROM FRAMES OUT.ppm [EVERY] [--wav SOUND.wav]
//!       [--press BUTTON@FRAME]...
//!
//! Saves the last frame to OUT.ppm, or with EVERY one frame in that many as
//! OUT-<frame>.ppm; --wav saves the sound of all the frames; --press holds
//! a button (a, b, select, start, right, left, up, down, r, l) for a tenth
//! of a second from a frame on. Prints the CPU's registers at the end.

use std::io::Write;

fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    let wav = args.iter().position(|a| a == "--wav").map(|i| {
        let path = args.get(i + 1).expect("--wav takes a file").clone();
        args.drain(i..i + 2);
        path
    });
    let mut presses = Vec::new();
    while let Some(i) = args.iter().position(|a| a == "--press") {
        let press = args.get(i + 1).expect("--press takes BUTTON@FRAME").clone();
        args.drain(i..i + 2);
        let (name, frame) = press.split_once('@').expect("--press takes BUTTON@FRAME");
        let names = ["a", "b", "select", "start", "right", "left", "up", "down", "r", "l"];
        let bit = names.iter().position(|&n| n == name).unwrap_or_else(|| panic!("no button {name}"));
        presses.push((1u16 << bit, frame.parse::<u32>().expect("FRAME is a number")));
    }
    let mut sound = Vec::new();
    if args.len() < 4 {
        eprintln!("usage: run-gba ROM FRAMES OUT.ppm [EVERY]");
        std::process::exit(2);
    }
    let rom = std::fs::read(&args[1]).expect("cannot read the ROM");
    let frames: u32 = args[2].parse().expect("FRAMES is a number");
    let every: Option<u32> = args.get(4).map(|n| n.parse().expect("EVERY is a number"));
    let mut gba = gba::Gba::new(&rom).unwrap_or_else(|e| panic!("{}: {e}", args[1]));
    let started = std::time::Instant::now();
    for frame in 1..=frames {
        let held = presses.iter().filter(|&&(_, at)| (at..at + 6).contains(&frame)).fold(0, |b, &(bit, _)| b | bit);
        gba.run_frame(held);
        sound.extend_from_slice(gba.audio());
        if let Some(every) = every
            && frame % every == 0
        {
            save(&gba, &args[3].replace(".ppm", &format!("-{frame:05}.ppm")));
        }
    }
    let elapsed = started.elapsed();
    eprintln!("{frames} frames in {:.2} s ({:.0} fps)", elapsed.as_secs_f64(), frames as f64 / elapsed.as_secs_f64());
    if every.is_none() {
        save(&gba, &args[3]);
    }
    eprintln!("{:08x?}", gba.registers());
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
    let rate = gba::SAMPLE_HZ;
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

/// The colours with their five bits a channel widened to eight.
fn save(gba: &gba::Gba, path: &str) {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).expect("cannot create the picture"));
    write!(out, "P6\n{} {}\n255\n", gba::WIDTH, gba::HEIGHT).unwrap();
    let channel = |c: u16| (c << 3 | c >> 2) as u8;
    for &rgb in gba.frame() {
        out.write_all(&[channel(rgb & 31), channel(rgb >> 5 & 31), channel(rgb >> 10 & 31)]).unwrap();
    }
}
