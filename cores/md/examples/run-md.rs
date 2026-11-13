//! Headless ROM runner: RGB565 to PPM, stereo PCM to WAV, pads and saves.
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const USAGE: &str = "usage: run-md ROM FRAMES OUT.ppm [EVERY] [--press [2:]BUTTON@FRAME[:DURATION]] [--wav OUT.wav] [--save SRAM] [--load-state STATE] [--save-state STATE] [--warmup FRAMES]";

struct Press {
    port: usize,
    button: u16,
    first: u32,
    end: u32,
}

fn press(value: &str) -> Result<Press, Box<dyn std::error::Error>> {
    let (name, at) = value
        .split_once('@')
        .ok_or("expected BUTTON@FRAME[:DURATION]")?;
    let (port, name) = name.strip_prefix("2:").map_or((0, name), |name| (1, name));
    let button = match name.to_ascii_lowercase().as_str() {
        "a" => md::A,
        "b" => md::B,
        "c" => md::C,
        "x" => md::X,
        "y" => md::Y,
        "z" => md::Z,
        "start" => md::START,
        "mode" => md::MODE,
        "up" => md::UP,
        "down" => md::DOWN,
        "left" => md::LEFT,
        "right" => md::RIGHT,
        _ => return Err("unknown button".into()),
    };
    let (first, duration) = at.split_once(':').unwrap_or((at, "6"));
    let first: u32 = first.parse()?;
    let duration: u32 = duration.parse()?;
    if first == 0 || duration == 0 {
        return Err("press frame and duration must be positive".into());
    }
    Ok(Press {
        port,
        button,
        first,
        end: first
            .checked_add(duration)
            .ok_or("press interval overflow")?,
    })
}

fn destination(path: &Path) -> std::io::Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path);
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(fs::canonicalize(parent)?.join(
        path.file_name()
            .ok_or_else(|| std::io::Error::other("missing filename"))?,
    ))
}

fn ppm(path: &Path, md: &md::Md) -> std::io::Result<()> {
    let (w, h) = md.dimensions();
    let mut bytes = format!("P6\n{w} {h}\n255\n").into_bytes();
    for &p in md.frame() {
        let (r, g, b) = ((p >> 11) as u8, ((p >> 5) & 63) as u8, (p & 31) as u8);
        bytes.extend_from_slice(&[
            (r << 3) | (r >> 2),
            (g << 2) | (g >> 4),
            (b << 3) | (b >> 2),
        ]);
    }
    fs::write(path, bytes)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut positional = Vec::new();
    let (mut wav, mut save, mut load_state, mut save_state) = (None, None, None, None);
    let mut presses = Vec::new();
    let mut warmup = 0u32;
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or("missing option value");
        match arg.as_str() {
            "--wav" => wav = Some(PathBuf::from(value()?)),
            "--save" => save = Some(PathBuf::from(value()?)),
            "--load-state" => load_state = Some(PathBuf::from(value()?)),
            "--save-state" => save_state = Some(PathBuf::from(value()?)),
            "--press" => presses.push(press(&value()?)?),
            "--warmup" => warmup = value()?.parse()?,
            _ if arg.starts_with('-') => {
                return Err(format!("unknown option {arg}\n{USAGE}").into());
            }
            _ => positional.push(arg),
        }
    }
    if !(3..=4).contains(&positional.len()) {
        return Err(USAGE.into());
    }
    let frames: u32 = positional[1].parse()?;
    let every = positional.get(3).map(|s| s.parse::<u32>()).transpose()?;
    if frames == 0 || every == Some(0) || warmup >= frames {
        return Err("invalid frame count or warmup".into());
    }
    let picture = PathBuf::from(&positional[2]);
    let source = fs::canonicalize(&positional[0])?;
    let input_state = load_state.as_ref().map(fs::canonicalize).transpose()?;
    let mut outputs = Vec::new();
    for path in [
        Some(&picture),
        wav.as_ref(),
        save.as_ref(),
        save_state.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let resolved = destination(path)?;
        if resolved == source
            || input_state.as_ref() == Some(&resolved)
            || outputs.contains(&resolved)
        {
            return Err("output paths must be distinct from inputs and each other".into());
        }
        outputs.push(resolved);
    }
    // Check interval screenshots before any output is written, too.
    let mut snapshots = Vec::new();
    if let Some(every) = every {
        for frame in (every..=frames).step_by(every as usize) {
            let stem = picture
                .file_stem()
                .ok_or("missing picture filename")?
                .to_string_lossy();
            let path = picture.with_file_name(format!("{stem}-{frame:06}.ppm"));
            let resolved = destination(&path)?;
            if resolved == source
                || input_state.as_ref() == Some(&resolved)
                || outputs.contains(&resolved)
            {
                return Err("snapshot path collides with an input or output".into());
            }
            snapshots.push((frame, path));
        }
    }
    let mut md = md::Md::new(&fs::read(source)?)?;
    if let Some(path) = &save {
        if md.save_data().is_none() {
            return Err("cartridge has no parallel save RAM".into());
        }
        match fs::read(path) {
            Ok(bytes) => md.load_save_data(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if let Some(path) = load_state {
        md.load_state(&fs::read(path)?)?;
    }
    eprintln!("{} ({:?})", md.title(), md.region());
    let mut started = std::time::Instant::now();
    let mut audio = Vec::new();
    for frame in 1..=frames {
        if frame == warmup + 1 {
            started = std::time::Instant::now();
        }
        let mut pads = [0; 2];
        for p in &presses {
            if (p.first..p.end).contains(&frame) {
                pads[p.port] |= p.button;
            }
        }
        md.run_frame(pads);
        if wav.is_some() {
            audio.extend_from_slice(md.audio());
        }
        if let Some((_, path)) = snapshots.iter().find(|(n, _)| *n == frame) {
            ppm(path, &md)?;
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    eprintln!(
        "{:.1} fps; {}",
        f64::from(frames - warmup) / seconds,
        md.debug()
    );
    ppm(&picture, &md)?;
    if let Some(path) = save {
        fs::write(path, md.save_data().unwrap())?;
    }
    if let Some(path) = save_state {
        fs::write(path, md.save_state())?;
    }
    if let Some(path) = wav {
        let size = u32::try_from(audio.len().checked_mul(2).ok_or("WAV too large")?)?;
        let mut file = fs::File::create(path)?;
        file.write_all(b"RIFF")?;
        file.write_all(&size.checked_add(36).ok_or("WAV too large")?.to_le_bytes())?;
        file.write_all(b"WAVEfmt \x10\0\0\0\x01\0\x02\0")?;
        file.write_all(&md::SAMPLE_HZ.to_le_bytes())?;
        file.write_all(&(md::SAMPLE_HZ * 4).to_le_bytes())?;
        file.write_all(b"\x04\0\x10\0data")?;
        file.write_all(&size.to_le_bytes())?;
        for sample in audio {
            file.write_all(&sample.to_le_bytes())?;
        }
    }
    Ok(())
}
