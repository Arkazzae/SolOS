//! The 2A03's APU: two pulse channels, a triangle, noise and the delta
//! modulation channel (DMC), driven by the frame counter, mixed as the
//! console mixes them, and resampled to 48 kHz for the host.
//!
//! Most CPU cycles change nothing the APU puts out: they only count down
//! to the next that does (a channel's timer running out where that changes
//! its output, the frame counter's step, the end of a sample). There the
//! cycles before are made up at once and that cycle runs in full, so the
//! APU does what it would cycle by cycle.

use savestate::state;

/// Lengths a length counter loads, by the index written.
const LENGTHS: [u8; 32] = [
    10, 254, 20, 2, 40, 4, 80, 6, 160, 8, 60, 10, 14, 12, 26, 14, 12, 16, 24, 18, 48, 20, 96, 22, 192, 24, 72, 26, 16,
    28, 32, 30,
];
const DUTIES: [u8; 4] = [0b0000_0001, 0b0000_0011, 0b0000_1111, 0b1111_1100];
const TRIANGLE: [u8; 32] = [
    15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
];
/// Noise and DMC periods, in CPU cycles (NTSC).
const NOISE_PERIODS: [u16; 16] = [4, 8, 16, 32, 64, 96, 128, 160, 202, 254, 380, 508, 762, 1016, 2034, 4068];
const DMC_PERIODS: [u16; 16] = [428, 380, 340, 320, 286, 254, 226, 214, 190, 160, 142, 128, 106, 84, 72, 54];

/// The console's non-linear mix of the pulses (by their summed output) and
/// of the triangle, noise and DMC (by 3 t + 2 n + d), worked out once.
const PULSE_MIX: [f32; 31] = {
    let mut table = [0.0; 31];
    let mut i = 1;
    while i < 31 {
        table[i] = 95.52 / (8128.0 / i as f32 + 100.0);
        i += 1;
    }
    table
};
const TND_MIX: [f32; 203] = {
    let mut table = [0.0; 203];
    let mut i = 1;
    while i < 203 {
        table[i] = 163.67 / (24329.0 / i as f32 + 100.0);
        i += 1;
    }
    table
};

/// The rate of the samples given to the host, and the CPU clock (NTSC):
/// 21477272 / 12 Hz, so a sample every 21477272 / (12 * 48000) cycles.
pub const SAMPLE_HZ: u32 = 48_000;
const PHASE_STEP: u32 = SAMPLE_HZ * 12;
const PHASE_WRAP: u32 = 21_477_272;

/// The console's high-pass filters (about 90 and 440 Hz) as one-pole
/// filters at the sample rate: each output is a * (last output + input -
/// last input).
const HIGH_PASS: [f32; 2] = [high_pass(90.0), high_pass(440.0)];

const fn high_pass(hz: f32) -> f32 {
    let rc = 1.0 / (2.0 * std::f32::consts::PI * hz);
    rc / (rc + 1.0 / SAMPLE_HZ as f32)
}

/// The frame counter's clocks: envelopes and the triangle's linear counter
/// every quarter frame, lengths and sweeps every half.
const QUARTER: u8 = 1;
const HALF: u8 = 2;

/// The frame counter's steps, in CPU cycles of its sequence.
const STEPS_4: [u32; 6] = [7457, 14913, 22371, 29828, 29829, 29830];
const STEPS_5: [u32; 5] = [7457, 14913, 22371, 37281, 37282];

/// A length counter, with its reload and halt flag taking effect after the
/// half-frame clock of the cycle they are written in, as on the chip.
#[derive(Clone, Copy, Default)]
struct Length {
    count: u8,
    halt: bool,
    enabled: bool,
    reload: Option<u8>,
    new_halt: Option<bool>,
}

state!(Length { count, halt, enabled, reload, new_halt });
state!(Envelope { start, constant, period, divider, decay, looping });
state!(Pulse {
    first, duty, step, period, timer, envelope, length, sweep_enabled, sweep_period, sweep_negate, sweep_shift,
    sweep_divider, sweep_reload,
});
state!(Triangle { period, timer, step, length, linear, linear_period, linear_reload, control });
state!(Noise { period, timer, short, shift, envelope, length });
state!(Dmc { irq_enabled, looping, period, timer, level, sample_addr, sample_len, addr, remaining, buffer, shift, bits,
    silence, irq });
// The samples of the frame being made are not part of the state.
state!(Apu {
    pulse, triangle, noise, dmc, five_step, irq_inhibit, frame_irq, frame_cycle, pending_frame, odd, length_written,
    next_step, outputs, level, dirty, sum, count, counted, phase, high_pass, countdown, skipped,
});

/// Runs a timer that counts down every clock and reloads on the clock it
/// is at 0 for `clocks` clocks; how many times it ran out.
fn run_timer(timer: &mut u16, reload: u16, clocks: u32) -> u32 {
    let t = u32::from(*timer);
    if clocks <= t {
        *timer = (t - clocks) as u16;
        return 0;
    }
    let length = u32::from(reload) + 1;
    let after = clocks - t - 1;
    *timer = (u32::from(reload) - after % length) as u16;
    1 + after / length
}

impl Length {
    fn clock(&mut self) {
        if !self.halt && self.count > 0 {
            self.count -= 1;
        }
    }

    fn write(&mut self, index: u8) {
        if self.enabled {
            self.reload = Some(LENGTHS[usize::from(index)]);
        }
    }

    /// Applies writes from this cycle; a reload lost to a clock in the same
    /// cycle when the counter was not empty.
    fn settle(&mut self, clocked: bool, before: u8) {
        if let Some(length) = self.reload.take()
            && !(clocked && before != 0)
        {
            self.count = length;
        }
        if let Some(halt) = self.new_halt.take() {
            self.halt = halt;
        }
    }

    fn enable(&mut self, on: bool) {
        self.enabled = on;
        if !on {
            self.count = 0;
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Envelope {
    start: bool,
    constant: bool,
    period: u8,
    divider: u8,
    decay: u8,
    looping: bool,
}

impl Envelope {
    fn write(&mut self, value: u8) {
        self.looping = value & 0x20 != 0;
        self.constant = value & 0x10 != 0;
        self.period = value & 0x0f;
    }

    fn clock(&mut self) {
        if self.start {
            self.start = false;
            self.decay = 15;
            self.divider = self.period;
        } else if self.divider == 0 {
            self.divider = self.period;
            if self.decay > 0 {
                self.decay -= 1;
            } else if self.looping {
                self.decay = 15;
            }
        } else {
            self.divider -= 1;
        }
    }

    fn volume(&self) -> u8 {
        if self.constant { self.period } else { self.decay }
    }
}

#[derive(Clone, Copy, Default)]
struct Pulse {
    /// Pulse 1 negates its sweep with one's complement, pulse 2 two's.
    first: bool,
    duty: u8,
    step: u8,
    period: u16,
    timer: u16,
    envelope: Envelope,
    length: Length,
    sweep_enabled: bool,
    sweep_period: u8,
    sweep_negate: bool,
    sweep_shift: u8,
    sweep_divider: u8,
    sweep_reload: bool,
}

impl Pulse {
    fn write(&mut self, reg: u16, value: u8) {
        match reg {
            0 => {
                self.duty = value >> 6;
                self.length.new_halt = Some(value & 0x20 != 0);
                self.envelope.write(value);
            }
            1 => {
                self.sweep_enabled = value & 0x80 != 0;
                self.sweep_period = value >> 4 & 7;
                self.sweep_negate = value & 0x08 != 0;
                self.sweep_shift = value & 7;
                self.sweep_reload = true;
            }
            2 => self.period = self.period & 0x700 | u16::from(value),
            _ => {
                self.period = self.period & 0xff | u16::from(value & 7) << 8;
                self.length.write(value >> 3);
                self.step = 0;
                self.envelope.start = true;
            }
        }
    }

    /// Every other CPU cycle; true when the output may have changed.
    fn clock_timer(&mut self) -> bool {
        if self.timer == 0 {
            self.timer = self.period;
            self.step = (self.step + 1) & 7;
            true
        } else {
            self.timer -= 1;
            false
        }
    }

    fn target(&self) -> u16 {
        let change = self.period >> self.sweep_shift;
        if self.sweep_negate {
            self.period.saturating_sub(change + u16::from(self.first))
        } else {
            self.period + change
        }
    }

    fn muted(&self) -> bool {
        self.period < 8 || self.target() > 0x7ff
    }

    fn clock_sweep(&mut self) {
        if self.sweep_divider == 0 && self.sweep_enabled && self.sweep_shift > 0 && !self.muted() {
            self.period = self.target();
        }
        if self.sweep_divider == 0 || self.sweep_reload {
            self.sweep_divider = self.sweep_period;
            self.sweep_reload = false;
        } else {
            self.sweep_divider -= 1;
        }
    }

    fn output(&self) -> u8 {
        if self.length.count == 0 || self.muted() || DUTIES[usize::from(self.duty)] >> (7 - self.step) & 1 == 0 {
            0
        } else {
            self.envelope.volume()
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Triangle {
    period: u16,
    timer: u16,
    step: u8,
    length: Length,
    linear: u8,
    linear_period: u8,
    linear_reload: bool,
    control: bool,
}

impl Triangle {
    fn write(&mut self, reg: u16, value: u8) {
        match reg {
            0 => {
                self.control = value & 0x80 != 0;
                self.length.new_halt = Some(self.control);
                self.linear_period = value & 0x7f;
            }
            2 => self.period = self.period & 0x700 | u16::from(value),
            3 => {
                self.period = self.period & 0xff | u16::from(value & 7) << 8;
                self.length.write(value >> 3);
                self.linear_reload = true;
            }
            _ => {}
        }
    }

    /// Every CPU cycle; the sequence only moves while both counters run.
    /// True when the output may have changed.
    fn clock_timer(&mut self) -> bool {
        if self.timer == 0 {
            self.timer = self.period;
            if self.linear > 0 && self.length.count > 0 {
                self.step = (self.step + 1) & 31;
                return true;
            }
        } else {
            self.timer -= 1;
        }
        false
    }

    fn clock_linear(&mut self) {
        if self.linear_reload {
            self.linear = self.linear_period;
        } else if self.linear > 0 {
            self.linear -= 1;
        }
        if !self.control {
            self.linear_reload = false;
        }
    }

    fn output(&self) -> u8 {
        // Periods under 2 would play far above hearing; the chip's output
        // there is a constant level, which this keeps at the middle.
        if self.period < 2 { 7 } else { TRIANGLE[usize::from(self.step)] }
    }
}

#[derive(Clone, Copy, Default)]
struct Noise {
    period: u16,
    timer: u16,
    short: bool,
    shift: u16,
    envelope: Envelope,
    length: Length,
}

impl Noise {
    fn write(&mut self, reg: u16, value: u8) {
        match reg {
            0 => {
                self.length.new_halt = Some(value & 0x20 != 0);
                self.envelope.write(value);
            }
            2 => {
                self.short = value & 0x80 != 0;
                self.period = NOISE_PERIODS[usize::from(value & 0x0f)];
            }
            3 => {
                self.length.write(value >> 3);
                self.envelope.start = true;
            }
            _ => {}
        }
    }

    /// Every CPU cycle, the period being in CPU cycles. True when the
    /// output may have changed.
    fn clock_timer(&mut self) -> bool {
        if self.timer == 0 {
            self.timer = self.period - 1;
            let tap = if self.short { 6 } else { 1 };
            let feedback = (self.shift ^ self.shift >> tap) & 1;
            self.shift = self.shift >> 1 | feedback << 14;
            true
        } else {
            self.timer -= 1;
            false
        }
    }

    fn output(&self) -> u8 {
        if self.length.count == 0 || self.shift & 1 != 0 { 0 } else { self.envelope.volume() }
    }
}

#[derive(Clone, Copy, Default)]
struct Dmc {
    irq_enabled: bool,
    looping: bool,
    period: u16,
    timer: u16,
    level: u8,
    sample_addr: u16,
    sample_len: u16,
    /// The memory reader.
    addr: u16,
    remaining: u16,
    buffer: Option<u8>,
    /// The output unit.
    shift: u8,
    bits: u8,
    silence: bool,
    irq: bool,
}

impl Dmc {
    fn write(&mut self, reg: u16, value: u8) {
        match reg {
            0 => {
                self.irq_enabled = value & 0x80 != 0;
                if !self.irq_enabled {
                    self.irq = false;
                }
                self.looping = value & 0x40 != 0;
                self.period = DMC_PERIODS[usize::from(value & 0x0f)];
            }
            1 => self.level = value & 0x7f,
            2 => self.sample_addr = 0xc000 | u16::from(value) << 6,
            _ => self.sample_len = u16::from(value) << 4 | 1,
        }
    }

    fn restart(&mut self) {
        self.addr = self.sample_addr;
        self.remaining = self.sample_len;
    }

    /// Every CPU cycle, the period being in CPU cycles. True when the
    /// output may have changed.
    fn clock_timer(&mut self) -> bool {
        if self.timer > 0 {
            self.timer -= 1;
            return false;
        }
        self.timer = self.period - 1;
        if !self.silence {
            if self.shift & 1 != 0 {
                if self.level <= 125 {
                    self.level += 2;
                }
            } else if self.level >= 2 {
                self.level -= 2;
            }
        }
        self.shift >>= 1;
        if self.bits > 0 {
            self.bits -= 1;
        }
        if self.bits == 0 {
            self.bits = 8;
            match self.buffer.take() {
                Some(byte) => {
                    self.shift = byte;
                    self.silence = false;
                }
                None => self.silence = true,
            }
        }
        true
    }

    /// The address the reader wants a byte from, if it does.
    fn wants(&self) -> Option<u16> {
        (self.buffer.is_none() && self.remaining > 0).then_some(self.addr)
    }

    fn fill(&mut self, byte: u8) {
        self.buffer = Some(byte);
        self.addr = self.addr.checked_add(1).unwrap_or(0x8000);
        self.remaining -= 1;
        if self.remaining == 0 {
            if self.looping {
                self.restart();
            } else if self.irq_enabled {
                self.irq = true;
            }
        }
    }
}

#[derive(Clone)]
pub struct Apu {
    pulse: [Pulse; 2],
    triangle: Triangle,
    noise: Noise,
    dmc: Dmc,
    /// The frame counter: 5-step mode, IRQ inhibited, the CPU cycle in its
    /// sequence, and a write to $4017 waiting to take effect.
    five_step: bool,
    irq_inhibit: bool,
    frame_irq: bool,
    frame_cycle: u32,
    pending_frame: Option<(u8, u8)>,
    /// The cycle of the sequence that next does something.
    next_step: u32,
    odd: bool,
    /// A length counter was written this cycle.
    length_written: bool,
    /// Each channel's output (pulses, triangle, noise; the DMC's is its
    /// level), the mix of them, and whether one changed since.
    outputs: [u8; 4],
    level: f32,
    dirty: bool,

    /// Resampling to 48 kHz: the mix summed over the CPU cycles of the
    /// sample being made, and the filters' state.
    sum: f32,
    count: u32,
    counted: u32,
    phase: u32,
    high_pass: [(f32, f32); 2],
    pub samples: Vec<i16>,

    /// Cycles to the next one that must run in full, and those passed over
    /// since the last that did.
    countdown: u32,
    skipped: u32,
}

impl Apu {
    pub fn new() -> Apu {
        let mut apu = Apu {
            pulse: [Pulse { first: true, ..Pulse::default() }, Pulse::default()],
            triangle: Triangle::default(),
            noise: Noise {
                period: NOISE_PERIODS[0],
                timer: 0,
                short: false,
                shift: 1,
                envelope: Envelope::default(),
                length: Length::default(),
            },
            dmc: Dmc { period: DMC_PERIODS[0], bits: 8, silence: true, ..Dmc::default() },
            five_step: false,
            irq_inhibit: false,
            frame_irq: false,
            frame_cycle: 0,
            pending_frame: None,
            next_step: STEPS_4[0],
            odd: false,
            length_written: false,
            outputs: [0; 4],
            level: 0.0,
            dirty: true,
            sum: 0.0,
            count: 0,
            counted: 0,
            phase: 0,
            high_pass: [(0.0, 0.0); 2],
            samples: Vec::with_capacity(1024),
            countdown: 1,
            skipped: 0,
        };
        apu.refresh();
        apu
    }

    /// The reset button silences the channels.
    pub fn reset(&mut self) {
        self.write(0x4015, 0);
        self.write(0x4017, if self.irq_inhibit { 0x40 } else { 0 } | if self.five_step { 0x80 } else { 0 });
    }

    pub fn irq(&self) -> bool {
        self.frame_irq || self.dmc.irq
    }

    /// The address the DMC wants a sample byte from; the CPU stops for it.
    pub fn dmc_wants(&self) -> Option<u16> {
        self.dmc.wants()
    }

    pub fn dmc_fill(&mut self, byte: u8) {
        self.dmc.fill(byte);
    }

    /// $4015: which length counters run, DMC activity and the IRQ flags.
    /// Reading clears the frame IRQ.
    pub fn read_status(&mut self) -> u8 {
        let mut status = 0;
        for (i, length) in [self.pulse[0].length, self.pulse[1].length, self.triangle.length, self.noise.length]
            .iter()
            .enumerate()
        {
            if length.count > 0 {
                status |= 1 << i;
            }
        }
        if self.dmc.remaining > 0 {
            status |= 0x10;
        }
        if self.frame_irq {
            status |= 0x40;
        }
        if self.dmc.irq {
            status |= 0x80;
        }
        self.frame_irq = false;
        status
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        self.catch_up();
        self.write_register(addr, value);
        self.refresh();
        // The next cycle settles the write; it runs in full.
        self.countdown = 1;
    }

    /// Every channel's output again, after something other than its timer
    /// changed it.
    fn refresh(&mut self) {
        self.outputs = [self.pulse[0].output(), self.pulse[1].output(), self.triangle.output(), self.noise.output()];
        self.dirty = true;
    }

    fn write_register(&mut self, addr: u16, value: u8) {
        self.length_written = true;
        match addr {
            0x4000..=0x4007 => self.pulse[usize::from(addr >> 2 & 1)].write(addr & 3, value),
            0x4008..=0x400b => self.triangle.write(addr & 3, value),
            0x400c..=0x400f => self.noise.write(addr & 3, value),
            0x4010..=0x4013 => self.dmc.write(addr & 3, value),
            0x4015 => {
                self.pulse[0].length.enable(value & 1 != 0);
                self.pulse[1].length.enable(value & 2 != 0);
                self.triangle.length.enable(value & 4 != 0);
                self.noise.length.enable(value & 8 != 0);
                self.dmc.irq = false;
                if value & 0x10 == 0 {
                    self.dmc.remaining = 0;
                } else if self.dmc.remaining == 0 {
                    self.dmc.restart();
                }
            }
            0x4017 => {
                self.irq_inhibit = value & 0x40 != 0;
                if self.irq_inhibit {
                    self.frame_irq = false;
                }
                // The new mode starts 3 or 4 CPU cycles later, on an APU cycle.
                self.pending_frame = Some((value, if self.odd { 4 } else { 3 }));
            }
            _ => {}
        }
    }

    fn quarter_frame(&mut self) {
        self.pulse[0].envelope.clock();
        self.pulse[1].envelope.clock();
        self.noise.envelope.clock();
        self.triangle.clock_linear();
    }

    fn half_frame(&mut self) {
        self.pulse[0].length.clock();
        self.pulse[1].length.clock();
        self.triangle.length.clock();
        self.noise.length.clock();
        self.pulse[0].clock_sweep();
        self.pulse[1].clock_sweep();
    }

    /// One CPU cycle: counted down, or run in full.
    #[inline(always)]
    pub fn tick(&mut self) {
        if self.countdown > 1 {
            self.countdown -= 1;
            self.skipped += 1;
            return;
        }
        self.catch_up();
        self.cycle();
        self.countdown = self.until_event();
    }

    /// Makes up the cycles passed over, in which no timer ran out where it
    /// changes an output, no frame counter step came and no sample ended.
    #[inline(never)]
    fn catch_up(&mut self) {
        let k = std::mem::take(&mut self.skipped);
        if k == 0 {
            return;
        }
        self.frame_cycle += k;
        let triangle = &mut self.triangle;
        let runs = run_timer(&mut triangle.timer, triangle.period, k);
        if triangle.linear > 0 && triangle.length.count > 0 {
            triangle.step = ((u32::from(triangle.step) + runs) & 31) as u8;
        }
        let noise = &mut self.noise;
        for _ in 0..run_timer(&mut noise.timer, noise.period - 1, k) {
            let tap = if noise.short { 6 } else { 1 };
            let feedback = (noise.shift ^ noise.shift >> tap) & 1;
            noise.shift = noise.shift >> 1 | feedback << 14;
        }
        self.dmc.timer -= k as u16;
        // The pulses are clocked on every other cycle.
        let clocks = if self.odd { (k + 1) / 2 } else { k / 2 };
        for pulse in &mut self.pulse {
            let runs = run_timer(&mut pulse.timer, pulse.period, clocks);
            pulse.step = ((u32::from(pulse.step) + runs) & 7) as u8;
        }
        self.odd ^= k & 1 != 0;
        self.count += k;
        self.phase += k * PHASE_STEP;
    }

    /// Cycles until the next that must run in full: one that changes an
    /// output (a timer running out on a channel that sounds and moves), the
    /// frame counter's step, the end of a sample; the next, after a write.
    fn until_event(&self) -> u32 {
        if self.pending_frame.is_some() || self.length_written {
            return 1;
        }
        let mut n = self.next_step - self.frame_cycle;
        n = n.min((PHASE_WRAP - self.phase).div_ceil(PHASE_STEP));
        n = n.min(u32::from(self.dmc.timer) + 1);
        let t = &self.triangle;
        if t.period >= 2 && t.linear > 0 && t.length.count > 0 {
            n = n.min(u32::from(t.timer) + 1);
        }
        let noise = &self.noise;
        if noise.length.count > 0 && noise.envelope.volume() > 0 {
            n = n.min(u32::from(noise.timer) + 1);
        }
        for p in &self.pulse {
            if p.length.count > 0 && p.envelope.volume() > 0 && !p.muted() {
                let timer = u32::from(p.timer);
                n = n.min(if self.odd { 2 * timer + 1 } else { 2 * timer + 2 });
            }
        }
        n
    }

    /// One CPU cycle in full. A channel whose timer steps gets its output
    /// again; the frame counter's clocks and length counter writes change
    /// them all.
    fn cycle(&mut self) {
        if self.length_written {
            self.length_written = false;
            let before = [self.pulse[0].length.count, self.pulse[1].length.count, self.triangle.length.count,
                self.noise.length.count];
            let half = self.clock_frame_counter() & HALF != 0;
            self.pulse[0].length.settle(half, before[0]);
            self.pulse[1].length.settle(half, before[1]);
            self.triangle.length.settle(half, before[2]);
            self.noise.length.settle(half, before[3]);
            self.refresh();
        } else if self.clock_frame_counter() != 0 {
            self.refresh();
        }

        if self.triangle.clock_timer() {
            self.set_output(2, self.triangle.output());
        }
        if self.noise.clock_timer() {
            self.set_output(3, self.noise.output());
        }
        if self.dmc.clock_timer() {
            self.dirty = true;
        }
        if self.odd {
            for i in 0..2 {
                if self.pulse[i].clock_timer() {
                    self.set_output(i, self.pulse[i].output());
                }
            }
        }
        self.odd = !self.odd;
        self.mix();
    }

    /// A channel's output; the mix is worked out again only if it changed.
    fn set_output(&mut self, channel: usize, output: u8) {
        if self.outputs[channel] != output {
            self.outputs[channel] = output;
            self.dirty = true;
        }
    }

    /// Steps the frame counter; QUARTER and HALF for the frames it clocked.
    #[inline(always)]
    fn clock_frame_counter(&mut self) -> u8 {
        // Most cycles do nothing.
        if self.pending_frame.is_none() && self.frame_cycle + 1 < self.next_step {
            self.frame_cycle += 1;
            return 0;
        }
        self.step_frame_counter()
    }

    #[inline(never)]
    fn step_frame_counter(&mut self) -> u8 {
        let mut clocked = 0;
        if let Some((value, delay)) = self.pending_frame {
            if delay == 0 {
                self.pending_frame = None;
                self.five_step = value & 0x80 != 0;
                self.frame_cycle = 0;
                if self.five_step {
                    self.quarter_frame();
                    self.half_frame();
                    clocked = QUARTER | HALF;
                }
            } else {
                self.pending_frame = Some((value, delay - 1));
            }
        }
        self.frame_cycle += 1;
        match (self.five_step, self.frame_cycle) {
            (_, 7457) | (_, 22371) => {
                self.quarter_frame();
                clocked |= QUARTER;
            }
            (_, 14913) | (false, 29829) | (true, 37281) => {
                self.quarter_frame();
                self.half_frame();
                clocked |= QUARTER | HALF;
            }
            _ => {}
        }
        if !self.five_step && (29828..=29830).contains(&self.frame_cycle) && !self.irq_inhibit {
            self.frame_irq = true;
        }
        if (!self.five_step && self.frame_cycle == 29830) || self.frame_cycle == 37282 {
            self.frame_cycle = 0;
        }
        let steps: &[u32] = if self.five_step { &STEPS_5 } else { &STEPS_4 };
        self.next_step = steps.iter().copied().find(|&s| s > self.frame_cycle).unwrap_or(u32::MAX);
        clocked
    }

    /// The console's mix, summed into the 48 kHz sample being made; each
    /// finished sample goes through the console's high-pass filters (about
    /// 90 and 440 Hz).
    fn mix(&mut self) {
        self.count += 1;
        if self.dirty {
            // The cycles before this one had the last level.
            self.add_level(self.count - 1);
            self.dirty = false;
            let [p1, p2, t, n] = self.outputs;
            let tnd = 3 * usize::from(t) + 2 * usize::from(n) + usize::from(self.dmc.level);
            self.level = PULSE_MIX[usize::from(p1 + p2)] + TND_MIX[tnd];
        }
        self.phase += PHASE_STEP;
        if self.phase >= PHASE_WRAP {
            self.sample();
        }
    }

    /// Adds the level to the sum for the cycles since it was last added, up
    /// to the given one: the sum is of each level times the cycles it lasted.
    fn add_level(&mut self, upto: u32) {
        self.sum += self.level * (upto - self.counted) as f32;
        self.counted = upto;
    }

    /// A finished 48 kHz sample, through the high-pass filters.
    #[inline(never)]
    fn sample(&mut self) {
        self.phase -= PHASE_WRAP;
        self.add_level(self.count);
        let mut x = self.sum / self.count as f32;
        self.sum = 0.0;
        self.count = 0;
        self.counted = 0;
        for (i, a) in HIGH_PASS.into_iter().enumerate() {
            let (prev_x, prev_y) = self.high_pass[i];
            let y = a * (prev_y + x - prev_x);
            self.high_pass[i] = (x, y);
            x = y;
        }
        // Twice the full mix's swing, which loud games come near.
        self.samples.push((x * 65536.0).clamp(-32768.0, 32767.0) as i16);
    }
}
