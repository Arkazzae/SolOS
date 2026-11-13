//! Play Mega Drive cartridges in an SDL2 window with keyboard, pads and PCM.
#[path = "support/desktop_input.rs"]
mod desktop_input;
#[path = "support/desktop_saves.rs"]
mod desktop_saves;
#[path = "support/desktop_sdl.rs"]
#[allow(dead_code)]
mod desktop_sdl;

use std::{
    error::Error,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};

const USAGE: &str = "usage: play-md ROM [--scale 1..6] [--save-dir DIR] [--fresh] [--no-audio] [--fullscreen] [--sdl2-library PATH] [--frames N] [--host-seconds N] [--frame OUT.ppm] [--run-unfocused]";
const CONTROLS: &str = "P1: arrows; Z/X/C = A/B/C; A/S/D = X/Y/Z; Enter = Start; Right Shift = Mode\nP2: keypad 8/2/4/6; 7/9/0 = A/B/C; 1/3/decimal = X/Y/Z; Enter = Start; + = Mode\nGamepads: X/A/B = A/B/C, LB/Y/RB = X/Y/Z, Menu = Start, View = Mode\nSpace pause; F2 reset; F5 save; F8 load; F11 fullscreen; Escape quit\nFocus loss pauses and releases keys. SRAM and Quick Resume are saved on exit.";
struct Options {
    source: PathBuf,
    save_dir: PathBuf,
    library: Option<PathBuf>,
    scale: u32,
    audio: bool,
    resume: bool,
    fullscreen: bool,
    run_unfocused: bool,
    frames: Option<u64>,
    seconds: Option<Duration>,
    picture: Option<PathBuf>,
}
impl Options {
    fn parse() -> Result<Option<Self>, Box<dyn Error>> {
        let mut args = std::env::args_os().skip(1);
        let mut o = Self {
            source: PathBuf::new(),
            save_dir: desktop_saves::default_directory(),
            library: None,
            scale: 3,
            audio: true,
            resume: true,
            fullscreen: false,
            run_unfocused: false,
            frames: None,
            seconds: None,
            picture: None,
        };
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or("missing option value");
            match arg.to_str() {
                Some("--help" | "-h") => {
                    println!("{USAGE}\n{CONTROLS}");
                    return Ok(None);
                }
                Some("--save-dir") => o.save_dir = value()?.into(),
                Some("--sdl2-library") => o.library = Some(value()?.into()),
                Some("--frame") => o.picture = Some(value()?.into()),
                Some("--scale") => {
                    o.scale = value()?.to_str().ok_or("invalid scale")?.parse()?;
                    if !(1..=6).contains(&o.scale) {
                        return Err("scale must be 1..6".into());
                    }
                }
                Some("--frames") => {
                    let n = value()?.to_str().ok_or("invalid frame count")?.parse()?;
                    if n == 0 {
                        return Err("frame count must be positive".into());
                    }
                    o.frames = Some(n);
                }
                Some("--host-seconds") => {
                    let n: f64 = value()?.to_str().ok_or("invalid host time")?.parse()?;
                    if !n.is_finite() || !(0.001..=86400.).contains(&n) {
                        return Err("host time must be 0.001..86400 seconds".into());
                    }
                    o.seconds = Some(Duration::from_secs_f64(n));
                }
                Some("--fresh") => o.resume = false,
                Some("--no-audio") => o.audio = false,
                Some("--fullscreen") => o.fullscreen = true,
                Some("--run-unfocused") => o.run_unfocused = true,
                Some(s) if s.starts_with('-') => {
                    return Err(format!("unknown option {s}\n{USAGE}").into());
                }
                _ if o.source.as_os_str().is_empty() => o.source = arg.into(),
                _ => return Err(USAGE.into()),
            }
        }
        if o.source.as_os_str().is_empty() {
            return Err(USAGE.into());
        }
        Ok(Some(o))
    }
}

fn canvas(md: &md::Md, pixels: &mut [u16]) {
    pixels.fill(0);
    let (w, h) = md.dimensions();
    let (left, top) = ((md::WIDTH - w) / 2, (md::HEIGHT - h) / 2);
    for (y, row) in md.frame().chunks_exact(w).take(h).enumerate() {
        let start = (top + y) * md::WIDTH + left;
        pixels[start..start + w].copy_from_slice(row);
    }
}
fn picture(path: &Path, md: &md::Md) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    let (w, h) = md.dimensions();
    write!(file, "P6\n{w} {h}\n255\n")?;
    for &p in md.frame() {
        let (r, g, b) = ((p >> 11) as u8, ((p >> 5) & 63) as u8, (p & 31) as u8);
        file.write_all(&[
            (r << 3) | (r >> 2),
            (g << 2) | (g >> 4),
            (b << 3) | (b >> 2),
        ])?;
    }
    Ok(())
}
fn run() -> Result<(), Box<dyn Error>> {
    let Some(o) = Options::parse()? else {
        return Ok(());
    };
    let source = fs::canonicalize(&o.source)?;
    if let Some(path) = &o.picture
        && path.exists()
        && fs::canonicalize(path)? == source
    {
        return Err("a screenshot cannot overwrite the cartridge".into());
    }
    let rom = fs::read(&source)?;
    let mut md = md::Md::new(&rom)?;
    let saves = desktop_saves::Saves::new(
        &o.save_dir,
        &rom,
        source
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .as_ref(),
    )?;
    saves.restore(&mut md, o.resume)?;
    let mut window = desktop_sdl::Window::open(o.library.as_deref(), o.scale, o.audio)?;
    if o.fullscreen {
        window.toggle_fullscreen()?;
    }
    let mut keyboard = desktop_input::Keyboard::default();
    let mut pacer = desktop_input::Pacer::new(md.region().pal());
    let mut pixels = vec![0; md::WIDTH * md::HEIGHT];
    let started = Instant::now();
    let mut report = started;
    let mut save_at = started;
    let (mut frames, mut report_frames, mut core_time, mut peak_audio) =
        (0u64, 0u64, Duration::ZERO, 0usize);
    let mut focused = window.focused();
    let mut paused = false;
    let mut was_paused = true;
    let mut audio_started = false;
    eprintln!(
        "{} ({:?})\n{CONTROLS}\nQuick Resume: {}",
        md.title(),
        md.region(),
        saves.state.display()
    );
    let result: Result<(), Box<dyn Error>> = (|| {
        'running: loop {
            while let Some(event) = window.poll() {
                match event {
                    desktop_sdl::Event::Quit => break 'running,
                    desktop_sdl::Event::Focus(value) => {
                        focused = value;
                        keyboard.clear();
                    }
                    desktop_sdl::Event::Key {
                        scancode,
                        pressed: true,
                        repeat: false,
                    } => match scancode {
                        41 => break 'running,
                        44 => {
                            paused = !paused;
                            keyboard.clear();
                        }
                        59 => {
                            md.reset();
                            keyboard.clear();
                            window.clear_audio();
                            audio_started = false;
                            pacer.rebase();
                        }
                        62 => {
                            saves.save(&md)?;
                            eprintln!("Saved Quick Resume");
                        }
                        65 => {
                            match saves.load(&mut md) {
                                Ok(()) => eprintln!("Loaded Quick Resume"),
                                Err(e) => eprintln!("Load: {e}"),
                            }
                            keyboard.clear();
                            window.clear_audio();
                            audio_started = false;
                            pacer.rebase();
                        }
                        68 => window.toggle_fullscreen()?,
                        _ => keyboard.key(scancode, true, false),
                    },
                    desktop_sdl::Event::Key {
                        scancode,
                        pressed,
                        repeat,
                    } => keyboard.key(scancode, pressed, repeat),
                }
            }
            if o.seconds.is_some_and(|limit| started.elapsed() >= limit)
                || o.frames.is_some_and(|limit| frames >= limit)
            {
                break;
            }
            let stopped = paused || (!focused && !o.run_unfocused);
            if stopped != was_paused {
                window.pause_audio(true);
                window.clear_audio();
                audio_started = false;
                pacer.rebase();
                was_paused = stopped;
            }
            if stopped {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            let keys = keyboard.take_frame();
            let pads = window.controllers();
            let t = Instant::now();
            md.run_frame([
                desktop_input::clean_directions(keys[0] | pads[0]),
                desktop_input::clean_directions(keys[1] | pads[1]),
            ]);
            core_time += t.elapsed();
            canvas(&md, &mut pixels);
            window.render(&pixels)?;
            if o.audio {
                window.audio(md.audio())?;
                peak_audio = peak_audio.max(window.queued_frames());
                if !audio_started && window.queued_frames() >= 1600 {
                    window.pause_audio(false);
                    audio_started = true;
                }
            }
            frames += 1;
            if save_at.elapsed() >= Duration::from_secs(30) {
                saves.ram(&md)?;
                save_at = Instant::now();
            }
            if report.elapsed() >= Duration::from_secs(2) {
                let fps = (frames - report_frames) as f64 / report.elapsed().as_secs_f64();
                let ms = core_time.as_secs_f64() * 1000. / (frames - report_frames) as f64;
                window.title(&format!(
                    "Sol Mega Drive — {} — {fps:.1} fps — core {ms:.2} ms",
                    md.title()
                ));
                eprintln!(
                    "{fps:.1} fps, core {ms:.2} ms/frame, audio queue {} ms",
                    window.queued_frames() * 1000 / 48000
                );
                report = Instant::now();
                report_frames = frames;
                core_time = Duration::ZERO;
            }
            let next = pacer.next();
            if let Some(remaining) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(remaining);
            }
        }
        if let Some(path) = &o.picture {
            picture(path, &md)?;
        }
        Ok(())
    })();
    window.pause_audio(true);
    let saved = saves.save(&md);
    eprintln!(
        "{frames} frames in {:.3}s; peak queued audio {:.1}ms; {}",
        started.elapsed().as_secs_f64(),
        peak_audio as f64 / 48.,
        md.debug()
    );
    result?;
    saved?;
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("play-md: {e}");
            ExitCode::FAILURE
        }
    }
}
