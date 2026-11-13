//! Runs a ROM headless and saves frames as PPM pictures, to look at the
//! core's output on a PC.
//!
//!   cargo run --release --example run -- ROM FRAMES OUT.ppm [EVERY] [--wav SOUND.wav]
//!
//! Saves the last frame to OUT.ppm, or with EVERY one frame in that many as
//! OUT-<frame>.ppm; --wav saves the sound of all the frames.

use std::io::Write;

fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    let wav = args.iter().position(|a| a == "--wav").map(|i| {
        let path = args.get(i + 1).expect("--wav takes a file").clone();
        args.drain(i..i + 2);
        path
    });
    let mut sound = Vec::new();
    if args.len() < 4 {
        eprintln!("usage: run ROM FRAMES OUT.ppm [EVERY]");
        std::process::exit(2);
    }
    let rom = std::fs::read(&args[1]).expect("cannot read the ROM");
    let frames: u32 = args[2].parse().expect("FRAMES is a number");
    let every: Option<u32> = args.get(4).map(|n| n.parse().expect("EVERY is a number"));
    let mut nes = nes::Nes::new(&rom).unwrap_or_else(|e| panic!("{}: {e}", args[1]));
    let started = std::time::Instant::now();
    for frame in 1..=frames {
        nes.run_frame([0, 0]);
        sound.extend_from_slice(nes.audio());
        if let Some(every) = every
            && frame % every == 0
        {
            save(&nes, &args[3].replace(".ppm", &format!("-{frame:05}.ppm")));
        }
    }
    let elapsed = started.elapsed();
    eprintln!("{frames} frames in {:.2} s ({:.0} fps)", elapsed.as_secs_f64(), frames as f64 / elapsed.as_secs_f64());
    if every.is_none() {
        save(&nes, &args[3]);
    }
    if let Some(path) = wav {
        save_wav(&sound, &path);
        let peak = sound.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        eprintln!("{} samples, peak {peak}", sound.len());
    }
}

/// Mono 16-bit PCM.
fn save_wav(samples: &[i16], path: &str) {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).expect("cannot create the sound"));
    let bytes = (samples.len() * 2) as u32;
    let rate = nes::SAMPLE_HZ;
    out.write_all(b"RIFF").unwrap();
    out.write_all(&(36 + bytes).to_le_bytes()).unwrap();
    out.write_all(b"WAVEfmt ").unwrap();
    for field in [16u32, 1 | 1 << 16, rate, rate * 2, 2 | 16 << 16] {
        out.write_all(&field.to_le_bytes()).unwrap();
    }
    out.write_all(b"data").unwrap();
    out.write_all(&bytes.to_le_bytes()).unwrap();
    for s in samples {
        out.write_all(&s.to_le_bytes()).unwrap();
    }
}

fn save(nes: &nes::Nes, path: &str) {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).expect("cannot create the picture"));
    write!(out, "P6\n{} {}\n255\n", nes::WIDTH, nes::HEIGHT).unwrap();
    for &pixel in nes.frame() {
        out.write_all(&nes::palette::rgb(pixel)).unwrap();
    }
}
