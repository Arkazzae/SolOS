//! The sound: the Game Boy's four channels (two pulse, the first with a
//! sweep, a wave channel and noise) and two channels of 8-bit samples that
//! DMA feeds through 32-byte FIFOs, a sample each time a timer overflows.
//! The console's mixer adds them up 32768 times a second (its default
//! rate), with a bias, into 10 bits; that is resampled to 48 kHz for the
//! host.
//!
//! The Game Boy channels count in its machine cycles (16 of the GBA's),
//! and are run in bulk up to the moment something reads or changes them
//! or the mixer takes a sample, rather than cycle by cycle.

use crate::Gba;
use crate::bus::set_byte;
use savestate::state;

/// The rate of the sound given to the host.
pub const SAMPLE_HZ: u32 = 48_000;
/// The mixer's rate, in samples a second, and in cycles a sample.
const MIX_HZ: u32 = 32_768;
pub const MIX_CYCLES: u64 = 512;
/// The Game Boy channels' machine cycle, in the GBA's cycles.
const MACHINE: u64 = 16;
/// The frame sequencer steps at 512 Hz.
const FRAME_STEP: u32 = 2048;

/// A high-pass filter at 48 kHz that takes away what is left of the bias
/// and the Game Boy channels' unsigned output (about 5 Hz).
const CHARGE: f32 = 0.9993;

/// The pulse duty cycles' eight steps, the first step the lowest bit.
const DUTIES: [u8; 4] = [0b1000_0000, 0b1000_0001, 0b1110_0001, 0b0111_1110];

/// The noise channel's divisors, in machine cycles.
const NOISE_DIVISORS: [u32; 8] = [2, 4, 8, 12, 16, 20, 24, 28];

/// A length counter: the channel stops when it runs out, if enabled.
#[derive(Clone, Copy, Default)]
struct Length {
    count: u16,
    enabled: bool,
}

state!(Length { count, enabled });
state!(Envelope { register, volume, timer });
state!(Pulse { on, duty, step, freq, timer, length, envelope, sweep, sweep_timer, sweep_on, shadow, negated });
state!(Wave { on, dac, control, volume, freq, timer, position, length });
state!(Noise { on, register, timer, lfsr, length, envelope });
state!(Fifo { data, read, len, sample });
// The samples of the frame being made are not part of the state.
state!(Apu { registers, wave_ram, power, pulse, wave, noise, frame_step, frame_timer, soundcnt_h, bias, fifo, time,
    previous, ahead, capacitor });

impl Length {
    /// Clocked by the frame sequencer; false when the channel stops.
    fn clock(&mut self) -> bool {
        if self.enabled && self.count > 0 {
            self.count -= 1;
            return self.count > 0;
        }
        true
    }

    /// A write to the register with the trigger. Enabling the counter in
    /// the half of the frame sequencer's period that does not clock it next
    /// clocks it once more, and a trigger fills it when empty (one less in
    /// that half). False when the channel stops.
    fn write(&mut self, value: u8, max: u16, first_half: bool) -> bool {
        let was_enabled = self.enabled;
        self.enabled = value & 0x40 != 0;
        let mut running = true;
        if first_half && !was_enabled && self.enabled && self.count > 0 {
            self.count -= 1;
            running = self.count > 0;
        }
        if value & 0x80 != 0 && self.count == 0 {
            self.count = if self.enabled && first_half { max - 1 } else { max };
        }
        running || value & 0x80 != 0
    }
}

/// A volume envelope: the volume steps up or down every period
/// sixty-fourths of a second.
#[derive(Clone, Copy, Default)]
struct Envelope {
    register: u8,
    volume: u8,
    timer: u8,
}

impl Envelope {
    /// The DAC is on while the register's top five bits are not all 0.
    fn dac(&self) -> bool {
        self.register & 0xf8 != 0
    }

    fn trigger(&mut self) {
        self.volume = self.register >> 4;
        self.timer = self.period();
    }

    fn period(&self) -> u8 {
        match self.register & 7 {
            0 => 8,
            p => p,
        }
    }

    fn clock(&mut self) {
        if self.register & 7 == 0 {
            return;
        }
        if self.timer > 1 {
            self.timer -= 1;
        } else {
            self.timer = self.period();
            if self.register & 8 != 0 && self.volume < 15 {
                self.volume += 1;
            } else if self.register & 8 == 0 && self.volume > 0 {
                self.volume -= 1;
            }
        }
    }
}

/// Runs a timer that fires every period cycles for n cycles: it has
/// `timer` left until it next fires. Gives how many times it fired.
fn run_timer(timer: &mut u32, period: u32, n: u32) -> u32 {
    if n < *timer {
        *timer -= n;
        return 0;
    }
    let after = n - *timer;
    *timer = period - after % period;
    1 + after / period
}

#[derive(Clone, Copy, Default)]
struct Pulse {
    on: bool,
    duty: u8,
    step: u8,
    freq: u16,
    timer: u32,
    length: Length,
    envelope: Envelope,
    /// The sweep (the first channel's), its timer, whether it runs, the
    /// frequency it works from, and whether it has subtracted since the
    /// trigger.
    sweep: u8,
    sweep_timer: u8,
    sweep_on: bool,
    shadow: u16,
    negated: bool,
}

impl Pulse {
    fn output(&self) -> u8 {
        if self.on && DUTIES[usize::from(self.duty)] >> self.step & 1 != 0 { self.envelope.volume } else { 0 }
    }

    fn run(&mut self, n: u32) {
        let steps = run_timer(&mut self.timer, 2048 - u32::from(self.freq), n);
        self.step = ((u32::from(self.step) + steps) & 7) as u8;
    }

    fn trigger(&mut self) {
        self.on = self.envelope.dac();
        self.timer = 2048 - u32::from(self.freq);
        self.envelope.trigger();
        let (period, shift) = (self.sweep >> 4 & 7, self.sweep & 7);
        self.shadow = self.freq;
        self.sweep_timer = if period == 0 { 8 } else { period };
        self.sweep_on = period != 0 || shift != 0;
        self.negated = false;
        if shift != 0 {
            self.sweep_target();
        }
    }

    /// The next frequency of the sweep; past 2047 stops the channel.
    fn sweep_target(&mut self) -> u16 {
        let delta = self.shadow >> (self.sweep & 7);
        let target = if self.sweep & 8 != 0 {
            self.negated = true;
            self.shadow - delta
        } else {
            self.shadow + delta
        };
        if target > 2047 {
            self.on = false;
        }
        target
    }

    fn clock_sweep(&mut self) {
        if self.sweep_timer > 1 {
            self.sweep_timer -= 1;
            return;
        }
        let period = self.sweep >> 4 & 7;
        self.sweep_timer = if period == 0 { 8 } else { period };
        if self.sweep_on && period != 0 {
            let target = self.sweep_target();
            if target <= 2047 && self.sweep & 7 != 0 {
                self.shadow = target;
                self.freq = target;
                self.sweep_target();
            }
        }
    }
}

/// The wave channel. The GBA's has two banks of 32 samples: it plays one
/// (or both, one after the other) while the CPU reaches the other.
#[derive(Clone, Copy, Default)]
struct Wave {
    on: bool,
    dac: bool,
    /// SOUND3CNT_L: bit 5 plays both banks, bit 6 picks the bank played.
    control: u8,
    /// SOUND3CNT_H's top byte: the volume, and 75% in bit 7.
    volume: u8,
    freq: u16,
    /// In units of half a machine cycle.
    timer: u32,
    /// The sample playing, 0-63 over both banks.
    position: u8,
    length: Length,
}

impl Wave {
    fn output(&self, ram: &[u8; 32]) -> u8 {
        if !self.on {
            return 0;
        }
        let byte = ram[usize::from(self.position >> 1)];
        let sample = if self.position & 1 == 0 { byte >> 4 } else { byte & 0x0f };
        if self.volume & 0x80 != 0 { sample * 3 / 4 } else { sample >> [4, 0, 1, 2][usize::from(self.volume >> 5 & 3)] }
    }

    /// The bank played first.
    fn bank(&self) -> u8 {
        self.control >> 6 & 1
    }

    fn run(&mut self, n: u32) {
        let steps = run_timer(&mut self.timer, 2048 - u32::from(self.freq), 2 * n);
        let samples = if self.control & 0x20 != 0 { 64 } else { 32 };
        // Positions count from the bank played first.
        let first = u32::from(self.bank()) * 32;
        let at = (u32::from(self.position) + 64 - first) % 64;
        self.position = ((at + steps) % samples + first) as u8 % 64;
    }
}

#[derive(Clone, Copy, Default)]
struct Noise {
    on: bool,
    register: u8,
    timer: u32,
    lfsr: u16,
    length: Length,
    envelope: Envelope,
}

impl Noise {
    fn output(&self) -> u8 {
        if self.on && self.lfsr & 1 == 0 { self.envelope.volume } else { 0 }
    }

    fn period(&self) -> u32 {
        NOISE_DIVISORS[usize::from(self.register & 7)] << (self.register >> 4)
    }

    fn run(&mut self, n: u32) {
        let period = self.period();
        let steps = run_timer(&mut self.timer, period, n);
        // Shifts 14 and 15 stop the generator.
        if self.register >> 4 >= 14 {
            return;
        }
        for _ in 0..steps {
            let bit = (self.lfsr ^ self.lfsr >> 1) & 1;
            self.lfsr = self.lfsr >> 1 | bit << 14;
            if self.register & 8 != 0 {
                self.lfsr = self.lfsr & !0x40 | bit << 6;
            }
        }
    }
}

/// A FIFO of 8-bit samples for one of the DMA channels, and the sample
/// playing.
#[derive(Clone, Copy, Default)]
struct Fifo {
    data: [u8; 32],
    read: u8,
    len: u8,
    sample: u8,
}

impl Fifo {
    fn push(&mut self, value: u8) {
        if self.len < 32 {
            self.data[usize::from((self.read + self.len) & 31)] = value;
            self.len += 1;
        }
    }

    fn pop(&mut self) {
        if self.len > 0 {
            self.sample = self.data[usize::from(self.read)];
            self.read = (self.read + 1) & 31;
            self.len -= 1;
        }
    }
}

#[derive(Clone)]
pub struct Apu {
    /// The Game Boy channels' registers, NR10-NR52, by their Game Boy
    /// number (FF10-FF26).
    registers: [u8; 0x17],
    wave_ram: [u8; 32],
    power: bool,
    pulse: [Pulse; 2],
    wave: Wave,
    noise: Noise,
    /// The frame sequencer's next step (of 8), and the machine cycles
    /// until it.
    frame_step: u8,
    frame_timer: u32,
    /// SOUNDCNT_H (volumes, and where the FIFOs go and which timers feed
    /// them) and SOUNDBIAS.
    pub soundcnt_h: u16,
    bias: u16,
    fifo: [Fifo; 2],
    /// The cycle the Game Boy channels have been run up to.
    time: u64,

    /// Resampling to 48 kHz: the last mixer sample, and how far the next
    /// output sample is past it (in units where a mixer sample is SAMPLE_HZ
    /// long and an output sample MIX_HZ); and the high-pass filter.
    previous: [i32; 2],
    ahead: u32,
    capacitor: [f32; 2],
    pub samples: Vec<i16>,
}

/// The Game Boy register a byte of the GBA's sound registers is, if any.
fn gb_register(reg: u32) -> Option<usize> {
    let nr = match reg {
        0x60 => 0x10,
        0x62 => 0x11,
        0x63 => 0x12,
        0x64 => 0x13,
        0x65 => 0x14,
        0x68 => 0x16,
        0x69 => 0x17,
        0x6c => 0x18,
        0x6d => 0x19,
        0x70 => 0x1a,
        0x72 => 0x1b,
        0x73 => 0x1c,
        0x74 => 0x1d,
        0x75 => 0x1e,
        0x78 => 0x20,
        0x79 => 0x21,
        0x7c => 0x22,
        0x7d => 0x23,
        0x80 => 0x24,
        0x81 => 0x25,
        0x84 => 0x26,
        _ => return None,
    };
    Some(nr - 0x10)
}

/// The bits of each 16-bit sound register (from 0x60) that read back.
const READ_MASKS: [u16; 0x16] = [
    0x007f, 0xffc0, 0x4000, 0, 0xffc0, 0, 0x4000, 0, 0x00e0, 0xe000, 0x4000, 0, 0xff00, 0, 0x40ff, 0, 0xff77, 0x770f,
    0x0080, 0, 0xc3fe, 0,
];

impl Apu {
    pub fn new() -> Apu {
        Apu {
            registers: [0; 0x17],
            wave_ram: [0; 32],
            power: false,
            pulse: [Pulse::default(); 2],
            wave: Wave::default(),
            noise: Noise { lfsr: 0x7fff, timer: 2, ..Noise::default() },
            frame_step: 0,
            frame_timer: FRAME_STEP,
            soundcnt_h: 0,
            bias: 0x200,
            fifo: [Fifo::default(); 2],
            time: 0,
            previous: [0; 2],
            ahead: 0,
            capacitor: [0.0; 2],
            samples: Vec::with_capacity(2048),
        }
    }

    pub fn set_bias_level(&mut self, level: u16) {
        self.bias = self.bias & !0x3fe | level & 0x3fe;
    }

    /// Whether a timer's overflows feed a FIFO.
    pub fn uses_timer(&self, timer: usize) -> bool {
        self.power
            && (usize::from(self.soundcnt_h >> 10 & 1) == timer || usize::from(self.soundcnt_h >> 14 & 1) == timer)
    }

    /// Runs the Game Boy channels up to a cycle.
    fn run(&mut self, now: u64) {
        let cycles = ((now.saturating_sub(self.time)) / MACHINE) as u32;
        self.time += u64::from(cycles) * MACHINE;
        if !self.power {
            return;
        }
        let mut left = cycles;
        while left > 0 {
            let n = left.min(self.frame_timer);
            if self.pulse[0].on {
                self.pulse[0].run(n);
            }
            if self.pulse[1].on {
                self.pulse[1].run(n);
            }
            if self.wave.on {
                self.wave.run(n);
            }
            if self.noise.on {
                self.noise.run(n);
            }
            left -= n;
            self.frame_timer -= n;
            if self.frame_timer == 0 {
                self.frame_timer = FRAME_STEP;
                self.step_frame();
            }
        }
    }

    /// The frame sequencer: lengths on even steps, the sweep on 2 and 6,
    /// envelopes on 7.
    fn step_frame(&mut self) {
        let step = self.frame_step;
        self.frame_step = (step + 1) & 7;
        if step & 1 == 0 {
            self.pulse[0].on &= self.pulse[0].length.clock();
            self.pulse[1].on &= self.pulse[1].length.clock();
            self.wave.on &= self.wave.length.clock();
            self.noise.on &= self.noise.length.clock();
        }
        if step == 2 || step == 6 {
            self.pulse[0].clock_sweep();
        }
        if step == 7 {
            self.pulse[0].envelope.clock();
            self.pulse[1].envelope.clock();
            self.noise.envelope.clock();
        }
    }

    /// A sample of the mixer: the Game Boy channels (each 0-15) panned,
    /// at the master volume and SOUNDCNT_H's share, with the FIFOs' samples,
    /// plus the bias, kept to 10 bits. Then any 48 kHz samples that fall
    /// before it.
    fn mix(&mut self, now: u64) {
        self.run(now);
        let outputs =
            [self.pulse[0].output(), self.pulse[1].output(), self.wave.output(&self.wave_ram), self.noise.output()];
        let (volume, panning) = (self.registers[0x14], self.registers[0x15]);
        let mut level = [0i32; 2];
        if self.power {
            for (i, &output) in outputs.iter().enumerate() {
                if panning >> (i + 4) & 1 != 0 {
                    level[0] += i32::from(output);
                }
                if panning >> i & 1 != 0 {
                    level[1] += i32::from(output);
                }
            }
            let shift = 4 - i32::from(self.soundcnt_h & 3).min(3);
            level[0] = (level[0] * 8 * (i32::from(volume >> 4 & 7) + 1)) >> shift;
            level[1] = (level[1] * 8 * (i32::from(volume & 7) + 1)) >> shift;
            for (i, fifo) in self.fifo.iter().enumerate() {
                let sample = (i32::from(fifo.sample as i8) * 4) >> (1 - (self.soundcnt_h >> (2 + i) & 1));
                let route = self.soundcnt_h >> (8 + 4 * i);
                if route & 2 != 0 {
                    level[0] += sample;
                }
                if route & 1 != 0 {
                    level[1] += sample;
                }
            }
        }
        let bias = i32::from(self.bias & 0x3fe);
        let current = level.map(|l| (l + bias).clamp(0, 0x3ff) - bias);
        while self.ahead <= SAMPLE_HZ {
            for side in 0..2 {
                let (a, b) = (self.previous[side] as f32, current[side] as f32);
                let x = a + (b - a) * (self.ahead as f32 / SAMPLE_HZ as f32);
                let y = x - self.capacitor[side];
                self.capacitor[side] = x - y * CHARGE;
                self.samples.push((y * 64.0).clamp(-32768.0, 32767.0) as i16);
            }
            self.ahead += MIX_HZ;
        }
        self.ahead -= SAMPLE_HZ;
        self.previous = current;
    }

    pub fn read_register(&mut self, reg: u32) -> Option<u16> {
        match reg {
            0x60..=0x8b => {
                let i = (reg as usize - 0x60) / 2;
                let value = match reg {
                    0x82 => self.soundcnt_h,
                    0x84 => {
                        let on = [self.pulse[0].on, self.pulse[1].on, self.wave.on, self.noise.on];
                        on.iter().enumerate().fold(u16::from(self.power) << 7, |s, (i, &on)| s | u16::from(on) << i)
                    }
                    0x88 => self.bias,
                    _ => {
                        let byte = |r: u32| gb_register(r).map_or(0, |nr| u16::from(self.registers[nr]));
                        byte(reg) | byte(reg + 1) << 8
                    }
                };
                Some(value & READ_MASKS[i] | if reg == 0x84 { value & 0x0f } else { 0 })
            }
            // The wave bank the channel is not playing.
            0x90..=0x9f => {
                let i = self.cpu_wave_index(reg);
                Some(u16::from_le_bytes([self.wave_ram[i], self.wave_ram[i + 1]]))
            }
            _ => None,
        }
    }

    fn cpu_wave_index(&self, reg: u32) -> usize {
        usize::from(self.wave.bank() ^ 1) * 16 + (reg as usize & 0xf)
    }

    fn write(&mut self, reg: u32, value: u8) {
        match reg {
            0x82 | 0x83 => {
                set_byte(&mut self.soundcnt_h, reg, value);
                // Bits 11 and 15 empty a FIFO.
                for i in 0..2 {
                    if self.soundcnt_h & 0x800 << (4 * i) != 0 {
                        self.fifo[i] = Fifo::default();
                    }
                }
                self.soundcnt_h &= 0x770f;
                return;
            }
            0x88 | 0x89 => return set_byte(&mut self.bias, reg, value),
            0x90..=0x9f => {
                let i = self.cpu_wave_index(reg);
                self.wave_ram[i] = value;
                return;
            }
            0xa0..=0xa7 => return self.fifo[(reg as usize - 0xa0) / 4].push(value),
            _ => {}
        }
        let Some(nr) = gb_register(reg) else { return };
        if nr == 0x16 {
            return self.set_power(value & 0x80 != 0);
        }
        if !self.power {
            return;
        }
        self.registers[nr] = value;
        // The frame sequencer's next step does not clock lengths.
        let first_half = self.frame_step & 1 == 1;
        match nr + 0x10 {
            0x10 => {
                let pulse = &mut self.pulse[0];
                // Leaving subtraction after the sweep has subtracted stops the
                // channel.
                if pulse.negated && pulse.sweep & 8 != 0 && value & 8 == 0 {
                    pulse.on = false;
                }
                pulse.sweep = value & 0x7f;
            }
            0x11 | 0x16 => {
                let pulse = &mut self.pulse[usize::from(nr == 6)];
                pulse.duty = value >> 6;
                pulse.length.count = 64 - u16::from(value & 0x3f);
            }
            0x12 | 0x17 => {
                let pulse = &mut self.pulse[usize::from(nr == 7)];
                pulse.envelope.register = value;
                pulse.on &= pulse.envelope.dac();
            }
            0x13 | 0x18 => {
                let pulse = &mut self.pulse[usize::from(nr == 8)];
                pulse.freq = pulse.freq & 0x700 | u16::from(value);
            }
            0x14 | 0x19 => {
                let pulse = &mut self.pulse[usize::from(nr == 9)];
                pulse.freq = pulse.freq & 0xff | u16::from(value & 7) << 8;
                pulse.on &= pulse.length.write(value, 64, first_half);
                if value & 0x80 != 0 {
                    pulse.trigger();
                }
            }
            0x1a => {
                self.wave.control = value & 0xe0;
                self.wave.dac = value & 0x80 != 0;
                self.wave.on &= self.wave.dac;
            }
            0x1b => self.wave.length.count = 256 - u16::from(value),
            0x1c => self.wave.volume = value & 0xe0,
            0x1d => self.wave.freq = self.wave.freq & 0x700 | u16::from(value),
            0x1e => {
                let wave = &mut self.wave;
                wave.freq = wave.freq & 0xff | u16::from(value & 7) << 8;
                wave.on &= wave.length.write(value, 256, first_half);
                if value & 0x80 != 0 {
                    wave.on = wave.dac;
                    wave.position = wave.bank() * 32;
                    wave.timer = 2048 - u32::from(wave.freq) + 3;
                }
            }
            0x20 => self.noise.length.count = 64 - u16::from(value & 0x3f),
            0x21 => {
                self.noise.envelope.register = value;
                self.noise.on &= self.noise.envelope.dac();
            }
            0x22 => self.noise.register = value,
            0x23 => {
                let noise = &mut self.noise;
                noise.on &= noise.length.write(value, 64, first_half);
                if value & 0x80 != 0 {
                    noise.on = noise.envelope.dac();
                    noise.envelope.trigger();
                    noise.lfsr = 0x7fff;
                    noise.timer = noise.period();
                }
            }
            _ => {}
        }
    }

    /// Off, the Game Boy channels' registers clear; on, the frame
    /// sequencer and duty steps start over.
    fn set_power(&mut self, on: bool) {
        if on == self.power {
            return;
        }
        self.power = on;
        self.registers = [0; 0x17];
        self.registers[0x16] = u8::from(on) << 7;
        self.pulse = [Pulse::default(); 2];
        self.wave = Wave::default();
        self.noise = Noise { lfsr: 0x7fff, timer: 2, ..Noise::default() };
        if on {
            self.frame_step = 0;
            self.frame_timer = FRAME_STEP;
            self.pulse[0].timer = 2048;
            self.pulse[1].timer = 2048;
            self.wave.timer = 2048;
        }
    }
}

impl Gba {
    /// A mixer sample, every MIX_CYCLES.
    pub(crate) fn audio_event(&mut self, at: u64) {
        self.apu.mix(at);
        self.schedule(crate::AUDIO, at + MIX_CYCLES);
    }

    pub(crate) fn apu_read_register(&mut self, reg: u32) -> Option<u16> {
        self.idle_loop.touched = true;
        self.apu.run(self.cycles);
        self.apu.read_register(reg)
    }

    pub(crate) fn apu_write_register(&mut self, reg: u32, value: u8) {
        self.apu.run(self.cycles);
        let feeds = [self.apu.uses_timer(0), self.apu.uses_timer(1)];
        self.apu.write(reg, value);
        if feeds != [self.apu.uses_timer(0), self.apu.uses_timer(1)] {
            self.reschedule_timer(0);
            self.reschedule_timer(1);
        }
    }

    /// A timer overflowed: the FIFOs it feeds play their next sample, and
    /// ask DMA for more when half empty.
    pub(crate) fn apu_timer_overflow(&mut self, timer: usize) {
        if !self.apu.power {
            return;
        }
        for i in 0..2 {
            if usize::from(self.apu.soundcnt_h >> (10 + 4 * i) & 1) == timer {
                self.apu.fifo[i].pop();
                if self.apu.fifo[i].len <= 16 {
                    self.fifo_dma(i);
                }
            }
        }
    }
}
