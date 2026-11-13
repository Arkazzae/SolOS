//! The video interface: its registers, the timing of lines and fields
//! they set, the interrupt at the half-line V_INTR names, and the picture,
//! taken from the frame buffer in RDRAM at the end of each field. The
//! picture is the part of the frame buffer the video interface shows,
//! unscaled; its filters (anti-aliasing, dither filter, gamma) are not
//! applied.
//!
//! The lines are not events of their own: the event comes at the line
//! that interrupts and at the end of the field, and the line being sent
//! (V_CURRENT) is counted back from the next event's line by the time left
//! until it, so a read sees what an event at every line start would show.

use crate::{CPU_HZ, N64, Region, VI, VI_INTR};
use savestate::state;

/// The video interface's clock: 13.6 of it to a period of the colour
/// subcarrier on NTSC and PAL-M, 11.2 on PAL (n64brew, Video Interface).
const NTSC_HZ: u64 = 48_681_812;
const PAL_HZ: u64 = 49_656_530;

#[derive(Clone)]
pub struct Vi {
    /// The registers from VI_CTRL to VI_STAGED_DATA, as written (but
    /// V_CURRENT, which is counted from the time: line_at).
    regs: [u32; 16],
    /// The line before the one at which the next event comes (which starts
    /// at events[VI]), the field (which alternates when interlaced), and
    /// fields sent since power on. A line ends where the next starts, so
    /// with an event at every line this is the line being sent.
    line: u32,
    field: bool,
    pub fields: u64,
    /// The CPU cycles of each line until the next event.
    line_cycles: u64,
    pal: bool,
    /// The picture of the last field, XRGB8888.
    pub picture: Vec<u32>,
    pub picture_width: usize,
    pub picture_height: usize,
}

state!(Vi { regs, line, field, fields, line_cycles, pal });

const CTRL: usize = 0;
const ORIGIN: usize = 1;
const WIDTH: usize = 2;
const V_INTR: usize = 3;
const V_TOTAL: usize = 6;
const H_TOTAL: usize = 7;
const H_VIDEO: usize = 9;
const V_VIDEO: usize = 10;
const X_SCALE: usize = 12;
const Y_SCALE: usize = 13;

impl Vi {
    pub fn new() -> Vi {
        Vi {
            regs: [0; 16],
            line: 0,
            field: false,
            fields: 0,
            line_cycles: 0,
            pal: false,
            picture: Vec::new(),
            picture_width: 0,
            picture_height: 0,
        }
    }

    /// Lines in a field: half of V_TOTAL + 1 half-lines; until a game sets
    /// it, as many as the standard has.
    fn lines(&self) -> u32 {
        match self.regs[V_TOTAL] & 0x3ff {
            0..=0x100 if self.pal => 312,
            0..=0x100 => 262,
            total => (total + 1) / 2,
        }
    }

    /// The cycles of a line: H_TOTAL + 1 quarter pixels, which are clocks
    /// of the video interface.
    fn timing(&self) -> u64 {
        let quarters = match self.regs[H_TOTAL] & 0xfff {
            0..=0x400 if self.pal => 3178,
            0..=0x400 => 3094,
            total => u64::from(total) + 1,
        };
        quarters * CPU_HZ / if self.pal { PAL_HZ } else { NTSC_HZ }
    }

    /// The line being sent while an instruction runs at `now` (the cycle
    /// count with it, which an event due then follows), the next event at
    /// `next_at`: counted back from the event's line, by the lines that
    /// start from now on.
    fn line_at(&self, now: u64, next_at: u64) -> u32 {
        let later = next_at.saturating_sub(now) / self.line_cycles;
        self.line.saturating_sub(later as u32)
    }

    /// The next line after `line` at which the video interface does
    /// something: V_INTR's, if it comes in this field, or the end of the
    /// field (the line after the last; after the next one, when V_TOTAL
    /// was cut below the line being sent).
    fn next_line(&self, line: u32) -> u32 {
        let end = self.lines().max(line + 1);
        let intr = (self.regs[V_INTR] & 0x3fe) >> 1;
        if intr > line && intr < end { intr } else { end }
    }
}

impl N64 {
    pub(crate) fn power_on_vi(&mut self, region: Region) {
        self.vi.pal = region == Region::Pal;
        self.vi.line_cycles = self.vi.timing();
        let now = self.cycles;
        self.plan_vi(0, now);
    }

    pub(crate) fn read_vi(&self, addr: u32) -> u32 {
        let reg = (addr as usize >> 2) & 15;
        match reg {
            // The half-line, with the field in bit 0.
            4 => self.vi.line_at(self.cycles, self.events[VI]) << 1 | u32::from(self.vi.field),
            _ => self.vi.regs[reg],
        }
    }

    pub(crate) fn write_vi(&mut self, addr: u32, value: u32) {
        let reg = (addr as usize >> 2) & 15;
        let old = self.vi.regs[reg];
        match reg {
            4 => self.acknowledge(VI_INTR),
            _ => self.vi.regs[reg] = value,
        }
        let changed = old != self.vi.regs[reg];
        if changed && matches!(reg, V_INTR | V_TOTAL | H_TOTAL) {
            // The line being sent and when the next starts, as timed so far.
            let vi = &self.vi;
            let line = vi.line_at(self.cycles, self.events[VI]);
            let end = self.events[VI] - u64::from(vi.line - line) * vi.line_cycles;
            if vi.timing() != vi.line_cycles {
                // The new length counts from the next line on.
                self.vi.line = line;
                self.schedule(VI, end);
            } else {
                // The interrupt's line or the field's end may have moved.
                let start = end - self.vi.line_cycles;
                self.plan_vi(line, start);
            }
        }
    }

    /// A line of note starts (V_INTR's, or the first of a field): the
    /// interrupt if it is V_INTR's, and after the last, the next field,
    /// with the picture of the one that ended.
    pub(crate) fn vi_event(&mut self, at: u64) {
        let vi = &mut self.vi;
        let mut line = vi.line + 1;
        if line >= vi.lines() {
            line = 0;
            vi.fields += 1;
            // Interlaced (serrated) video alternates its fields.
            vi.field = vi.regs[CTRL] & 0x40 != 0 && !vi.field;
            self.take_picture();
        }
        if line << 1 == self.vi.regs[V_INTR] & 0x3fe {
            self.request(VI_INTR);
        }
        // H_TOTAL written since times the lines from this one on.
        self.vi.line_cycles = self.vi.timing();
        self.plan_vi(line, at);
    }

    /// Plans the next event after `line`, which started at `start`.
    fn plan_vi(&mut self, line: u32, start: u64) {
        let next = self.vi.next_line(line);
        self.vi.line = next - 1;
        let at = start + u64::from(next - line) * self.vi.line_cycles;
        self.schedule(VI, at);
    }

    /// The picture of the field that ended: taken by the RDP's drawing
    /// thread once it has drawn everything sent before (the lines it reads
    /// marked, so that nothing changes them first), or here.
    fn take_picture(&mut self) {
        let regs = self.vi.regs;
        if self.rdp_thread.drawing() {
            if let Some((addr, len)) = picture_reach(&regs) {
                self.rdp_thread.mark(addr, len);
            }
            self.rdp_thread.picture(regs);
            return;
        }
        if let Some((addr, len)) = picture_reach(&regs) {
            self.touch_rdram(addr, len);
        }
        let (width, height) = convert_picture(&regs, &self.rdram, &mut self.vi.picture);
        (self.vi.picture_width, self.vi.picture_height) = (width, height);
    }
}

/// The bytes of RDRAM a picture reads (start, length), with these
/// registers; None while video is off.
pub(crate) fn picture_reach(regs: &[u32; 16]) -> Option<(usize, usize)> {
    let (width, height, stride) = picture_size(regs)?;
    let bytes = if regs[CTRL] & 3 == 3 { 4 } else { 2 };
    Some(((regs[ORIGIN] & 0xff_ffff) as usize, ((height - 1) * stride + width) * bytes))
}

/// The picture's width and height, and the frame buffer's line length, in
/// pixels.
pub(crate) fn picture_size(regs: &[u32; 16]) -> Option<(usize, usize, usize)> {
    let kind = regs[CTRL] & 3;
    let (h_start, h_end) = (regs[H_VIDEO] >> 16 & 0x3ff, regs[H_VIDEO] & 0x3ff);
    let (v_start, v_end) = (regs[V_VIDEO] >> 16 & 0x3ff, regs[V_VIDEO] & 0x3ff);
    let width = (h_end.saturating_sub(h_start) * (regs[X_SCALE] & 0xfff)) >> 10;
    let height = ((v_end.saturating_sub(v_start) >> 1) * (regs[Y_SCALE] & 0xfff)) >> 10;
    let stride = regs[WIDTH] & 0xfff;
    (kind >= 2 && width != 0 && height != 0 && stride != 0).then_some((width as usize, height as usize, stride as usize))
}

/// Converts the part of the frame buffer that is shown into XRGB8888:
/// H_VIDEO's and V_VIDEO's active area (V_VIDEO in half-lines) scaled by
/// X_SCALE and Y_SCALE (2.10 fixed point, the step through the frame buffer
/// for each pixel or line shown). Its width and height, 0 while video is
/// off.
pub(crate) fn convert_picture(regs: &[u32; 16], rdram: &[u8], picture: &mut Vec<u32>) -> (usize, usize) {
    let Some((width, height, stride)) = picture_size(regs) else {
        picture.clear();
        return (0, 0);
    };
    picture.resize(width * height, 0);
    for (y, line) in picture.chunks_exact_mut(width).enumerate() {
        convert_line(regs, rdram, line, y, stride);
    }
    (width, height)
}

/// Line `y` of the picture, `line.len()` pixels, from a frame buffer of
/// `stride` pixels a line (as `convert_picture`).
pub(crate) fn convert_line(regs: &[u32; 16], rdram: &[u8], line: &mut [u32], y: usize, stride: usize) {
    let origin = (regs[ORIGIN] & 0xff_ffff) as usize;
    let size = if regs[CTRL] & 3 == 3 { 4 } else { 2 };
    let from = origin + size * y * stride;
    // RGBA5551, each channel widened to 8 bits.
    let rgba16 = |p: u32| {
        let channel = |c: u32| (c << 3 | c >> 2) & 0xff;
        channel(p >> 11 & 31) << 16 | channel(p >> 6 & 31) << 8 | channel(p >> 1 & 31)
    };
    match rdram.get(from..from + size * line.len()) {
        // The whole line in RDRAM.
        Some(source) if size == 4 => {
            for (pixel, p) in line.iter_mut().zip(source.chunks_exact(4)) {
                *pixel = u32::from_be_bytes(p.try_into().unwrap()) >> 8;
            }
        }
        Some(source) => {
            for (pixel, p) in line.iter_mut().zip(source.chunks_exact(2)) {
                *pixel = rgba16(u32::from(u16::from_be_bytes(p.try_into().unwrap())));
            }
        }
        // Running off RDRAM's end: what lies beyond is black.
        None => {
            for (x, pixel) in line.iter_mut().enumerate() {
                let i = from + size * x;
                *pixel = match rdram.get(i..i + size) {
                    Some(p) if size == 4 => u32::from_be_bytes(p.try_into().unwrap()) >> 8,
                    Some(p) => rgba16(u32::from(u16::from_be_bytes(p.try_into().unwrap()))),
                    None => 0,
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CTRL, H_TOTAL, V_INTR, V_TOTAL};
    use crate::{N64, VI_INTR};

    fn random(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    /// The video interface as it was timed before: an event at every line,
    /// H_TOTAL counting from the line after the one it is written in.
    struct EveryLine {
        line: u32,
        field: bool,
        fields: u64,
        line_cycles: u64,
        next: u64,
        interrupts: u64,
    }

    impl EveryLine {
        /// The events due by `now`.
        fn run(&mut self, n64: &N64, now: u64) {
            while self.next <= now {
                self.line += 1;
                if self.line >= n64.vi.lines() {
                    self.line = 0;
                    self.fields += 1;
                    self.field = n64.vi.regs[CTRL] & 0x40 != 0 && !self.field;
                }
                if self.line << 1 == n64.vi.regs[V_INTR] & 0x3fe {
                    self.interrupts += 1;
                }
                self.next += self.line_cycles;
            }
        }
    }

    /// V_CURRENT, the fields and the interrupts read between random steps
    /// and register writes, as an event at every line would have them.
    #[test]
    fn lines_counted_from_the_time_match_an_event_at_every_line() {
        for region in [b'E', b'P'] {
            let mut rom = vec![0; 0x1000];
            rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
            rom[0x3e] = region;
            let mut n64 = N64::new(&rom).unwrap();
            n64.set_rdp_thread(false);
            let mut now = n64.cycles;
            let line_cycles = n64.vi.timing();
            let mut old =
                EveryLine { line: 0, field: false, fields: 0, line_cycles, next: now + line_cycles, interrupts: 0 };
            let mut seed = 0x0f1e_1d05_2026_1103u64 + u64::from(region);
            let mut seen = 0;
            for step in 0..300_000 {
                // Anywhere in the next two lines; an instruction at `now`
                // runs before an event due then.
                now += 1 + random(&mut seed) % (2 * old.line_cycles);
                n64.cycles = now - 1;
                n64.run_events();
                n64.cycles = now;
                old.run(&n64, now - 1);
                let requested = n64.mi_intr & VI_INTR != 0;
                assert_eq!(requested, old.interrupts > seen, "step {step}: the interrupt");
                if requested {
                    n64.write_vi(0x0440_0010, 0);
                    seen = old.interrupts;
                }
                let current = n64.read_vi(0x0440_0010);
                assert_eq!(current, old.line << 1 | u32::from(old.field), "step {step}: V_CURRENT");
                assert_eq!(n64.vi.fields, old.fields, "step {step}: fields");
                let r = random(&mut seed);
                let (reg, value) = match r % 64 {
                    0..=3 => (V_INTR, (r >> 8) as u32 % 0x240),
                    4 | 5 => (V_TOTAL, [0, 0x20d, 0x271, 0x101 + (r >> 8) as u32 % 0x2ff][(r >> 20) as usize % 4]),
                    6 | 7 => (H_TOTAL, [0, 0xc15, 0xc7f, 0x401 + (r >> 8) as u32 % 0xbff][(r >> 20) as usize % 4]),
                    8 => (CTRL, 0x40 * ((r >> 8) as u32 & 1)),
                    _ => continue,
                };
                n64.write_vi(0x0440_0000 | (reg as u32) << 2, value);
                if reg == H_TOTAL {
                    old.line_cycles = n64.vi.timing();
                }
            }
            assert!(old.fields > 50 && seen > 20, "{} fields, {seen} interrupts", old.fields);
        }
    }
}
