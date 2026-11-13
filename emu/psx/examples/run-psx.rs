//! Runs a PS-X EXE headless, to look at the core's output on a PC.
//!
//!   cargo run --release --example run-psx -- EXE|DISC.chd FRAMES [--vram VRAM.ppm] [--picture OUT.ppm]
//!   cargo run --release --example run-psx -- --bios BIOS.BIN DISC.chd|- FRAMES [...]
//!
//! Runs a PS-X EXE, or the program on a disc, with the core's own BIOS, or
//! with --bios starts that BIOS, with the disc in the drive (- for none).
//! Prints what the program wrote to the TTY, then the BIOS calls
//! that are not answered and the CPU's registers; --vram saves all of
//! VRAM, --picture what the GPU shows, --wav the sound of all the
//! frames, and --card the memory card in slot 1 as it was left; --press
//! BUTTON@FRAME holds a button of the pad (select, start, up, right, down,
//! left, l1, r1, l2, r2, triangle, circle, cross, square) for a tenth of
//! a second from a frame on, PLAYER:BUTTON@FRAME another player's (2 to 4:
//! the second port's pad, or with --multitap PORT a Multitap's slots in
//! port 1 or 2, four pads in it); --from FRAME times only the
//! frames after that one, each one's picture taken as a frontend would;
//! --no-jit runs the interpreter alone, --no-gpu-thread draws on the
//! CPU's thread; --hashes prints a hash of each frame's picture, to
//! compare runs; --counters (on the console's ARM) reads its CPU's
//! counters for each thread over the timed frames, and --samples OUT
//! samples where each thread spends its cycles, into OUT as lines of
//! thread, code address and samples (for llvm-symbolizer); --state FILE
//! starts from a save state (the game host's quick resume, qr, is one),
//! --save FILE writes one when the frames are run, and --pictures DIR
//! writes each frame's picture there as NNNNN.ppm.

use std::io::Write;

fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    let mut flag = |name: &str| args.iter().position(|a| a == name).map(|i| args.remove(i)).is_some();
    let (no_jit, no_gpu_thread, hashes) = (flag("--no-jit"), flag("--no-gpu-thread"), flag("--hashes"));
    #[cfg_attr(not(target_arch = "arm"), allow(unused_variables))]
    let counting = flag("--counters");
    let mut option = |name: &str| {
        let i = args.iter().position(|a| a == name)?;
        let path = args.get(i + 1).unwrap_or_else(|| panic!("{name} takes a file")).clone();
        args.drain(i..i + 2);
        Some(path)
    };
    let (vram, picture, bios) = (option("--vram"), option("--picture"), option("--bios"));
    #[cfg_attr(not(target_arch = "arm"), allow(unused_variables))]
    let samples = option("--samples");
    let wav = option("--wav");
    let card = option("--card");
    let (state, save_to, pictures) = (option("--state"), option("--save"), option("--pictures"));
    let from: u32 = option("--from").map_or(0, |f| f.parse().expect("--from takes a frame"));
    let tap: Option<usize> = option("--multitap").map(|p| p.parse::<usize>().expect("--multitap takes 1 or 2") - 1);
    let mut presses = Vec::new();
    while let Some(press) = option("--press") {
        let (player, press) = match press.split_once(':') {
            Some((p, rest)) => (p.parse::<usize>().expect("PLAYER is 1 to 4") - 1, rest.to_owned()),
            None => (0, press),
        };
        let (name, frame) = press.split_once('@').expect("--press takes BUTTON@FRAME");
        use psx::buttons::*;
        let bit = match name {
            "select" => SELECT,
            "start" => START,
            "up" => UP,
            "right" => RIGHT,
            "down" => DOWN,
            "left" => LEFT,
            "l1" => L1,
            "r1" => R1,
            "l2" => L2,
            "r2" => R2,
            "triangle" => TRIANGLE,
            "circle" => CIRCLE,
            "cross" => CROSS,
            "square" => SQUARE,
            _ => panic!("no button {name}"),
        };
        presses.push((player, bit, frame.parse::<u32>().expect("FRAME is a number")));
    }
    if args.len() < 3 {
        eprintln!("usage: run-psx [--bios BIOS.BIN] EXE|DISC.chd|- FRAMES [--vram VRAM.ppm] [--picture OUT.ppm] [--wav OUT.wav]");
        std::process::exit(2);
    }
    let frames: u32 = args[2].parse().expect("FRAMES is a number");
    let chd = |path: &str| {
        let disc = disc::ChdDisc::open(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        eprintln!("{} tracks, {} sectors", disc.tracks().len(), disc.end());
        Chd(disc)
    };
    let mut psx = match &bios {
        Some(path) => psx::Psx::new(&std::fs::read(path).expect("cannot read the BIOS")),
        None if args[1].ends_with(".chd") => psx::Psx::with_disc(chd(&args[1])),
        None => psx::Psx::with_exe(&std::fs::read(&args[1]).expect("cannot read the EXE")),
    }
    .unwrap_or_else(|e| panic!("{}: {e}", args[1]));
    if no_jit {
        psx.set_jit(false);
    }
    if no_gpu_thread {
        psx.set_gpu_thread(false);
    }
    if bios.is_some() && args[1] != "-" {
        psx.insert_disc(chd(&args[1]));
    }
    if let Some(path) = &state {
        psx.load_state(&std::fs::read(path).expect("cannot read the state")).unwrap_or_else(|e| panic!("{path}: {e}"));
    }
    if let Some(dir) = &pictures {
        std::fs::create_dir_all(dir).expect("cannot make the pictures' directory");
    }
    let mut started = std::time::Instant::now();
    let mut busy = Vec::new();
    #[cfg(target_arch = "arm")]
    let (mut counters, mut samplers) = (None, Vec::new());
    // Time in picture(): with the GPU on its thread, waiting for it.
    let mut showing = std::time::Duration::ZERO;
    let mut sound = Vec::new();
    match tap {
        Some(port) => psx.set_multitap(Some(port), 4),
        None => psx.set_second_pad(presses.iter().any(|&(player, ..)| player == 1)),
    }
    for frame in 1..=frames {
        for player in 0..4 {
            let pressed = presses.iter().filter(|&&(p, _, at)| p == player && (at..at + 6).contains(&frame));
            psx.set_player(player, pressed.fold(0, |b, &(_, bit, _)| b | bit));
        }
        if frame == from + 1 {
            started = std::time::Instant::now();
            busy = thread_times();
            #[cfg(target_arch = "arm")]
            if counting {
                counters = Some(counters::start());
            }
            #[cfg(target_arch = "arm")]
            if samples.is_some() {
                samplers = counters::samplers();
            }
        }
        psx.run_frame();
        #[cfg(target_arch = "arm")]
        samplers.iter_mut().for_each(counters::Sampler::drain);
        if frame > from || hashes || pictures.is_some() {
            let asked = std::time::Instant::now();
            let (width, height, rgb) = psx.picture();
            if frame > from {
                showing += asked.elapsed();
            }
            if hashes {
                let fnv = rgb.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
                println!("{frame} {width}x{height} {fnv:016x}");
            }
            if let Some(dir) = &pictures {
                save(&format!("{dir}/{frame:05}.ppm"), width, height, rgb);
            }
        }
        if wav.is_some() {
            sound.extend_from_slice(psx.audio());
        }
    }
    let elapsed = started.elapsed();
    std::io::stdout().write_all(psx.tty()).unwrap();
    let timed = frames.saturating_sub(from);
    let (seconds, fps) = (elapsed.as_secs_f64(), timed as f64 / elapsed.as_secs_f64());
    eprintln!("{timed} frames in {seconds:.2} s ({fps:.0} fps), {:.2} s of it taking pictures", showing.as_secs_f64());
    for (name, end) in thread_times() {
        let start = busy.iter().find(|(n, _)| *n == name).map_or(0.0, |&(_, t)| t);
        eprintln!("thread {name}: {:.2} s on the CPU", end - start);
    }
    #[cfg(target_arch = "arm")]
    if let Some(c) = counters {
        counters::report(&c, timed);
    }
    #[cfg(target_arch = "arm")]
    if let Some(path) = &samples {
        let mut out = String::new();
        for s in &samplers {
            let mut seen: Vec<_> = s.seen.iter().collect();
            seen.sort_by(|a, b| b.1.cmp(a.1));
            for (ip, n) in seen {
                out += &format!("{} {ip:#x} {n}\n", s.name);
            }
        }
        // Compiled blocks, with the instructions they start with.
        for (code, pc) in psx.jit_blocks() {
            let words: Vec<String> = (0..16).map(|i| format!("{:08x}", psx.peek32(pc + 4 * i))).collect();
            out += &format!("block {code:#x} {pc:#x} {}\n", words.join(" "));
        }
        std::fs::write(path, out).expect("cannot write the samples");
    }
    for &(table, function) in psx.unknown_calls() {
        eprintln!("not answered: {table:x}({function:x}h)");
    }
    eprintln!("{:08x?}", psx.registers());
    if let Some(path) = vram {
        let rgb: Vec<u8> = psx.vram().iter().flat_map(|&p| [p, p >> 5, p >> 10].map(|c| ((c & 31) << 3 | (c & 31) >> 2) as u8)).collect();
        save(&path, psx::VRAM_WIDTH, psx::VRAM_HEIGHT, &rgb);
    }
    if let Some(path) = save_to {
        std::fs::write(path, psx.save_state()).expect("cannot write the state");
    }
    if let Some(path) = card {
        std::fs::write(path, psx.memory_card(0).expect("no card in slot 1")).expect("cannot write the card");
    }
    if let Some(path) = wav {
        save_wav(&path, &sound);
        let peak = sound.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        eprintln!("{} samples, peak {peak}", sound.len() / 2);
    }
    if let Some(path) = picture {
        let (width, height, rgb) = psx.picture();
        save(&path, width, height, rgb);
    }
}

/// Stereo 16-bit PCM at 44100 Hz.
fn save_wav(path: &str, samples: &[i16]) {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).expect("cannot create the sound"));
    let bytes = (samples.len() * 2) as u32;
    out.write_all(b"RIFF").unwrap();
    out.write_all(&(36 + bytes).to_le_bytes()).unwrap();
    out.write_all(b"WAVEfmt ").unwrap();
    for field in [16u32, 1 | 2 << 16, 44100, 44100 * 4, 4 | 16 << 16] {
        out.write_all(&field.to_le_bytes()).unwrap();
    }
    out.write_all(b"data").unwrap();
    out.write_all(&bytes.to_le_bytes()).unwrap();
    for s in samples {
        out.write_all(&s.to_le_bytes()).unwrap();
    }
}

fn save(path: &str, width: usize, height: usize, rgb: &[u8]) {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).expect("cannot create the picture"));
    write!(out, "P6\n{width} {height}\n255\n").unwrap();
    out.write_all(rgb).unwrap();
}

/// A CHD image as the drive's disc.
struct Chd(disc::ChdDisc);

impl psx::Disc for Chd {
    fn read(&mut self, lba: u32, sector: &mut [u8; 2352]) -> bool {
        self.0.read(lba, sector)
    }

    fn tracks(&self) -> Vec<psx::Track> {
        self.0.tracks().iter().map(|t| psx::Track { start: t.start, audio: t.audio }).collect()
    }

    fn end(&self) -> u32 {
        self.0.end()
    }
}

/// Each thread's name and the seconds it has run on the CPU (from Linux's
/// /proc; none elsewhere).
fn thread_times() -> Vec<(String, f64)> {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else { return Vec::new() };
    let mut times = Vec::new();
    for task in tasks.flatten() {
        let (Ok(name), Ok(stat)) =
            (std::fs::read_to_string(task.path().join("comm")), std::fs::read_to_string(task.path().join("stat")))
        else {
            continue;
        };
        // After the name in parentheses: state, then 11 fields to utime and
        // stime, in clock ticks (100 a second).
        let fields: Vec<&str> = stat.rsplit_once(')').map_or("", |(_, rest)| rest).split_whitespace().collect();
        let ticks = |i: usize| fields.get(i).and_then(|f| f.parse::<f64>().ok()).unwrap_or(0.0);
        times.push((name.trim().to_string(), (ticks(11) + ticks(12)) / 100.0));
    }
    times
}

/// The ARM CPU's own counters, for each thread: through Linux's
/// perf_event_open, by raw event numbers of the ARMv7/v8 PMU.
#[cfg(target_arch = "arm")]
mod counters {
    const EVENTS: [(&str, u64); 6] = [
        ("cycles", 0x11),
        ("instructions", 0x08),
        ("L1I refills", 0x01),
        ("L1D refills", 0x03),
        ("L2D refills", 0x17),
        ("mispredicted branches", 0x10),
    ];

    /// Each thread's name and the counters opened on it.
    pub struct Counters(Vec<(String, Vec<i32>)>);

    fn syscall(number: usize, args: [usize; 6]) -> isize {
        let ret: isize;
        unsafe {
            core::arch::asm!("svc #0",
                in("r7") number,
                inlateout("r0") args[0] => ret,
                in("r1") args[1],
                in("r2") args[2],
                in("r3") args[3],
                in("r4") args[4],
                in("r5") args[5],
            );
        }
        ret
    }

    /// An event counted in user code of one thread, from now on.
    fn open(tid: usize, event: u64) -> Option<i32> {
        // perf_event_attr: type PERF_TYPE_RAW (4) and size (112), the
        // event, the kernel and hypervisor left out.
        let mut attr = [0u64; 14];
        attr[0] = 4 | 112 << 32;
        attr[1] = event;
        attr[5] = 1 << 5 | 1 << 6;
        let fd = syscall(364, [attr.as_ptr() as usize, tid, usize::MAX, usize::MAX, 0, 0]);
        (fd >= 0).then_some(fd as i32)
    }

    /// The threads there are: their ids and names.
    fn threads() -> Vec<(usize, String)> {
        let mut threads = Vec::new();
        for task in std::fs::read_dir("/proc/self/task").into_iter().flatten().flatten() {
            let Some(tid) = task.file_name().to_str().and_then(|t| t.parse().ok()) else { continue };
            let name = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default().trim().to_string();
            threads.push((tid, name));
        }
        threads
    }

    /// The pages of a sampler's ring buffer (a power of two).
    const PAGES: usize = 64;

    /// A thread's cycles sampled every 20000: the kernel writes where its
    /// code was into a ring buffer, emptied into counts by address.
    pub struct Sampler {
        pub name: String,
        ring: *mut u8,
        pub seen: std::collections::HashMap<u32, u32>,
    }

    pub fn samplers() -> Vec<Sampler> {
        let mut samplers = Vec::new();
        for (tid, name) in threads() {
            // Cycles (raw 0x11), a sample period, PERF_SAMPLE_IP.
            let mut attr = [0u64; 14];
            attr[0] = 4 | 112 << 32;
            attr[1] = 0x11;
            attr[2] = 20_000;
            attr[3] = 1;
            attr[5] = 1 << 5 | 1 << 6;
            let fd = syscall(364, [attr.as_ptr() as usize, tid, usize::MAX, usize::MAX, 0, 0]);
            if fd < 0 {
                continue;
            }
            // mmap2: the control page, then the buffer; shared, read and write.
            let ring = syscall(192, [0, (PAGES + 1) * 4096, 3, 1, fd as usize, 0]);
            if (-4096..0).contains(&ring) {
                continue;
            }
            samplers.push(Sampler { name, ring: ring as *mut u8, seen: Default::default() });
        }
        samplers
    }

    impl Sampler {
        /// Counts the samples written since, and gives their room back.
        pub fn drain(&mut self) {
            let size = (PAGES * 4096) as u64;
            unsafe {
                // data_head and data_tail, in the control page.
                let head = std::ptr::read_volatile(self.ring.add(1024) as *const u64);
                std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
                let mut tail = std::ptr::read_volatile(self.ring.add(1032) as *const u64);
                let data = self.ring.add(4096);
                let byte = |at: u64| *data.add((at % size) as usize);
                let word = |at: u64| u32::from_le_bytes(std::array::from_fn(|i| byte(at + i as u64)));
                while tail < head {
                    // The header: type, misc and size; a sample's address follows.
                    let (kind, len) = (word(tail), word(tail + 4) >> 16);
                    if kind == 9 {
                        *self.seen.entry(word(tail + 8)).or_default() += 1;
                    }
                    tail += u64::from(len.max(8));
                }
                std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                std::ptr::write_volatile(self.ring.add(1032) as *mut u64, tail);
            }
        }
    }

    pub fn start() -> Counters {
        let opened = |tid: usize| EVENTS.iter().filter_map(|&(_, e)| open(tid, e)).collect();
        Counters(threads().into_iter().map(|(tid, name)| (name, opened(tid))).collect())
    }

    pub fn report(c: &Counters, frames: u32) {
        for (name, fds) in &c.0 {
            if fds.len() != EVENTS.len() {
                eprintln!("thread {name}: no counters");
                continue;
            }
            let counts: Vec<u64> = fds
                .iter()
                .map(|&fd| {
                    let mut v = 0u64;
                    syscall(3, [fd as usize, &mut v as *mut u64 as usize, 8, 0, 0, 0]);
                    v
                })
                .collect();
            let per = |i: usize| counts[i] as f64 / f64::from(frames.max(1)) / 1e6;
            let line: Vec<String> = EVENTS.iter().enumerate().map(|(i, (n, _))| format!("{n} {:.2}M", per(i))).collect();
            eprintln!("thread {name}, a frame: {}", line.join(", "));
        }
    }
}
