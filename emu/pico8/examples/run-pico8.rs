//! Headless host: cartridge → frames, controller input, PPM and optional WAV
//! (one channel).
use std::io::{BufWriter, Write};

const DEMO: &[u8] = include_bytes!("carts/orbit.p8");

fn main() {
    if let Err(error) = run() {
        eprintln!("run-pico8: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    if std::env::args().nth(1).as_deref() == Some("--label") {
        // The cartridge's label, its cover, as a PPM.
        let (Some(path), Some(out)) = (std::env::args().nth(2), std::env::args().nth(3)) else {
            return Err("usage: run-pico8 --label CART OUT.ppm".into());
        };
        let label = pico8::label(&std::fs::read(path)?).ok_or("no label")?;
        let mut image = BufWriter::new(std::fs::File::create(out)?);
        write!(image, "P6\n128 128\n255\n")?;
        for &c in label.iter() {
            let rgb = pico8::PALETTE[usize::from(c & 31)];
            image.write_all(&[(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8])?;
        }
        image.flush()?;
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("--code") {
        // The cartridge's code, as P8SCII, to read what a game does.
        let path = std::env::args().nth(2).ok_or("usage: run-pico8 --code CART")?;
        let cart = pico8::Cartridge::parse(&std::fs::read(path)?).map_err(|e| e.to_string())?;
        std::io::stdout().write_all(&cart.code)?;
        return Ok(());
    }
    let source = args.next().ok_or("usage: run-pico8 CART.p8|CART.p8.png|--demo FRAMES OUT.ppm [--wav OUT.wav] [--press BUTTON@FRAME[:DURATION]]")?;
    let frames: u32 = args.next().ok_or("missing FRAMES")?.parse()?;
    if frames == 0 {
        return Err("FRAMES must be positive".into());
    }
    let output = args.next().ok_or("missing OUT.ppm")?;
    let (mut wav, mut presses, mut check_state, mut mash) = (None, Vec::new(), None, None);
    let mut draw_every = 1u32;
    while let Some(option) = args.next() {
        let value = args.next().ok_or("missing option value")?;
        match option.as_str() {
            "--wav" => wav = Some(value),
            // Buttons pressed at random (the same for a seed): a few at a
            // time, changing every 4 to 19 frames.
            "--mash" => mash = Some(value.parse::<u32>()?.max(1)),
            // _draw only every Nth frame, as a host behind time runs it.
            "--draw-every" => draw_every = value.parse::<u32>()?.max(1),
            // Saves the state after frame N, and at the end plays the
            // frames after it again from the state, which must match.
            "--check-state" => check_state = Some(value.parse::<u32>()?),
            "--press" => {
                let (button, at) = value
                    .split_once('@')
                    .ok_or("--press expects BUTTON@FRAME[:DURATION]")?;
                let (at, duration) = at.split_once(':').unwrap_or((at, "6"));
                let at = at.parse::<u32>()?;
                let duration = duration.parse::<u32>()?;
                let bit = match button {
                    "left" => pico8::LEFT,
                    "right" => pico8::RIGHT,
                    "up" => pico8::UP,
                    "down" => pico8::DOWN,
                    "o" => pico8::O,
                    "x" => pico8::X,
                    "pause" => pico8::PAUSE,
                    _ => return Err(format!("unknown button: {button}").into()),
                };
                presses.push((bit, at, at.saturating_add(duration)));
            }
            _ => return Err(format!("unknown option: {option}").into()),
        }
    }
    let data = if source == "--demo" {
        DEMO.to_vec()
    } else {
        std::fs::read(&source)?
    };
    let mut console = pico8::Pico8::new(&data)?;
    let mut sound = Vec::new();
    let (mut saved, mut after) = (None, Vec::new());
    let started = std::time::Instant::now();
    let (mut seed, mut mashed, mut until) = (mash.unwrap_or(1), 0u8, 0u32);
    for frame in 1..=frames {
        let mut buttons = [0; 8];
        if mash.is_some() {
            if frame >= until {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                mashed = (seed & 0x3f) as u8 & (seed >> 6) as u8;
                until = frame + 4 + (seed >> 12) % 16;
            }
            buttons[0] = mashed;
        }
        for &(bit, from, to) in &presses {
            if (from..to).contains(&frame) {
                buttons[0] |= bit;
            }
        }
        if frame % draw_every == 0 {
            console.run_frame(buttons)
        } else {
            console.run_frame_without_draw(buttons)
        }
        .map_err(|e| format!("frame {frame}: {e}"))?;
        if check_state == Some(frame) {
            saved = Some(console.save_state());
        }
        if check_state.is_some_and(|n| frame > n) {
            after.push((buttons, console.frame().to_vec(), console.audio().to_vec()));
        }
        if wav.is_some() {
            sound.extend_from_slice(console.audio());
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    if let Some(state) = saved {
        let mut again = pico8::Pico8::new(&data)?;
        again.load_state(&state)?;
        for (i, (buttons, picture, audio)) in after.iter().enumerate() {
            again.run_frame(*buttons).map_err(|e| format!("from the state, frame {i}: {e}"))?;
            if again.frame()[..] != picture[..] || again.audio() != &audio[..] {
                return Err(format!("from the state, frame {} differs", i + 1).into());
            }
        }
        eprintln!("state of {} bytes plays the same {} frames", state.len(), after.len());
    }
    let mut image = BufWriter::new(std::fs::File::create(output)?);
    write!(image, "P6\n{} {}\n255\n", pico8::WIDTH, pico8::HEIGHT)?;
    for &color in console.frame() {
        let rgb = pico8::PALETTE[color as usize];
        image.write_all(&[(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8])?;
    }
    image.flush()?;
    if let Some(path) = wav {
        save_wav(&sound, &path)?;
    }
    eprintln!(
        "{frames} frames, {} Hz mode, {elapsed:.3}s ({:.0} fps)",
        console.fps(),
        frames as f64 / elapsed
    );
    Ok(())
}

fn save_wav(samples: &[i16], path: &str) -> std::io::Result<()> {
    let bytes = u32::try_from(samples.len().saturating_mul(2))
        .ok()
        .and_then(|n| n.checked_add(36))
        .ok_or_else(|| std::io::Error::other("WAV exceeds RIFF size limit"))?
        - 36;
    let mut out = BufWriter::new(std::fs::File::create(path)?);
    let rate = pico8::SAMPLE_HZ as u32;
    out.write_all(b"RIFF")?;
    out.write_all(&(36 + bytes).to_le_bytes())?;
    out.write_all(b"WAVEfmt ")?;
    for field in [16u32, 1 | 1 << 16, rate, rate * 2, 2 | 16 << 16] {
        out.write_all(&field.to_le_bytes())?;
    }
    out.write_all(b"data")?;
    out.write_all(&bytes.to_le_bytes())?;
    for sample in samples {
        out.write_all(&sample.to_le_bytes())?;
    }
    out.flush()
}
