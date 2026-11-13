//! Runs a ROM headless and saves frames as PPM pictures, to look at the
//! core's output on a PC.
//!
//!   cargo run --release --example run-n64 -- ROM FRAMES OUT.ppm [EVERY] [--wav SOUND.wav]
//!       [--save GAME.sav] [--press BUTTON@FRAME]... [--warmup FRAMES]
//!       [--state-in STATE] [--state-out STATE]
//!       [--jit | --interpreter] [--jit-stats] [--verify-jit]
//!       [--rsp-scalar] [--rsp-memory-reference] [--rsp-vector-reference] [--rsp-interpreter] [--verify-rsp]
//!       [--rdp-reference] [--rdp-rgba16-reference] [--rdp-memory-reference] [--verify-rdp]
//!       [--rdp-combiner-reference] [--rdp-span-reference] [--no-rdp-thread] [--verify-rdp-thread]
//!       [--rdp-threads N] [--check-rdp-marks] [--hle] [--compare-hle] [--audio-hle] [--compare-audio-hle]
//!       [--compare-video-hle] [--exact-pictures]
//!       [--counters] [--samples OUT] [--frame-times]
//!
//! Saves the last frame to OUT.ppm, or with EVERY one frame in that many as
//! OUT-<frame>.ppm; --wav saves the sound of all the frames; --save puts the
//! cartridge's save memory back from a file (if there is one) and writes it
//! there at the end; --press holds a button (a, b, z, start, up, down, left,
//! right, l, r, c-up, c-down, c-left, c-right) for a tenth of a second from
//! a frame on. Prints what the game printed through the IS-Viewer, and the
//! CPU's registers at the end.
//! --warmup excludes the first frames from the timing, but still emulates
//! them and collects their sound; --state-in resumes a saved machine and
//! --state-out writes the final one, for repeatable scene benchmarks and
//! byte-for-byte comparisons with the reference interpreter.
//! The RDP draws on threads of its own (all the cores but the machine's,
//! at most three, each every n-th line, or --rdp-threads N) unless
//! --no-rdp-thread;
//! --verify-rdp-thread compares it with drawing on the machine's thread
//! after every frame, and --check-rdp-marks draws on the machine's thread
//! checking that each draw changes only RDRAM pages it marks.
//! --counters (on the console's ARM) reads its CPU's counters for each
//! thread over the timed frames, and --samples OUT samples where each
//! thread spends its cycles, into OUT as lines of thread, code address and
//! samples (for llvm-symbolizer). Both also print the guest's work: the
//! RSP's busy cycles and the RDP's spans and pixels a frame.

use std::io::Write;

fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    let jit = flag(&mut args, "--jit");
    let interpreter = flag(&mut args, "--interpreter");
    let jit_stats = flag(&mut args, "--jit-stats");
    let verify_jit = flag(&mut args, "--verify-jit");
    let rsp_scalar = flag(&mut args, "--rsp-scalar");
    let rsp_memory_reference = flag(&mut args, "--rsp-memory-reference");
    let rsp_vector_reference = flag(&mut args, "--rsp-vector-reference");
    let rsp_interpreter = flag(&mut args, "--rsp-interpreter");
    let verify_rsp = flag(&mut args, "--verify-rsp");
    let rdp_reference = flag(&mut args, "--rdp-reference");
    let rgba16_reference = flag(&mut args, "--rdp-rgba16-reference");
    let combiner_reference = flag(&mut args, "--rdp-combiner-reference");
    let span_reference = flag(&mut args, "--rdp-span-reference");
    let rdp_memory_reference = flag(&mut args, "--rdp-memory-reference");
    let verify_rdp = flag(&mut args, "--verify-rdp");
    let no_rdp_thread = flag(&mut args, "--no-rdp-thread");
    let verify_rdp_thread = flag(&mut args, "--verify-rdp-thread");
    let check_rdp_marks = flag(&mut args, "--check-rdp-marks");
    let hle = flag(&mut args, "--hle");
    let compare_hle = flag(&mut args, "--compare-hle");
    let audio_hle = flag(&mut args, "--audio-hle");
    let compare_audio_hle = flag(&mut args, "--compare-audio-hle");
    let compare_video_hle = flag(&mut args, "--compare-video-hle");
    let exact_pictures = flag(&mut args, "--exact-pictures");
    let counting = flag(&mut args, "--counters");
    // Each timed field's own time, as the console's host sees it (a field
    // over a refresh, 16.7 ms, misses the display's next one).
    let frame_times = flag(&mut args, "--frame-times");
    let mut times = Vec::new();
    let samples = option(&mut args, "--samples");
    let work = counting || samples.is_some();
    assert!(!(jit && interpreter), "choose --jit or --interpreter");
    assert!(!jit || cfg!(any(target_arch = "arm", target_arch = "aarch64")), "--jit requires an ARM build");
    assert!(!verify_jit || (cfg!(any(target_arch = "arm", target_arch = "aarch64")) && !interpreter),
        "--verify-jit requires an ARM build with the JIT enabled");
    assert!(!verify_rsp || !rsp_scalar,
        "--verify-rsp requires RSP acceleration enabled");
    assert!(!verify_rdp || !rdp_reference,
        "--verify-rdp requires RDP acceleration enabled");
    let verify = verify_jit || verify_rsp || verify_rdp || verify_rdp_thread;
    assert!(!verify_rdp_thread || !no_rdp_thread, "--verify-rdp-thread requires the RDP's thread");
    let state_in = option(&mut args, "--state-in");
    let state_out = option(&mut args, "--state-out");
    let warmup: u32 = option(&mut args, "--warmup")
        .map(|n| n.parse().expect("--warmup takes a number of frames"))
        .unwrap_or(0);
    let rdp_threads = args.iter().position(|a| a == "--rdp-threads").map(|i| {
        let n = args.get(i + 1).and_then(|n| n.parse::<usize>().ok()).expect("--rdp-threads takes a number");
        args.drain(i..i + 2);
        n
    });
    let wav = args.iter().position(|a| a == "--wav").map(|i| {
        let path = args.get(i + 1).expect("--wav takes a file").clone();
        args.drain(i..i + 2);
        path
    });
    let sav = args.iter().position(|a| a == "--save").map(|i| {
        let path = args.get(i + 1).expect("--save takes a file").clone();
        args.drain(i..i + 2);
        path
    });
    let mut presses = Vec::new();
    while let Some(i) = args.iter().position(|a| a == "--press") {
        let press = args.get(i + 1).expect("--press takes BUTTON@FRAME").clone();
        args.drain(i..i + 2);
        let (name, frame) = press.split_once('@').expect("--press takes BUTTON@FRAME");
        let names = [
            "c-right", "c-left", "c-down", "c-up", "r", "l", "", "", "right", "left", "down", "up", "start", "z", "b",
            "a",
        ];
        let bit = names.iter().position(|&n| n == name).unwrap_or_else(|| panic!("no button {name}"));
        presses.push((1u16 << bit, frame.parse::<u32>().expect("FRAME is a number")));
    }
    if args.len() < 4 {
        eprintln!("usage: run-n64 ROM FRAMES OUT.ppm [EVERY] [--warmup FRAMES] [--state-in STATE] [--state-out STATE]");
        std::process::exit(2);
    }
    let rom = std::fs::read(&args[1]).expect("cannot read the ROM");
    let frames: u32 = args[2].parse().expect("FRAMES is a number");
    assert!(frames > warmup, "FRAMES must be greater than --warmup");
    let every: Option<u32> = args.get(4).map(|n| n.parse().expect("EVERY is a number"));
    let mut n64 = n64::N64::new(&rom).unwrap_or_else(|e| panic!("{}: {e}", args[1]));
    n64.set_jit_profiling(jit_stats || verify_jit);
    n64.set_jit(jit || verify_jit);
    n64.set_jit_source_checks(verify_jit);
    n64.set_rsp_acceleration(!rsp_scalar);
    if rsp_memory_reference {
        n64.set_rsp_memory_acceleration(false);
    }
    if rsp_vector_reference {
        n64.set_rsp_vector_acceleration(false);
    }
    if rsp_interpreter {
        n64.set_rsp_jit(false);
    }
    n64.set_rsp_profiling(jit_stats || verify);
    n64.set_rdp_acceleration(!rdp_reference);
    n64.set_rdp_rgba16_sampling(!rgba16_reference);
    n64.set_rdp_combiner_acceleration(!combiner_reference);
    n64.set_rdp_span_acceleration(!span_reference);
    if no_rdp_thread {
        n64.set_rdp_thread(false);
    }
    if let Some(n) = rdp_threads {
        n64.set_rdp_drawers(n);
    }
    // Pictures of the very field run, for comparisons and saved frames.
    n64.set_rdp_exact_pictures(verify || every.is_some() || exact_pictures);
    if check_rdp_marks {
        n64.set_rdp_mark_check(true);
    }
    n64.set_gfx_hle(hle);
    n64.set_gfx_hle_compare(compare_hle);
    n64.set_audio_hle(audio_hle);
    n64.set_audio_hle_compare(compare_audio_hle);
    n64.set_video_hle_compare(compare_video_hle);
    n64.set_rdp_memory_acceleration(!rdp_memory_reference);
    n64.set_rdp_profiling(jit_stats || verify || work);
    if let Some(data) = sav.as_ref().and_then(|path| std::fs::read(path).ok()) {
        n64.load_save_data(&data);
    }
    if let Some(path) = state_in {
        let data = std::fs::read(path).expect("cannot read the state");
        n64.load_state(&data).expect("cannot load the state");
    }
    let mut reference = verify.then(|| {
        let mut reference = n64.clone();
        if verify_rdp_thread {
            // Only the thread differs.
            reference.set_rdp_thread(false);
            return reference;
        }
        reference.set_jit(false);
        reference.set_rsp_acceleration(false);
        reference.set_rsp_profiling(false);
        reference.set_rdp_acceleration(false);
        reference.set_rdp_profiling(false);
        reference
    });
    let mut sound = Vec::new();
    let mut started = std::time::Instant::now();
    let (mut busy, mut rsp_from, mut rdp_from, mut waits_from) = (Vec::new(), 0, n64.rdp_stats(), (0, 0, 0));
    let mut tasks_from = [0; 5];
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    let (mut counters, mut samplers) = (None, Vec::new());
    for frame in 1..=frames {
        if frame == warmup + 1 {
            started = std::time::Instant::now();
            busy = thread_times();
            (rsp_from, rdp_from, waits_from) = (n64.rsp_cycles(), n64.rdp_stats(), n64.rdp_thread_counts());
            tasks_from = n64.rsp_task_cycles();
            #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
            if counting {
                counters = Some(counters::start());
            }
            #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
            if samples.is_some() {
                samplers = counters::samplers();
            }
        }
        let held = presses.iter().filter(|&&(_, at)| (at..at + 6).contains(&frame)).fold(0, |b, &(bit, _)| b | bit);
        let field = std::time::Instant::now();
        n64.run_frame(held);
        if frame_times && frame > warmup {
            times.push(field.elapsed());
        }
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        samplers.iter_mut().for_each(counters::Sampler::drain);
        if let Some(reference) = &mut reference {
            reference.run_frame(held);
            assert!(n64.frame() == reference.frame(), "accelerated picture differs at frame {frame}");
            assert!(n64.audio() == reference.audio(), "accelerated sound differs at frame {frame}");
            let (a, b) = (n64.save_state(), reference.save_state());
            if a != b {
                let at = a.iter().zip(&b).position(|(a, b)| a != b).unwrap_or(a.len().min(b.len()));
                panic!("accelerated machine state differs at frame {frame}, byte {at}: {:x?} vs {:x?}",
                    n64.registers(), reference.registers());
            }
        }
        if wav.is_some() {
            sound.extend_from_slice(n64.audio());
        }
        if let Some(every) = every
            && frame % every == 0
        {
            save(&n64, &args[3].replace(".ppm", &format!("-{frame:05}.ppm")));
        }
    }
    let elapsed = started.elapsed();
    if verify_jit {
        assert!(n64.jit_stats().instructions > 0, "no guest instruction ran natively");
        eprintln!("JIT verified against the interpreter after every frame");
    }
    if verify_rsp {
        let stats = n64.rsp_stats();
        assert!(stats.native_vector_instructions > 0 || stats.fast_vector_memory_instructions > 0
            || stats.compiled_instructions > 0,
            "no RSP instruction used an accelerated path");
        eprintln!("RSP acceleration verified against scalar execution after every frame");
    }
    if hle && n64.gfx_hle_report().loading > 0 {
        eprintln!("graphics HLE: {} tasks left to the RSP (they load another microcode)", n64.gfx_hle_report().loading);
    }
    if compare_hle {
        let report = n64.gfx_hle_report();
        for note in report.notes.iter().chain(&report.poor) {
            eprintln!("{note}");
        }
        eprintln!("HLE: {} tasks, {} commands, {} the same, {} of {} triangles close, {} tasks misaligned",
            report.tasks, report.commands, report.same, report.close, report.triangles, report.misaligned);
    }
    if compare_audio_hle {
        let report = n64.audio_hle_report();
        for note in &report.notes {
            eprintln!("{note}");
        }
        // A microcode the HLE does not know: where its data is, and how it starts.
        for &(data, size) in &report.unknown {
            let bytes: String = (0..size.min(0x40)).map(|i| format!("{:02x}", n64.peek(data + i))).collect();
            eprintln!("audio HLE: unknown microcode, data at {data:#x}, {size:#x} bytes: {bytes}");
        }
        eprintln!("audio HLE: {} tasks, {} the same, {} left to the RSP", report.tasks, report.same, report.declined);
    }
    if compare_video_hle {
        let report = n64.video_hle_report();
        for note in &report.notes {
            eprintln!("{note}");
        }
        for &(text, print) in &report.unknown {
            eprintln!("video HLE: other microcode, text at {text:#x}, fingerprint {print:#018x}");
        }
        for (k, name) in ["YUV", "blend", "resize"].into_iter().enumerate() {
            let tasks = report.tasks[k].max(1);
            eprintln!("video HLE, {name}: {} tasks, {} the same, {} the same run alone; RSP cycles a task {}, \
                estimated {}, worst {:+}", report.tasks[k], report.same[k], report.same_alone[k],
                report.cycles[k] / tasks, report.estimated[k] / tasks, report.worst[k]);
        }
        eprintln!("video HLE: {} tasks left to the RSP", report.declined);
    }
    if verify_rdp_thread {
        eprintln!("RDP thread verified against drawing on the machine's thread after every frame");
    }
    if verify_rdp {
        let stats = n64.rdp_stats();
        assert!(stats.three_texel_samples > 0 || stats.rgba16_samples > 0 || stats.skipped_color_reads > 0
            || stats.fast_span_pixels > 0,
            "no RDP pixel used an accelerated path");
        eprintln!("RDP acceleration verified against the reference after every frame");
    }
    let measured = frames - warmup;
    if frame_times && !times.is_empty() {
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
        let order: Vec<f64> = times.iter().map(|&d| ms(d)).collect();
        let mut sorted = order.clone();
        sorted.sort_by(f64::total_cmp);
        let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize];
        let over = order.iter().filter(|&&t| t > 1e3 / 60.0).count();
        eprintln!("field times: median {:.1} ms, 90% {:.1}, max {:.1}; {over} of {} over a refresh", at(0.5), at(0.9),
            at(1.0), order.len());
        let first: Vec<String> = order.iter().take(24).map(|t| format!("{t:.1}")).collect();
        eprintln!("first fields: {}", first.join(" "));
    }
    eprintln!(
        "{measured} frames after {warmup} warmup in {:.3} s ({:.1} fps)",
        elapsed.as_secs_f64(), measured as f64 / elapsed.as_secs_f64()
    );
    for (name, end) in thread_times() {
        let start = busy.iter().find(|(n, _)| *n == name).map_or(0.0, |&(_, t)| t);
        eprintln!("thread {name}: {:.2} s on the CPU", end - start);
    }
    if work {
        let per = |n: u64| n as f64 / f64::from(measured) / 1e3;
        let rdp = n64.rdp_stats();
        let (waits, batches, full) = n64.rdp_thread_counts();
        let tasks = n64.rsp_task_cycles();
        eprintln!("RSP cycles a frame by task: other {:.0}k, graphics {:.0}k, sound {:.0}k, video {:.0}k, JPEG {:.0}k",
            per(tasks[0] - tasks_from[0]), per(tasks[1] - tasks_from[1]), per(tasks[2] - tasks_from[2]),
            per(tasks[3] - tasks_from[3]), per(tasks[4] - tasks_from[4]));
        eprintln!("a frame: RSP {:.0}k cycles, RDP {:.1}k spans, {:.0}k pixels; RDP thread: {:.2} waits, {:.1} batches, \
            {:.2} full", per(n64.rsp_cycles() - rsp_from), per(rdp.spans - rdp_from.spans),
            per(rdp.pixels - rdp_from.pixels), per(waits - waits_from.0) * 1e3, per(batches - waits_from.1) * 1e3,
            per(full - waits_from.2) * 1e3);
    }
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    if let Some(c) = counters {
        counters::report(&c, measured);
    }
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    if let Some(path) = &samples {
        let mut out = String::new();
        for s in &samplers {
            let mut seen: Vec<_> = s.seen.iter().collect();
            seen.sort_by(|a, b| b.1.cmp(a.1));
            for (ip, n) in seen {
                out += &format!("{} {ip:#x} {n}\n", s.name);
            }
        }
        // The CPU dynarec's blocks, to tell its samples by guest code.
        for (host, physical, instructions) in n64.jit_code_map() {
            out += &format!("jit {host:#x} {physical:#x} {instructions}\n");
        }
        std::fs::write(path, out).expect("cannot write the samples");
    }
    if every.is_none() {
        save(&n64, &args[3]);
    }
    if !n64.isviewer().is_empty() {
        eprintln!("IS-Viewer: {}", String::from_utf8_lossy(n64.isviewer()));
    }
    eprintln!("{:x?}", n64.registers());
    if jit_stats {
        eprintln!("JIT: {:?}", n64.jit_stats());
        let top_ops = n64.interpreted_opcodes();
        if !top_ops.is_empty() {
            eprintln!("Interpreted ops: {:?}", &top_ops[..top_ops.len().min(10)]);
            let mfc0 = n64.interpreted_mfc0();
            if !mfc0.is_empty() {
                eprintln!("Interpreted MFC0 rd: {:?}", mfc0);
            }
        }
        eprintln!("RSP: {:?}", n64.rsp_stats());
        eprintln!("RSP JIT blocks compared, compiled: {:?}", n64.rsp_jit_counts());
        eprintln!("RDP: {:?}", n64.rdp_stats());
    }
    if let (Some(path), Some(data)) = (sav, n64.save_data()) {
        std::fs::write(&path, data).expect("cannot write the save");
    }
    if let Some(path) = state_out {
        std::fs::write(path, n64.save_state()).expect("cannot write the state");
    }
    if let Some(path) = wav {
        save_wav(&sound, &path);
        let peak = sound.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        eprintln!("{} samples, peak {peak}", sound.len());
    }
}

fn option(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    let value = args.get(i + 1).unwrap_or_else(|| panic!("{flag} takes a value")).clone();
    args.drain(i..i + 2);
    Some(value)
}

fn flag(args: &mut Vec<String>, flag: &str) -> bool {
    if let Some(i) = args.iter().position(|a| a == flag) {
        args.remove(i);
        true
    } else {
        false
    }
}

/// Stereo 16-bit PCM.
fn save_wav(samples: &[i16], path: &str) {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).expect("cannot create the sound"));
    let bytes = (samples.len() * 2) as u32;
    let rate = n64::SAMPLE_HZ;
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

fn save(n64: &n64::N64, path: &str) {
    let (pixels, width, height) = n64.frame();
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).expect("cannot create the picture"));
    write!(out, "P6\n{} {}\n255\n", width.max(1), height.max(1)).unwrap();
    if pixels.is_empty() {
        out.write_all(&[0, 0, 0]).unwrap();
    }
    for &p in pixels {
        out.write_all(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]).unwrap();
    }
}

/// The ARM CPU's own counters, for each thread: through Linux's
/// perf_event_open, by raw event numbers of the ARMv7/v8 PMU.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
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

    /// The system calls used. MMAP is mmap2 on 32-bit ARM, whose offset (in pages) is 0 here anyway.
    #[cfg(target_arch = "arm")]
    mod number {
        pub const PERF_EVENT_OPEN: usize = 364;
        pub const READ: usize = 3;
        pub const MMAP: usize = 192;
    }
    #[cfg(target_arch = "aarch64")]
    mod number {
        pub const PERF_EVENT_OPEN: usize = 241;
        pub const READ: usize = 63;
        pub const MMAP: usize = 222;
    }

    #[cfg(target_arch = "arm")]
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

    #[cfg(target_arch = "aarch64")]
    fn syscall(number: usize, args: [usize; 6]) -> isize {
        let ret: isize;
        unsafe {
            core::arch::asm!("svc #0",
                in("x8") number,
                inlateout("x0") args[0] => ret,
                in("x1") args[1],
                in("x2") args[2],
                in("x3") args[3],
                in("x4") args[4],
                in("x5") args[5],
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
        let fd = syscall(number::PERF_EVENT_OPEN, [attr.as_ptr() as usize, tid, usize::MAX, usize::MAX, 0, 0]);
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

    /// The kernel's page size (AT_PAGESZ, in the auxiliary vector), which a ring buffer is made of.
    fn page_size() -> usize {
        const WORD: usize = size_of::<usize>();
        let vector = std::fs::read("/proc/self/auxv").expect("cannot read the auxiliary vector");
        let word = |bytes: &[u8]| usize::from_ne_bytes(bytes.try_into().unwrap());
        vector.chunks_exact(2 * WORD).find(|pair| word(&pair[..WORD]) == 6).map_or(4096, |pair| word(&pair[WORD..]))
    }

    /// A thread's cycles sampled every 20000: the kernel writes where its
    /// code was into a ring buffer, emptied into counts by address.
    pub struct Sampler {
        pub name: String,
        ring: *mut u8,
        page: usize,
        pub seen: std::collections::HashMap<u64, u32>,
    }

    pub fn samplers() -> Vec<Sampler> {
        let mut samplers = Vec::new();
        let page = page_size();
        for (tid, name) in threads() {
            // Cycles (raw 0x11), a sample period, PERF_SAMPLE_IP.
            let mut attr = [0u64; 14];
            attr[0] = 4 | 112 << 32;
            attr[1] = 0x11;
            attr[2] = 20_000;
            attr[3] = 1;
            attr[5] = 1 << 5 | 1 << 6;
            let fd = syscall(number::PERF_EVENT_OPEN, [attr.as_ptr() as usize, tid, usize::MAX, usize::MAX, 0, 0]);
            if fd < 0 {
                eprintln!("thread {name}: no samples");
                continue;
            }
            // The control page, then the buffer; shared, read and write.
            let ring = syscall(number::MMAP, [0, (PAGES + 1) * page, 3, 1, fd as usize, 0]);
            if (-4096..0).contains(&ring) {
                eprintln!("thread {name}: no samples");
                continue;
            }
            samplers.push(Sampler { name, ring: ring as *mut u8, page, seen: Default::default() });
        }
        samplers
    }

    impl Sampler {
        /// Counts the samples written since, and gives their room back.
        pub fn drain(&mut self) {
            let mask = (PAGES * self.page) - 1;
            unsafe {
                // data_head and data_tail, in the control page.
                let head = std::ptr::read_volatile(self.ring.add(1024) as *const u64);
                std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
                let mut tail = std::ptr::read_volatile(self.ring.add(1032) as *const u64);
                let data = self.ring.add(self.page);
                let byte = |at: u64| *data.add(at as usize & mask);
                let word = |at: u64| u32::from_le_bytes(std::array::from_fn(|i| byte(at + i as u64)));
                while tail < head {
                    // The header: type, misc and size; a sample's address follows.
                    let (kind, len) = (word(tail), word(tail + 4) >> 16);
                    if kind == 9 {
                        let ip = u64::from(word(tail + 8)) | u64::from(word(tail + 12)) << 32;
                        *self.seen.entry(ip).or_default() += 1;
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
                    syscall(number::READ, [fd as usize, &mut v as *mut u64 as usize, 8, 0, 0, 0]);
                    v
                })
                .collect();
            let per = |i: usize| counts[i] as f64 / f64::from(frames.max(1)) / 1e6;
            let line: Vec<String> = EVENTS.iter().enumerate().map(|(i, (n, _))| format!("{n} {:.2}M", per(i))).collect();
            eprintln!("thread {name}, a frame: {}", line.join(", "));
        }
    }
}
