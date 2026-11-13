//! Mega Drive audio: Sega PSG, YM2612 timers/DAC and a four-operator FM
//! synthesizer. FM phase and envelopes are an approximation: the chip's
//! logarithmic tables, SSG-EG, LFO and analogue ladder distortion are not
//! reproduced. All time bases use the console's master clock.

use savestate::state;

pub const SAMPLE_HZ: u32 = 48_000;
const NTSC_MASTER: u32 = 53_693_175;
const PAL_MASTER: u32 = 53_203_424;
const VOLUME: [f32; 16] = [
    1.0,
    0.7943282,
    0.6309574,
    0.5011872,
    0.39810717,
    0.31622776,
    0.25118864,
    0.19952624,
    0.15848932,
    0.12589253,
    0.1,
    0.07943282,
    0.06309573,
    0.05011872,
    0.039810717,
    0.0,
];

#[derive(Clone)]
struct Psg {
    period: [u16; 3],
    volume: [u8; 4],
    latch: u8,
    counters: [u16; 4],
    outputs: [bool; 4],
    noise: u8,
    lfsr: u16,
    divider: u32,
}
state!(Psg {
    period,
    volume,
    latch,
    counters,
    outputs,
    noise,
    lfsr,
    divider
});

impl Psg {
    fn new() -> Self {
        Self {
            period: [0; 3],
            volume: [15; 4],
            latch: 0,
            counters: [1; 4],
            outputs: [false; 4],
            noise: 0,
            lfsr: 0x8000,
            divider: 0,
        }
    }
    fn write(&mut self, n: u8) {
        let latched = n & 128 != 0;
        if latched {
            self.latch = (n >> 4) & 7;
        }
        let ch = usize::from(self.latch >> 1);
        if self.latch & 1 != 0 {
            self.volume[ch] = n & 15;
        } else if ch < 3 {
            self.period[ch] = if latched {
                (self.period[ch] & 0x3f0) | u16::from(n & 15)
            } else {
                (self.period[ch] & 15) | (u16::from(n & 63) << 4)
            };
        } else {
            self.noise = n & 7;
            self.lfsr = 0x8000;
            self.outputs[3] = false;
        }
    }
    fn advance(&mut self, master: u32) {
        self.divider += master;
        while self.divider >= 240 {
            self.divider -= 240;
            for ch in 0..3 {
                self.counters[ch] = self.counters[ch].saturating_sub(1);
                if self.counters[ch] == 0 {
                    self.counters[ch] = self.period[ch].max(1);
                    self.outputs[ch] = !self.outputs[ch];
                    if ch == 2 && self.noise & 3 == 3 && !self.outputs[ch] {
                        self.shift_noise();
                    }
                }
            }
            if self.noise & 3 != 3 {
                self.counters[3] = self.counters[3].saturating_sub(1);
                if self.counters[3] == 0 {
                    self.counters[3] = 0x20 << (self.noise & 3);
                    self.shift_noise();
                }
            }
        }
    }
    fn shift_noise(&mut self) {
        let feedback = if self.noise & 4 != 0 {
            (self.lfsr ^ (self.lfsr >> 3)) & 1
        } else {
            self.lfsr & 1
        };
        self.lfsr = (self.lfsr >> 1) | (feedback << 15);
        self.outputs[3] = self.lfsr & 1 != 0;
    }
    fn sample(&self) -> f32 {
        let mut n = 0.0;
        for ch in 0..4 {
            // Ultrasonic tone periods should not alias into the host's band.
            if ch < 3 && self.period[ch] < 2 {
                continue;
            }
            n += VOLUME[usize::from(self.volume[ch])] * if self.outputs[ch] { 1.0 } else { -1.0 };
        }
        n * 1100.0
    }
}

/// One cycle of the sine in 1024 steps and the point that closes it, for `Operator::sample`: with the two
/// neighbours interpolated it is within 5e-6 of the sine, far below one step of the 16-bit output, where the
/// `sin` it replaces was the largest cost of a frame that has the FM chip sounding.
const SINE_STEPS: usize = 1024;
fn sine_table() -> &'static [f32; SINE_STEPS + 1] {
    static TABLE: std::sync::OnceLock<[f32; SINE_STEPS + 1]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0.0; SINE_STEPS + 1];
        for (i, v) in table.iter_mut().enumerate() {
            *v = (i as f64 * std::f64::consts::TAU / SINE_STEPS as f64).sin() as f32;
        }
        table
    })
}

#[derive(Clone, Copy)]
struct Operator {
    phase: f64,
    step: f64,
    envelope: f32,
    stage: u8,
    key: bool,
    level: f32,
    sustain: f32,
    attack: f32,
    decay: f32,
    sustain_rate: f32,
    release: f32,
}
state!(Operator {
    phase,
    step,
    envelope,
    stage,
    key,
    level,
    sustain,
    attack,
    decay,
    sustain_rate,
    release
});
impl Operator {
    fn new() -> Self {
        Self {
            phase: 0.0,
            step: 0.0,
            envelope: 0.0,
            stage: 0,
            key: false,
            level: 1.0,
            sustain: 0.0,
            attack: 0.0,
            decay: 0.0,
            sustain_rate: 0.0,
            release: 0.0,
        }
    }
    fn key(&mut self, on: bool) {
        if on && !self.key {
            self.phase = 0.0;
            self.stage = 1;
        }
        if !on && self.key && self.stage != 0 {
            self.stage = 4;
        }
        self.key = on;
    }
    fn sample(&mut self, modulation: f32, sine: &[f32; SINE_STEPS + 1]) -> f32 {
        match self.stage {
            1 => {
                self.envelope += (1.5 - self.envelope) * self.attack;
                if self.envelope >= 1.0 {
                    self.envelope = 1.0;
                    self.stage = 2;
                }
            }
            2 => {
                self.envelope *= 1.0 - self.decay;
                if self.envelope <= self.sustain {
                    self.stage = 3;
                }
            }
            3 => self.envelope *= 1.0 - self.sustain_rate,
            4 => {
                self.envelope *= 1.0 - self.release;
                if self.envelope < 0.000015 {
                    self.envelope = 0.0;
                    self.stage = 0;
                }
            }
            _ => {}
        }
        self.phase = (self.phase + self.step).fract();
        if self.stage == 0 {
            return 0.0;
        }
        // The phase in cycles plus the modulation (radians) in cycles, wrapped into [0, 1).
        let cycles = self.phase as f32 + modulation * (1.0 / std::f32::consts::TAU);
        let position = (cycles - cycles.floor()) * SINE_STEPS as f32;
        let at = (position as usize).min(SINE_STEPS - 1);
        let (low, high) = (sine[at], sine[at + 1]);
        (low + (high - low) * (position - at as f32)) * self.envelope * self.level
    }
}

#[derive(Clone, Copy)]
struct Channel {
    previous: [f32; 2],
}
state!(Channel { previous });

#[derive(Clone)]
pub(crate) struct Sound {
    psg: Psg,
    registers: [u8; 512],
    address: [u8; 2],
    operators: [Operator; 24],
    channels: [Channel; 6],
    timer: [u32; 2],
    status: u8,
    busy: u32,
    sample_phase: u64,
    pal: bool,
    // Output belongs to the current frame, and is regenerated after resume.
    samples: Vec<i16>,
}
state!(Sound {
    psg,
    registers,
    address,
    operators,
    channels,
    timer,
    status,
    busy,
    sample_phase,
    pal
});

impl Sound {
    pub fn new() -> Self {
        Self {
            psg: Psg::new(),
            registers: [0; 512],
            address: [0; 2],
            operators: [Operator::new(); 24],
            channels: [Channel { previous: [0.0; 2] }; 6],
            timer: [0; 2],
            status: 0,
            busy: 0,
            sample_phase: 0,
            pal: false,
            samples: Vec::with_capacity(2000),
        }
    }
    pub fn reset(&mut self) {
        *self = Self::new();
    }
    /// The Z80 reset line also resets the YM2612. The VDP's PSG and the
    /// host's PCM clock continue running while a sound driver is replaced.
    pub fn reset_fm(&mut self) {
        self.registers.fill(0);
        self.address.fill(0);
        self.operators.fill(Operator::new());
        self.channels.fill(Channel { previous: [0.0; 2] });
        self.timer.fill(0);
        self.status = 0;
        self.busy = 0;
    }
    pub fn begin_frame(&mut self) {
        self.samples.clear();
    }
    pub fn audio(&self) -> &[i16] {
        &self.samples
    }
    pub fn validate(&self, pal: bool) -> bool {
        self.psg.latch < 8
            && self.psg.noise < 8
            && self.psg.volume.iter().all(|&n| n < 16)
            && self.psg.period.iter().all(|&n| n < 1024)
            && self.psg.divider < 240
            // The cartridge selects the clock, not the serialized flag.
            && self.sample_phase < u64::from(if pal { PAL_MASTER } else { NTSC_MASTER })
            && self.status & !3 == 0
            && self.busy <= 224
            && self.timer.iter().all(|&n| n < 256 * 1152 * 7)
            && self.operators.iter().all(|o| {
                o.phase.is_finite()
                    && (0.0..1.0).contains(&o.phase)
                    && o.step.is_finite()
                    && (0.0..100.0).contains(&o.step)
                    && o.stage <= 4
                    && [
                        o.envelope,
                        o.level,
                        o.sustain,
                        o.attack,
                        o.decay,
                        o.sustain_rate,
                        o.release,
                    ]
                    .iter()
                    .all(|n| n.is_finite() && (0.0..=1.0).contains(n))
            })
            && self.channels.iter().all(|c| {
                c.previous
                    .iter()
                    .all(|n| n.is_finite() && (-1.0..=1.0).contains(n))
            })
    }
    pub fn write_psg(&mut self, value: u8) {
        self.psg.write(value);
    }
    pub fn read_fm(&self) -> u8 {
        self.status | if self.busy != 0 { 0x80 } else { 0 }
    }
    pub fn write_fm(&mut self, port: u8, value: u8) {
        let bank = usize::from((port >> 1) & 1);
        if port & 1 == 0 {
            self.address[bank] = value;
            return;
        }
        let reg = usize::from(self.address[bank]) | bank << 8;
        let old = self.registers[reg];
        self.registers[reg] = value;
        self.busy = 32 * 7;
        if reg == 0x27 {
            if value & 0x10 != 0 {
                self.status &= !1;
            }
            if value & 0x20 != 0 {
                self.status &= !2;
            }
            for t in 0..2 {
                if value & (1 << t) != 0 && old & (1 << t) == 0 {
                    self.timer[t] = 0;
                }
            }
            self.update_channel(2);
        } else if reg == 0x28 {
            let mut ch = usize::from(value & 3);
            if ch == 3 {
                return;
            }
            if value & 4 != 0 {
                ch += 3;
            }
            for slot in 0..4 {
                self.operators[ch * 4 + slot].key(value & (0x10 << slot) != 0);
            }
        } else if reg & 0xfc >= 0x30 && reg & 0xfc <= 0x9c {
            let ch = (reg & 3) + bank * 3;
            if reg & 3 != 3 {
                self.update_channel(ch);
            }
        } else if (0xa0..=0xae).contains(&(reg & 255)) && reg & 3 != 3 {
            let ch = (reg & 3) + bank * 3;
            self.update_channel(ch);
            if bank == 0 && reg & 15 >= 8 {
                self.update_channel(2);
            }
        }
    }
    fn update_channel(&mut self, ch: usize) {
        let bank = if ch >= 3 { 256 } else { 0 };
        let c = ch % 3;
        let master = if self.pal { PAL_MASTER } else { NTSC_MASTER };
        for slot in 0..4 {
            let offset = [0, 8, 4, 12][slot] + c + bank;
            let (low, high) = if ch == 2 && slot < 3 && self.registers[0x27] & 0xc0 != 0 {
                // Three-slot mode frequencies are wired in the order 3,1,2.
                let special = [1, 2, 0][slot];
                (
                    self.registers[0xa8 + special],
                    self.registers[0xac + special],
                )
            } else {
                (
                    self.registers[bank + 0xa0 + c],
                    self.registers[bank + 0xa4 + c],
                )
            };
            let fnum = u16::from(low) | (u16::from(high & 7) << 8);
            let block = (high >> 3) & 7;
            let mul = self.registers[0x30 + offset] & 15;
            let multiplier = if mul == 0 { 0.5 } else { f64::from(mul) };
            let keycode = (block << 1) | ((fnum >> 9) as u8);
            let ks = keycode >> (3 - (self.registers[0x50 + offset] >> 6));
            let ar = self.registers[0x50 + offset] & 31;
            let dr = self.registers[0x60 + offset] & 31;
            let sr = self.registers[0x70 + offset] & 31;
            let rr = (self.registers[0x80 + offset] & 15) * 2 + 1;
            let sl = self.registers[0x80 + offset] >> 4;
            let op = &mut self.operators[ch * 4 + slot];
            op.step =
                f64::from(master) / (7.0 * 144.0) * f64::from(fnum) * f64::from(1u16 << block)
                    / (2_097_152.0 * f64::from(SAMPLE_HZ))
                    * multiplier;
            op.level = 2.0f32.powf(-f32::from(self.registers[0x40 + offset] & 127) / 8.0);
            op.sustain = if sl == 15 {
                0.00002
            } else {
                2.0f32.powf(-f32::from(sl) / 2.0)
            };
            op.attack = Self::rate(ar, ks, 0.0005);
            op.decay = Self::rate(dr, ks, 0.008);
            op.sustain_rate = Self::rate(sr, ks, 0.008);
            op.release = Self::rate(rr, ks, 0.008);
        }
    }
    fn rate(rate: u8, ks: u8, fastest: f32) -> f32 {
        if rate == 0 {
            return 0.0;
        }
        let effective = (u16::from(rate) * 2 + u16::from(ks)).min(63);
        let seconds = fastest * 2.0f32.powf((62.0 - f32::from(effective)) / 4.0);
        1.0 - (-1.0 / (seconds * SAMPLE_HZ as f32)).exp()
    }
    fn timer_period(&self, t: usize) -> u32 {
        if t == 0 {
            let value =
                (u16::from(self.registers[0x24]) << 2) | u16::from(self.registers[0x25] & 3);
            (1024 - u32::from(value)) * 72 * 7
        } else {
            (256 - u32::from(self.registers[0x26])) * 1152 * 7
        }
    }
    fn tick_timers(&mut self, master: u32) {
        self.busy = self.busy.saturating_sub(master);
        for t in 0..2 {
            if self.registers[0x27] & (1 << t) == 0 {
                continue;
            }
            self.timer[t] += master;
            let period = self.timer_period(t);
            while self.timer[t] >= period {
                self.timer[t] -= period;
                if self.registers[0x27] & (4 << t) != 0 {
                    self.status |= 1 << t;
                }
            }
        }
    }
    /// Advances by console master clocks (68000 clocks multiplied by seven).
    pub fn advance(&mut self, mut master_cycles: u32, pal: bool) {
        if self.pal != pal {
            self.pal = pal;
            for c in 0..6 {
                self.update_channel(c);
            }
        }
        let hz = u64::from(if pal { PAL_MASTER } else { NTSC_MASTER });
        while master_cycles != 0 {
            let to_sample = (hz - self.sample_phase).div_ceil(u64::from(SAMPLE_HZ)) as u32;
            let clocks = master_cycles.min(to_sample);
            master_cycles -= clocks;
            self.psg.advance(clocks);
            self.tick_timers(clocks);
            self.sample_phase += u64::from(clocks) * u64::from(SAMPLE_HZ);
            if self.sample_phase >= hz {
                self.sample_phase -= hz;
                self.sample();
            }
        }
    }
    fn sample(&mut self) {
        let sine = sine_table();
        let psg = self.psg.sample();
        let mut left = psg;
        let mut right = psg;
        for ch in 0..6 {
            let bank = if ch >= 3 { 256 } else { 0 };
            let c = ch % 3;
            let dac = ch == 5 && self.registers[0x2b] & 128 != 0;
            if !dac && self.operators[ch * 4..ch * 4 + 4].iter().all(|o| o.stage == 0) {
                // A channel with no sounding operator adds nothing. Its feedback memory still drains as
                // it would sample by sample. An idle operator's phase is not advanced: a key-on restarts
                // it from zero, so it can never be heard.
                let previous = self.channels[ch].previous;
                self.channels[ch].previous = [0.0, previous[0]];
                continue;
            }
            let n = if dac {
                (f32::from(self.registers[0x2a]) - 128.0) / 128.0
            } else {
                let algorithm = self.registers[bank + 0xb0 + c] & 7;
                let feedback = (self.registers[bank + 0xb0 + c] >> 3) & 7;
                let prev = self.channels[ch].previous;
                let fb = if feedback == 0 {
                    0.0
                } else {
                    (prev[0] + prev[1]) * f32::from(1u16 << feedback) / 64.0
                };
                let ops = &mut self.operators[ch * 4..ch * 4 + 4];
                let a = ops[0].sample(fb, sine);
                self.channels[ch].previous = [a, prev[0]];
                let b = ops[1].sample(
                    if matches!(algorithm, 0 | 3..=6) {
                        a * 4.0
                    } else {
                        0.0
                    },
                    sine,
                );
                let d_in = match algorithm {
                    0..=2 => {
                        let input = if algorithm == 0 {
                            b
                        } else if algorithm == 1 {
                            a + b
                        } else {
                            b
                        };
                        let d = ops[2].sample(input * 4.0, sine);
                        if algorithm == 2 { a + d } else { d }
                    }
                    3 => b + ops[2].sample(0.0, sine),
                    4 => ops[2].sample(0.0, sine),
                    5 => {
                        let d = ops[2].sample(a * 4.0, sine);
                        b + d + ops[3].sample(a * 4.0, sine)
                    }
                    _ => {
                        let d = ops[2].sample(0.0, sine);
                        (if algorithm == 6 { b + d } else { a + b + d }) + ops[3].sample(0.0, sine)
                    }
                };
                if algorithm >= 5 {
                    d_in
                } else {
                    let d = ops[3].sample(d_in * 4.0, sine);
                    if algorithm == 4 { b + d } else { d }
                }
            };
            let pan = self.registers[bank + 0xb4 + c];
            if pan & 128 != 0 {
                left += n * 2200.0;
            }
            if pan & 64 != 0 {
                right += n * 2200.0;
            }
        }
        self.samples.push(left.clamp(-32768.0, 32767.0) as i16);
        self.samples.push(right.clamp(-32768.0, 32767.0) as i16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fm(s: &mut Sound, reg: u8, value: u8) {
        s.write_fm(0, reg);
        s.write_fm(1, value);
    }
    #[test]
    fn silence_and_frame_sample_count() {
        let mut s = Sound::new();
        s.advance(NTSC_MASTER, false);
        assert_eq!(s.audio().len(), SAMPLE_HZ as usize * 2);
        assert!(s.audio().iter().all(|&x| x == 0));
        s.begin_frame();
        assert!(s.audio().is_empty());
    }
    #[test]
    fn psg_latch_period_and_attenuation() {
        let mut s = Sound::new();
        s.write_psg(0x85);
        s.write_psg(0x12);
        s.write_psg(0x90);
        assert_eq!(s.psg.period[0], 0x125);
        s.advance(NTSC_MASTER / 100, false);
        assert!(s.audio().iter().any(|&x| x > 0));
        assert!(s.audio().iter().any(|&x| x < 0));
        s.write_psg(0x9f);
        s.begin_frame();
        s.advance(10000, false);
        assert!(s.audio().iter().all(|&x| x == 0));
    }
    #[test]
    fn timer_flags_and_reset() {
        let mut s = Sound::new();
        fm(&mut s, 0x24, 255);
        fm(&mut s, 0x25, 3);
        fm(&mut s, 0x27, 5);
        s.advance(503, false);
        assert_eq!(s.read_fm() & 1, 0);
        s.advance(1, false);
        assert_eq!(s.read_fm() & 1, 1);
        fm(&mut s, 0x27, 0x10);
        assert_eq!(s.read_fm() & 1, 0);
    }

    #[test]
    fn fm_reset_preserves_pcm_timeline_and_psg() {
        let mut s = Sound::new();
        s.write_psg(0x85);
        s.write_psg(0x12);
        s.write_psg(0x90);
        s.advance(NTSC_MASTER / 2, false);
        let count = s.audio().len();
        let phase = s.sample_phase;
        s.reset_fm();
        assert_eq!(s.audio().len(), count);
        assert_eq!(s.sample_phase, phase);
        s.advance(NTSC_MASTER - NTSC_MASTER / 2, false);
        assert_eq!(s.audio().len(), SAMPLE_HZ as usize * 2);
        assert!(s.audio()[count..].iter().any(|&n| n != 0));
    }

    #[test]
    fn fm_patch_produces_pitched_stereo_and_releases_on_key_off() {
        let mut s = Sound::new();
        for slot in [0, 4, 8, 12] {
            fm(&mut s, 0x30 + slot, 1); // Frequency multiplier one.
            fm(&mut s, 0x40 + slot, if slot == 0 { 0 } else { 127 });
            fm(&mut s, 0x50 + slot, 31); // Fast attack.
            fm(&mut s, 0x80 + slot, 15); // Fast release.
        }
        fm(&mut s, 0xa4, 0x22); // Block four, F-number $269: about 251 Hz.
        fm(&mut s, 0xa0, 0x69);
        fm(&mut s, 0xb0, 7); // Four independent carriers.
        fm(&mut s, 0xb4, 0xc0);
        fm(&mut s, 0x28, 0x10); // Key on only operator one.
        s.advance(NTSC_MASTER / 10, false);
        assert!(s.audio().chunks_exact(2).all(|n| n[0] == n[1]));
        let left: Vec<_> = s.audio().chunks_exact(2).map(|n| n[0]).collect();
        let crossings = left.windows(2).filter(|n| n[0] < 0 && n[1] >= 0).count();
        assert!((24..=26).contains(&crossings), "{crossings} crossings");
        fm(&mut s, 0x28, 0);
        s.begin_frame();
        s.advance(NTSC_MASTER / 2, false);
        assert!(s.audio()[s.audio().len() - 100..].iter().all(|&n| n == 0));
        assert!(s.validate(false));
    }
    #[test]
    fn dac_panning_and_bank_latches() {
        let mut s = Sound::new();
        s.write_fm(2, 0xb6);
        s.write_fm(3, 0x80);
        fm(&mut s, 0x2a, 255);
        fm(&mut s, 0x2b, 128);
        s.advance(2000, false);
        assert!(s.audio().chunks_exact(2).all(|n| n[0] > 0 && n[1] == 0));
    }
    #[test]
    fn state_restores_noise_and_fm_phase() {
        use savestate::State;
        let mut s = Sound::new();
        s.write_psg(0xe4);
        s.write_psg(0xf0);
        s.advance(100000, false);
        let mut data = Vec::new();
        s.save(&mut data);
        s.begin_frame();
        s.advance(200000, false);
        let expected = s.audio().to_vec();
        s.load(&mut data.as_slice()).unwrap();
        s.begin_frame();
        s.advance(200000, false);
        assert_eq!(s.audio(), expected);
    }

    fn driver_machine(code: &[u8]) -> crate::Md {
        let mut rom = vec![0; 0x20000];
        rom[..4].copy_from_slice(&0x00ffff00u32.to_be_bytes());
        rom[4..8].copy_from_slice(&0x200u32.to_be_bytes());
        rom[0x100..0x104].copy_from_slice(b"SEGA");
        rom[0x1f0] = b'U';
        rom[0x200..0x202].copy_from_slice(&[0x60, 0xfe]); // 68000 BRA.S $
        rom[0x10000] = 0x5a;
        let mut md = crate::Md::new(&rom).unwrap();
        md.hw.zram[..3].copy_from_slice(&[0xc3, 0x00, 0x01]); // JP $0100
        md.hw.zram[0x100..0x100 + code.len()].copy_from_slice(code);
        md.hw.z_reset = false;
        md.hw.z_reset_pending = false;
        md
    }

    #[test]
    fn z80_driver_reaches_sound_ports_and_resumes_identical_pcm() {
        let mut program = Vec::new();
        let mut write = |address: u16, n: u8| {
            program.extend_from_slice(&[0x3e, n, 0x32, address as u8, (address >> 8) as u8]);
        };
        write(0x4002, 0xb6); // Channel six panning, FM bank one.
        write(0x4003, 0x80); // Left only.
        write(0x4000, 0x2b);
        write(0x4001, 0x80); // DAC enable.
        write(0x4000, 0x2a);
        write(0x4001, 0xc0); // Initial DAC sample.
        write(0x7f11, 0x85); // PSG tone 0 period and unmute.
        write(0x7f11, 0x12);
        write(0x7f11, 0x90);
        program.extend_from_slice(&[0xfb, 0x76, 0x18, 0xfd]); // EI / HALT / JR HALT.
        let mut md = driver_machine(&program);
        // A real VBlank IRQ wakes HALT; the driver changes its DAC sample.
        md.hw.zram[0x38..0x40].copy_from_slice(&[0x3c, 0x32, 0x01, 0x40, 0xfb, 0xed, 0x4d, 0]);
        md.run_frame([0; 2]);
        assert!(md.audio().chunks_exact(2).any(|n| n[0] != n[1]));
        assert!(md.audio().iter().any(|&n| n < 0));
        assert!(md.hw.sound.validate(md.region().pal()));
        assert!(md.z80.validate());
        let state = md.save_state();
        md.run_frame([0; 2]);
        let next = md.audio().to_vec();
        md.run_frame([0; 2]);
        let after = md.audio().to_vec();
        md.load_state(&state).unwrap();
        md.run_frame([0; 2]);
        assert_eq!(md.audio(), next);
        md.run_frame([0; 2]);
        assert_eq!(md.audio(), after);
    }

    #[test]
    fn z80_serial_bank_register_reads_main_rom() {
        let mut program = Vec::new();
        // ROM address $010000 is bank number 2. Bits enter least significant first.
        for n in 0..9 {
            program.extend_from_slice(&[0x3e, ((2u16 >> n) & 1) as u8, 0x32, 0x00, 0x60]);
        }
        program.extend_from_slice(&[0x3a, 0x00, 0x80, 0x32, 0x00, 0x02, 0x76]);
        let mut md = driver_machine(&program);
        md.run_frame([0; 2]);
        assert_eq!(md.hw.zbank, 0x10000);
        assert_eq!(md.hw.zram[0x200], 0x5a);
    }

    #[test]
    fn restored_audio_phase_must_fit_the_cartridges_clock() {
        let mut rom = vec![0; 0x400];
        rom[4..8].copy_from_slice(&0x200u32.to_be_bytes());
        rom[0x100..0x104].copy_from_slice(b"SEGA");
        rom[0x1f0] = b'E';
        rom[0x200..0x202].copy_from_slice(&[0x60, 0xfe]);
        let mut md = crate::Md::new(&rom).unwrap();
        let intact = md.save_state();
        // A fresh PAL machine still has the sound chip's default NTSC
        // flag until its first advance; this valid state must load.
        md.load_state(&intact).unwrap();
        let mut bad = md.clone();
        bad.hw.sound.sample_phase = u64::from(NTSC_MASTER - 1);
        assert!(bad.hw.sound.validate(false));
        // Trusting that flag previously let a PAL restore underflow its
        // remaining clocks on the next advance.
        assert_eq!(
            md.load_state(&bad.save_state()),
            Err(crate::Error::BadState)
        );
        assert_eq!(md.save_state(), intact);
        md.run_frame([0; 2]);
        assert!(!md.audio().is_empty());
    }
}
