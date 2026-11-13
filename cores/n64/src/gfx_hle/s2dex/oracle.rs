//! S2DEX's HLE checked against the microcode itself, run on the RSP: display lists with random structures, run
//! both ways from the same machine, must give the RDP the same commands, word for word (no-ops left out, syncs
//! by their kind). The microcodes are not part of the repository: S2DEX_UCODES names a directory with
//! VERSION-text.bin (the text as the game has it in RDRAM, 8 KB from its start) and VERSION-data.bin (2 KB of
//! data); without it these tests check nothing.
//!
//!   S2DEX_UCODES=DIR cargo test --release -p n64 --lib s2dex -- --ignored --nocapture

use super::super::*;

const TEXT: u32 = 0x10_0000;
const DATA: u32 = 0x10_4000;
const STACK: u32 = 0x10_5000;
const YIELD: u32 = 0x10_6000;
const FIFO: u32 = 0x18_0000;
const FIFO_END: u32 = 0x19_0000;
const LIST: u32 = 0x20_0000;
/// Where tests put structures and textures (2 MB on).
pub(super) const HEAP: u32 = 0x40_0000;

/// The builds there are, from S2DEX_UCODES: (name, text, data).
pub(super) fn builds() -> Vec<(String, Vec<u8>, Vec<u8>)> {
    let Some(dir) = std::env::var_os("S2DEX_UCODES") else {
        eprintln!("S2DEX_UCODES is not set: nothing checked");
        return Vec::new();
    };
    let dir = std::path::PathBuf::from(dir);
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("S2DEX_UCODES is a directory") {
        let name = entry.unwrap().file_name().into_string().unwrap();
        let only = std::env::var("S2DEX_BUILD").unwrap_or_default();
        if let Some(version) = name.strip_suffix("-text.bin").filter(|v| v.contains(only.as_str())) {
            let text = std::fs::read(dir.join(&name)).unwrap();
            let data = std::fs::read(dir.join(format!("{version}-data.bin"))).unwrap();
            found.push((version.to_string(), text, data));
        }
    }
    found.sort();
    found
}

/// A small generator of random numbers (xorshift), for repeatable lists.
pub(super) struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }

    pub fn below(&mut self, n: u32) -> u32 {
        self.next() % n.max(1)
    }

    pub fn range(&mut self, low: i32, high: i32) -> i32 {
        low + self.below((high - low + 1) as u32) as i32
    }
}

/// A machine with the microcode in RDRAM and a graphics task of it in DMEM, its display list at LIST.
pub(super) fn machine(text: &[u8], data: &[u8]) -> N64 {
    let mut rom = vec![0; 0x1000];
    rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
    let mut n64 = N64::new(&rom).unwrap();
    n64.rdram[TEXT as usize..TEXT as usize + text.len()].copy_from_slice(text);
    n64.rdram[DATA as usize..DATA as usize + data.len()].copy_from_slice(data);
    n64
}

pub(super) fn put32(n64: &mut N64, at: u32, value: u32) {
    n64.rdram[at as usize..at as usize + 4].copy_from_slice(&value.to_be_bytes());
}

pub(super) fn put16(n64: &mut N64, at: u32, value: u16) {
    n64.rdram[at as usize..at as usize + 2].copy_from_slice(&value.to_be_bytes());
}

pub(super) fn put_list(n64: &mut N64, list: &[(u32, u32)]) {
    for (i, &(w0, w1)) in list.iter().enumerate() {
        put32(n64, LIST + 8 * i as u32, w0);
        put32(n64, LIST + 8 * i as u32 + 4, w1);
    }
}

fn task() -> Task {
    Task { flags: 0, dram_stack_size: 0x400, ucode_data: DATA, ucode_data_size: 0x800, data_ptr: LIST }
}

thread_local! {
    /// S2DEX's DMEM as the HLE left it at its task's end, to compare with the RSP's.
    pub(in super::super) static LAST_DMEM: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    static RSP_DMEM: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    static RSP_CYCLES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    pub(in super::super) static LAST_VU: std::cell::RefCell<[[u16; 8]; 32]> =
        const { std::cell::RefCell::new([[0; 8]; 32]) };
    static RSP_VU: std::cell::RefCell<[[u16; 8]; 32]> = const { std::cell::RefCell::new([[0; 8]; 32]) };
}

/// The task run by the microcode on the RSP (as the boot code starts it): the RDP's commands, or None if it
/// did not finish.
pub(super) fn run_rsp(n64: &N64, text: &[u8]) -> Option<Vec<Vec<u64>>> {
    let mut n64 = n64.clone();
    let rsp = &mut n64.rsp;
    rsp.memory[..0x800].copy_from_slice(&n64.rdram[DATA as usize..DATA as usize + 0x800]);
    rsp.memory[0x1080..0x2000].copy_from_slice(&text[..0xf80]);
    let task = [1, 0, 0, 0, TEXT, 0x1000, DATA, 0x800, STACK, 0x400, FIFO, FIFO_END, LIST, 0, YIELD, 0xc00];
    for (i, word) in task.into_iter().enumerate() {
        rsp.write_memory(0xfc0 + 4 * i as u32, word, !0);
    }
    // What earlier tasks left in the vector registers: nothing the microcode sends may depend on it.
    let mut rng = Rng(0x2545_f491_4f6c_dd1d ^ u64::from(n64.rdram[LIST as usize + 7]));
    for lane in rsp.vu.regs.iter_mut().flatten() {
        *lane = rng.next() as u16;
    }
    rsp.pc = 0x080;
    rsp.next_pc = 0x084;
    rsp.status &= !1;
    n64.hle = Hle { capturing: true, capture_only: true, ..Hle::default() };
    // S2DEX_TRACE: IMEM addresses (hex) where the scalar registers are printed.
    let trace: Vec<u32> = std::env::var("S2DEX_TRACE").unwrap_or_default().split(',')
        .filter_map(|a| u32::from_str_radix(a.trim(), 16).ok()).map(|a| a & 0xffc).collect();
    for _ in 0..50_000_000 {
        if trace.contains(&n64.rsp.pc) {
            const NAMES: [&str; 32] = ["0", "at", "v0", "v1", "a0", "a1", "a2", "a3", "t0", "t1", "t2", "t3", "t4",
                "t5", "t6", "t7", "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7", "t8", "t9", "k0", "k1", "gp", "sp",
                "s8", "ra"];
            let regs: Vec<String> = (1..32).map(|r| format!("{}={:x}", NAMES[r], n64.rsp.gpr[r])).collect();
            eprintln!("RSP {:04x}: {}", 0x1000 | n64.rsp.pc, regs.join(" "));
        }
        if n64.rsp.halted() {
            RSP_CYCLES.with(|c| c.set(n64.rsp.clock));
            RSP_DMEM.with(|d| *d.borrow_mut() = n64.rsp.memory[..0x1000].to_vec());
            RSP_VU.with(|d| *d.borrow_mut() = n64.rsp.vu.regs);
            return Some(n64.hle.captured);
        }
        n64.rsp_step();
    }
    None
}

/// The task run by the HLE: the RDP's commands.
pub(super) fn run_hle(n64: &N64) -> Vec<Vec<u64>> {
    let mut n64 = n64.clone();
    n64.hle = Hle { capturing: true, capture_only: true, ..Hle::default() };
    let ucode = n64.identify(&task()).expect("S2DEX's data");
    n64.run_gfx(ucode, &task());
    n64.hle.captured
}

/// No-ops left out, syncs by their kind (the microcode leaves anything in the rest).
pub(super) fn normal(commands: &[Vec<u64>]) -> Vec<Vec<u64>> {
    commands.iter().filter(|c| c[0] >> 56 & 0x3f != 0).map(|c| match c[0] >> 56 & 0x3f {
        0x26..=0x29 => vec![c[0] & 0xff00_0000_0000_0000],
        _ => c.clone(),
    }).collect()
}

/// Runs `cases` lists from `make` on every build, both ways; panics at the first difference, with the list.
pub(super) fn check(name: &str, cases: u32, make: impl Fn(&mut Rng, &mut N64) -> Vec<(u32, u32)>) {
    // S2DEX_SCALE: that many times the cases (other lists).
    let cases = cases * std::env::var("S2DEX_SCALE").ok().and_then(|n| n.parse().ok()).unwrap_or(1);
    for (version, text, data) in builds() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ u64::from(cases));
        let (mut empty, mut endless) = (0, 0);
        for case in 0..cases {
            let mut n64 = machine(&text, &data);
            let list = make(&mut rng, &mut n64);
            put_list(&mut n64, &list);
            // A background with a scale of 1/1024 or so takes the microcode a very long time: left out.
            let Some(rsp) = run_rsp(&n64, &text) else {
                endless += 1;
                continue;
            };
            let (expected, actual) = (normal(&rsp), normal(&run_hle(&n64)));
            if expected.len() <= 1 {
                empty += 1;
            }
            if expected != actual {
                // The work area's words that differ at the end (S2DEX's own DMEM, from 0x180).
                let (rsp, hle) = (RSP_DMEM.with(|d| d.borrow().clone()), LAST_DMEM.with(|d| d.borrow().clone()));
                if hle.len() >= 0x1000 {
                    let word = |m: &[u8], at: usize| u32::from_be_bytes(m[at..at + 4].try_into().unwrap());
                    let differ: Vec<String> = (0x180..0x480).step_by(4).filter(|&at| word(&rsp, at) != word(&hle, at))
                        .map(|at| format!("{at:03x}: {:08x} vs {:08x}", word(&rsp, at), word(&hle, at))).collect();
                    eprintln!("DMEM differs (RSP vs HLE): {differ:?}");
                    let (rsp, hle) = (RSP_VU.with(|d| *d.borrow()), LAST_VU.with(|d| *d.borrow()));
                    for v in 0..32 {
                        if rsp[v] != hle[v] {
                            eprintln!("v{v} differs: {:04x?} vs {:04x?}", rsp[v], hle[v]);
                        }
                    }
                }
                let first = expected.iter().zip(&actual).position(|(e, a)| e != a)
                    .unwrap_or(expected.len().min(actual.len()));
                panic!("{name} {version} case {case}: {} commands from the RSP, {} here, first differing {first}:\n\
                    RSP  {:016x?}\nHLE  {:016x?}\nlist {list:08x?}", expected.len(), actual.len(),
                    &expected[first.saturating_sub(4)..(first + 6).min(expected.len())],
                    &actual[first.saturating_sub(4)..(first + 6).min(actual.len())]);
            }
        }
        eprintln!("{name} {version}: {} lists the same ({empty} with nothing drawn), {endless} too long for the RSP",
            cases - endless);
    }
}

#[test]
#[ignore]
fn s2dex_passes_rdp_commands_through() {
    check("pass-through", 200, |rng, _| {
        let mut list = Vec::new();
        for _ in 0..rng.range(1, 30) {
            let w1 = rng.next();
            let command = match rng.below(12) {
                0 => (0xed00_0000 | rng.below(0x100_0000), w1 & 0x03ff_ffff),
                1 => (0xef00_0000 | rng.below(0x100_0000), w1),
                // G_SETOTHERMODE_L and _H: `len` bits from `shift` (H's within its 24), w1 within them.
                2 | 3 => {
                    let high = rng.below(2);
                    let shift = rng.below(if high == 1 { 23 } else { 31 });
                    let len = 1 + rng.below(if high == 1 { 24 } else { 32 } - shift);
                    let mask = ((1u64 << len) - 1) as u32;
                    (0xe200_0000 | high << 24 | (32 - shift - len) << 8 | (len - 1), w1 & mask << shift)
                }
                4 => (0xe600_0000 + (rng.below(4) << 24), 0),
                5 => (0xfa00_0000 | rng.below(0x100_0000), w1),
                6 => (0xfd00_0000 | rng.below(0x100_0000), w1 & 0x00ff_ffff),
                7 => (0xdb06_0000 | (rng.below(16) * 4), w1 & 0x00ff_fff0),
                8..=10 => {
                    // A texture rectangle: G_RDPHALF_0, then G_RDPHALF_1 and 2 with s, t and their slopes.
                    list.push((0xe400_0000 | rng.below(2) << 24 | rng.below(0x100_0000), w1 & 0x00ff_ffff));
                    list.push((0xe100_0000, rng.next()));
                    (0xf100_0000, rng.next())
                }
                _ => (0xf500_0000 | rng.below(0x100_0000), w1),
            };
            list.push(command);
        }
        list.push((0xdf00_0000, 0));
        list
    });
}

/// A uObjSprite at `at`: (x, y) in 10.2, its texture's width and height in texels, scale 1.0 (u5.10).
pub(super) fn put_sprite(n64: &mut N64, at: u32, xy: (i16, i16), wh: (u16, u16), scale: (u16, u16), tmem: (u16, u16),
    format: [u8; 4]) {
    let halves = [xy.0 as u16, scale.0, wh.0 << 5, 0, xy.1 as u16, scale.1, wh.1 << 5, 0, tmem.1, tmem.0];
    for (i, h) in halves.into_iter().enumerate() {
        put16(n64, at + 2 * i as u32, h);
    }
    n64.rdram[at as usize + 20..at as usize + 24].copy_from_slice(&format);
}

/// A random uObjSprite at `at`: on and off the screen, scaled from 1/32 to 32 (and by the extremes), its
/// texture up to 256 texels a side, flipped or not.
pub(super) fn random_sprite(rng: &mut Rng, n64: &mut N64, at: u32) {
    let scale = |rng: &mut Rng| match rng.below(8) {
        0 => [1, 0x20, 0xffff, 0x8000][rng.below(4) as usize],
        1..=3 => 0x400,
        _ => 0x20 + rng.below(0x4000),
    } as u16;
    let size = |rng: &mut Rng| {
        (if rng.below(4) == 0 { 1 + rng.below(0x2000) } else { (1 + rng.below(256)) << 5 }) as u16
    };
    let halves = [rng.range(-800, 1600) as u16, scale(rng), size(rng), 0, rng.range(-600, 1200) as u16, scale(rng),
        size(rng), 0, rng.below(0x80) as u16, rng.below(0x200) as u16];
    for (i, h) in halves.into_iter().enumerate() {
        put16(n64, at + 2 * i as u32, h);
    }
    let flags = [0, 1, 0x10, 0x11][rng.below(4) as usize];
    let format = [rng.below(5) as u8, rng.below(4) as u8, rng.below(16) as u8, flags];
    n64.rdram[at as usize + 20..at as usize + 24].copy_from_slice(&format);
}

/// A random uObjMtx at `at`: a rotation and scale (s15.16), a place (s10.2) and base scales (u5.10).
pub(super) fn random_matrix(rng: &mut Rng, n64: &mut N64, at: u32) {
    let angle = rng.below(360) as f64 * std::f64::consts::PI / 180.0;
    let (sx, sy) = (0.25 + rng.below(16) as f64 / 4.0, 0.25 + rng.below(16) as f64 / 4.0);
    let fixed = |v: f64| (v * 65536.0) as i32 as u32;
    let (a, b, c, d) = if rng.below(3) == 0 {
        (fixed(1.0), 0, 0, fixed(1.0))
    } else {
        (fixed(angle.cos() * sx), fixed(-angle.sin() * sy), fixed(angle.sin() * sx), fixed(angle.cos() * sy))
    };
    for (i, w) in [a, b, c, d].into_iter().enumerate() {
        put32(n64, at + 4 * i as u32, w);
    }
    put16(n64, at + 16, rng.range(-400, 1600) as u16);
    put16(n64, at + 18, rng.range(-400, 1200) as u16);
    put16(n64, at + 20, [0x400, 0x200, 0x800, 1 + rng.below(0x2000)][rng.below(4) as usize] as u16);
    put16(n64, at + 22, [0x400, 0x200, 0x800, 1 + rng.below(0x2000)][rng.below(4) as usize] as u16);
}

/// Commands that set what objects are drawn with: the render mode, the cycle type, the scissor, the object
/// matrix or its place and scale.
pub(super) fn random_object_state(rng: &mut Rng, n64: &mut N64, list: &mut Vec<(u32, u32)>, at: u32) {
    match rng.below(6) {
        0 => list.push((0x0b00_0000, rng.below(0x80))),
        1 => list.push((0xef00_0000 | rng.below(4) << 20 | rng.below(0x10_0000), rng.next())),
        2 => {
            let (x0, y0) = (rng.below(400), rng.below(300));
            list.push((0xed00_0000 | x0 << 12 | y0, (x0 + rng.below(1400)) << 12 | (y0 + rng.below(1000))));
        }
        3 => {
            random_matrix(rng, n64, at);
            list.push((0xdc10_0000, at));
        }
        4 => {
            random_matrix(rng, n64, at);
            list.push((0xdc00_0002, at + 16));
        }
        _ => {}
    }
}

#[test]
#[ignore]
fn s2dex_draws_object_rectangles() {
    check("rectangle", 400, |rng, n64| {
        let mut list = Vec::new();
        for i in 0..rng.range(1, 10) {
            let at = HEAP + 0x100 * i as u32;
            random_object_state(rng, n64, &mut list, at + 0x80);
            random_txtr(rng, n64, at);
            random_sprite(rng, n64, at + 24);
            list.push(match rng.below(4) {
                0 => (0x0100_0000, at + 24),
                1 => (0xda00_0000, at + 24),
                2 => (0x0700_002f, at),
                _ => (0x0800_002f, at),
            });
        }
        list.push((0xdf00_0000, 0));
        list
    });
}

#[test]
#[ignore]
fn s2dex_draws_object_sprites() {
    check("sprite", 400, |rng, n64| {
        let mut list = Vec::new();
        for i in 0..rng.range(1, 10) {
            let at = HEAP + 0x100 * i as u32;
            random_object_state(rng, n64, &mut list, at + 0x80);
            random_txtr(rng, n64, at);
            random_sprite(rng, n64, at + 24);
            list.push(if rng.below(2) == 0 { (0x0200_0000, at + 24) } else { (0x0600_002f, at) });
        }
        list.push((0xdf00_0000, 0));
        list
    });
}

/// A random uObjBg (or uObjScaleBg, `scaled`) at `at`: an image of up to 512 texels a side, of any format,
/// drawn to a frame on, off or across the screen's edges, scaled from 1/4 to 4 (and by the extremes).
pub(super) fn random_bg(rng: &mut Rng, n64: &mut N64, at: u32, scaled: bool) {
    let texels = |rng: &mut Rng| match rng.below(4) {
        0 => [320, 240, 256, 64][rng.below(4) as usize],
        _ => 2 + rng.below(255) * 2,
    };
    let (width, height) = (texels(rng), texels(rng));
    let frame = |rng: &mut Rng, size: u32| match rng.below(4) {
        0 => size << 2,
        _ => (1 + rng.below(400)) << 2 | rng.below(4),
    };
    // Mostly on the screen (the microcode does not end a background wholly outside the scissor: it draws a
    // strip for each of 65536 lines less what it is outside by).
    let far = rng.below(16) == 0;
    let halves = [
        rng.below(width << 5) as u16,
        (width << 2) as u16,
        if far { rng.range(-1600, 1600) } else { rng.range(-160, 1200) } as u16,
        frame(rng, width) as u16,
        rng.below(height << 5) as u16,
        (height << 2) as u16,
        if far { rng.range(-1600, 1600) } else { rng.range(-160, 900) } as u16,
        frame(rng, height) as u16,
    ];
    for (i, h) in halves.into_iter().enumerate() {
        put16(n64, at + 2 * i as u32, h);
    }
    put32(n64, at + 0x10, HEAP + 0x10_0000 + rng.below(0x1000) * 8);
    put16(n64, at + 0x14, if rng.below(2) == 0 { 0xfff4 } else { 0x0033 });
    n64.rdram[at as usize + 0x16] = rng.below(5) as u8;
    n64.rdram[at as usize + 0x17] = rng.below(4) as u8;
    put16(n64, at + 0x18, rng.below(16) as u16);
    put16(n64, at + 0x1a, (rng.below(4) == 0) as u16);
    if scaled {
        let scale = |rng: &mut Rng| match rng.below(6) {
            0 => [1, 0x40, 0x7fff, 0xffff][rng.below(4) as usize],
            1..=2 => 0x400,
            _ => 0x100 + rng.below(0xf00),
        } as u16;
        put16(n64, at + 0x1c, scale(rng));
        put16(n64, at + 0x1e, scale(rng));
        let y = u32::from(halves[4]);
        let origin = if rng.below(2) == 0 { y } else { y.wrapping_add(rng.below(0x2000)).wrapping_sub(0x1000) };
        put32(n64, at + 0x20, origin);
        put32(n64, at + 0x24, 0);
    } else {
        // tmemW, tmemH, tmemLoadSH, tmemLoadTH, tmemSizeW, tmemSize, as guS2DInitBg makes them, or near.
        let tmem_w = (width << (rng.below(4))) / 16 + rng.below(2);
        let tmem_h = (2048 / tmem_w.max(1)).max(1) * 4;
        let tmem = [tmem_w, tmem_h, tmem_w * 16 - 1, tmem_h - 1, tmem_w * 2, tmem_w * tmem_h / 2];
        for (i, h) in tmem.into_iter().enumerate() {
            put16(n64, at + 0x1c + 2 * i as u32, h as u16);
        }
    }
}

/// Commands that set what backgrounds are drawn with: the render mode, the cycle type, the scissor.
pub(super) fn random_bg_state(rng: &mut Rng, list: &mut Vec<(u32, u32)>) {
    match rng.below(4) {
        0 => list.push((0x0b00_0000, rng.below(0x80))),
        1 => list.push((0xef00_0000 | rng.below(4) << 20 | rng.below(0x10_0000), rng.next())),
        2 => {
            let (x0, y0) = (rng.below(400), rng.below(300));
            list.push((0xed00_0000 | x0 << 12 | y0, (x0 + 4 + rng.below(1400)) << 12 | (y0 + 4 + rng.below(1000))));
        }
        _ => {}
    }
}

#[test]
#[ignore]
fn s2dex_draws_scaled_backgrounds() {
    check("scaled background", 400, |rng, n64| {
        let mut list = Vec::new();
        for i in 0..rng.range(1, 4) {
            let at = HEAP + 0x100 * i as u32;
            random_bg_state(rng, &mut list);
            random_bg(rng, n64, at, true);
            list.push((0x0900_0000, at));
        }
        list.push((0xdf00_0000, 0));
        list
    });
}

#[test]
#[ignore]
fn s2dex_copies_backgrounds() {
    check("background copy", 400, |rng, n64| {
        let mut list = Vec::new();
        for i in 0..rng.range(1, 4) {
            let at = HEAP + 0x100 * i as u32;
            random_bg_state(rng, &mut list);
            random_bg(rng, n64, at, false);
            list.push((0x0a00_0000, at));
        }
        list.push((0xdf00_0000, 0));
        list
    });
}

/// Lists of everything at once: objects and backgrounds in turn (the microcode swaps their code in and out),
/// sub-lists called and branched to, by G_DL through segments and by G_SELECT_DL against the status words.
#[test]
#[ignore]
fn s2dex_runs_mixed_lists() {
    check("mixed", 300, |rng, n64| {
        const SUBS: u32 = HEAP + 0x8000;
        // Four short lists to call: each draws something and ends.
        for k in 0..4u32 {
            let at = HEAP + 0x1000 * (k + 1);
            random_txtr(rng, n64, at);
            random_sprite(rng, n64, at + 24);
            random_bg(rng, n64, at + 0x100, k % 2 == 0);
            let sub = [(0xe700_0000, 0), [(0x0700_002f, at), (0x0200_0000, at + 24), (0x0900_0000, at + 0x100),
                (0x0a00_0000, at + 0x100)][k as usize], (0xdf00_0000, 0)];
            for (i, &(w0, w1)) in sub.iter().enumerate() {
                put32(n64, SUBS + 0x100 * k + 8 * i as u32, w0);
                put32(n64, SUBS + 0x100 * k + 8 * i as u32 + 4, w1);
            }
        }
        let mut list = vec![(0xdb06_0014, SUBS)];
        for i in 0..rng.range(1, 12) {
            let at = HEAP + 0x100 * i as u32;
            random_object_state(rng, n64, &mut list, HEAP + 0x6000 + 0x40 * i as u32);
            let sub = rng.below(4) * 0x100;
            match rng.below(7) {
                0 => list.push((0xde00_0000 | rng.below(2) << 16, 0x0500_0000 | sub)),
                1 => {
                    // G_SELECT_DL: the address's low half, the status id and the flag, then the high half, push or
                    // not, and the mask.
                    let address = SUBS + sub;
                    let flags = [0, 1, 0x8000_0001, rng.next()];
                    let (sid, flag) = (rng.below(4) * 4, flags[rng.below(4) as usize]);
                    list.push((0xe400_0000 | sid << 16 | address & 0xffff, flag));
                    let mask = flags[rng.below(4) as usize] | rng.below(2);
                    list.push((0x0400_0000 | rng.below(2) << 16 | address >> 16, mask));
                }
                2 => list.push((0xdb08_0000 | (rng.below(4) * 4), [0, 1, rng.next()][rng.below(3) as usize])),
                3 => {
                    random_bg(rng, n64, at, true);
                    list.push((0x0900_0000, at));
                }
                4 => {
                    random_bg(rng, n64, at, false);
                    list.push((0x0a00_0000, at));
                }
                5 => {
                    random_sprite(rng, n64, at);
                    list.push(([0x0100_0000, 0xda00_0000, 0x0200_0000][rng.below(3) as usize], at));
                }
                _ => {
                    random_txtr(rng, n64, at);
                    random_sprite(rng, n64, at + 24);
                    list.push(([0x0500_0017, 0x0600_002f, 0x0700_002f, 0x0800_002f][rng.below(4) as usize], at));
                }
            }
        }
        list.push((0xdf00_0000, 0));
        list
    });
}

/// A list made by hand, both ways: change it to see what the microcode sends (the first difference shows it).
#[test]
#[ignore]
fn s2dex_explore() {
    check("explore", 1, |_, n64| {
        put_sprite(n64, HEAP, (40, 80), (32, 16), (1024, 1024), (0, 4), [0, 2, 0, 0]);
        vec![(0x0100_0000, HEAP), (0x0b00_0000, 8), (0x0100_0000, HEAP), (0xdf00_0000, 0)]
    });
}

/// A random uObjTxtr at `at`: a Block, Tile or TLUT load of a texture in the heap, with a status id, flag and
/// mask that often match what the status word holds.
pub(super) fn random_txtr(rng: &mut Rng, n64: &mut N64, at: u32) {
    let image = HEAP + 0x10_0000 + rng.below(0x1000) * 8;
    let (kind, a, b, c) = match rng.below(3) {
        // Block: tmem, tsize, tline.
        0 => (0x0000_1033, rng.below(0x200), rng.below(0x800), rng.below(0x800)),
        // Tile: tmem, twidth, theight.
        1 => (0x00fc_1034, rng.below(0x200), rng.below(0x400), rng.below(0x400)),
        // TLUT: phead, pnum, 0.
        _ => (0x0000_0030, 0x100 + rng.below(0x100), rng.below(0x100), 0),
    };
    put32(n64, at, kind);
    put32(n64, at + 4, image);
    put16(n64, at + 8, a as u16);
    put16(n64, at + 10, b as u16);
    put16(n64, at + 12, c as u16);
    put16(n64, at + 14, (rng.below(4) * 4) as u16);
    let flags = [0, 1, 0x8000_0001, rng.next()];
    put32(n64, at + 16, flags[rng.below(4) as usize]);
    put32(n64, at + 20, flags[rng.below(4) as usize] | rng.below(2));
}

#[test]
#[ignore]
fn s2dex_loads_textures() {
    check("load texture", 300, |rng, n64| {
        let mut list = Vec::new();
        for i in 0..rng.range(1, 12) {
            let at = HEAP + 0x100 * i as u32;
            random_txtr(rng, n64, at);
            if rng.below(4) == 0 {
                list.push((0xdb08_0000 | (rng.below(4) * 4), [0, 1, rng.next()][rng.below(3) as usize]));
            }
            if rng.below(3) == 0 {
                list.push((0xe700_0000, 0));
            }
            list.push((0x0500_0017, at));
        }
        list.push((0xdf00_0000, 0));
        list
    });
}

/// The microcode's RSP cycles for one command of each kind (after a setting command or not), against the RDP
/// commands it sent, as lines "kind cycles commands" (less a list that only ends), to fit `Work::s2dex`.
#[test]
#[ignore]
fn s2dex_cycles() {
    for (version, text, data) in builds().into_iter().filter(|(v, ..)| v.contains("2.08")) {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let run = |n64: &mut N64, list: &[(u32, u32)]| {
            put_list(n64, list);
            let commands = normal(&run_rsp(n64, &text).expect("the microcode finished")).len();
            (RSP_CYCLES.with(|c| c.get()), commands)
        };
        let (base, _) = run(&mut machine(&text, &data), &[(0xdf00_0000, 0)]);
        let kinds = ["scaled", "copy", "rectangle", "rotated", "sprite", "texture", "texture_rectangle",
            "texture_sprite", "other"];
        for kind in kinds {
            for _ in 0..300 {
                let mut n64 = machine(&text, &data);
                let mut list = Vec::new();
                if rng.below(2) == 0 {
                    random_object_state(&mut rng, &mut n64, &mut list, HEAP + 0x200);
                }
                let (before, _) = run(&mut n64.clone(), &[list.clone(), vec![(0xdf00_0000, 0)]].concat());
                random_txtr(&mut rng, &mut n64, HEAP);
                random_sprite(&mut rng, &mut n64, HEAP + 24);
                random_bg(&mut rng, &mut n64, HEAP + 0x100, kind == "scaled");
                list.push(match kind {
                    "scaled" => (0x0900_0000, HEAP + 0x100),
                    "copy" => (0x0a00_0000, HEAP + 0x100),
                    "rectangle" => (0x0100_0000, HEAP + 24),
                    "rotated" => (0xda00_0000, HEAP + 24),
                    "sprite" => (0x0200_0000, HEAP + 24),
                    "texture" => (0x0500_0017, HEAP),
                    "texture_rectangle" => (0x0700_002f, HEAP),
                    "texture_sprite" => (0x0600_002f, HEAP),
                    _ => (0xfa00_0000, rng.next()),
                });
                list.push((0xdf00_0000, 0));
                let without = [&list[..list.len() - 2], &list[list.len() - 1..]].concat();
                let (_, before_commands) = run(&mut n64.clone(), &without);
                let (cycles, commands) = run(&mut n64, &list);
                println!("{version} {kind} {} {}", cycles.saturating_sub(before.max(base)), commands - before_commands);
            }
        }
    }
}
