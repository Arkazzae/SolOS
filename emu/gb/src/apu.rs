//! The APU: two pulse channels (the first with a frequency sweep), a wave
//! channel playing 32 samples from its RAM, and noise, stepped by machine
//! cycle and clocked by a frame sequencer that the timer's divider drives.
//! They are mixed to stereo by the panning and master volume, put through
//! the console's high-pass filter and resampled to 48 kHz for the host.

use savestate::state;

/// The rate of the sound given to the host.
pub const SAMPLE_HZ: u32 = 48_000;
/// Machine cycles a second.
const CYCLE_HZ: u32 = 1 << 20;

/// The DMG's output capacitor, as a high-pass filter at the sample rate:
/// 0.999958 a clock, to the power of the clocks in a sample.
const CHARGE: f32 = 0.996_337;

/// Bits of each register FF10-FF2F that always read as 1.
const READ_MASKS: [u8; 0x20] = [
    0x80, 0x3f, 0x00, 0xff, 0xbf, 0xff, 0x3f, 0x00, 0xff, 0xbf, 0x7f, 0xff, 0x9f, 0xff, 0xbf, 0xff, 0xff, 0x00, 0x00,
    0xbf, 0x00, 0x00, 0x70, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];

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
state!(Wave { on, dac, shift, freq, timer, position, sample, read_now, length });
state!(Noise { on, register, timer, lfsr, length, envelope });
// The samples of the frame being made are not part of the state.
state!(Apu { registers, wave_ram, power, pulse, wave, noise, frame_step, frame_bit, odd, outputs, level, dirty, sum, count,
    counted, phase, capacitor });

impl Length {
    /// Clocked by the frame sequencer; false when the channel stops.
    fn clock(&mut self) -> bool {
        if self.enabled && self.count > 0 {
            self.count -= 1;
            return self.count > 0;
        }
        true
    }

    /// A write to NRx4. Enabling the counter in the half of the frame
    /// sequencer's period that does not clock it next clocks it once more,
    /// and a trigger fills it when empty (one less in that half). False
    /// when the channel stops.
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

/// A volume envelope (NRx2): the volume steps up or down every period
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
        // A timer at 0 (never started) runs out at once.
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

#[derive(Clone, Copy, Default)]
struct Pulse {
    on: bool,
    duty: u8,
    step: u8,
    freq: u16,
    timer: u16,
    length: Length,
    envelope: Envelope,
    /// The sweep (the first channel's NR10), its timer, whether it runs,
    /// the frequency it works from, and whether it has subtracted since the
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

    /// Steps the timer; true when the duty step moved.
    fn clock_timer(&mut self) -> bool {
        if self.timer > 1 {
            self.timer -= 1;
            return false;
        }
        self.timer = 2048 - self.freq;
        self.step = (self.step + 1) & 7;
        true
    }

    fn trigger(&mut self) {
        self.on = self.envelope.dac();
        self.timer = 2048 - self.freq;
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

#[derive(Clone, Copy, Default)]
struct Wave {
    on: bool,
    dac: bool,
    /// The volume shift: 4 for silence, else 0, 1 or 2.
    shift: u8,
    freq: u16,
    /// In units of two clocks, half a machine cycle.
    timer: i32,
    position: u8,
    sample: u8,
    /// The wave RAM was read this machine cycle; only then can the CPU
    /// reach it while the channel plays.
    read_now: bool,
    length: Length,
}

impl Wave {
    fn output(&self) -> u8 {
        if !self.on {
            return 0;
        }
        let nibble = if self.position & 1 == 0 { self.sample >> 4 } else { self.sample & 0x0f };
        nibble >> self.shift
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

    fn clock_timer(&mut self) -> bool {
        if self.timer > 1 {
            self.timer -= 1;
            return false;
        }
        self.timer = self.period();
        // Shifts 14 and 15 stop the generator.
        if self.register >> 4 >= 14 {
            return false;
        }
        let bit = (self.lfsr ^ self.lfsr >> 1) & 1;
        self.lfsr = self.lfsr >> 1 | bit << 14;
        if self.register & 8 != 0 {
            self.lfsr = self.lfsr & !0x40 | bit << 6;
        }
        true
    }
}

#[derive(Clone)]
pub struct Apu {
    /// On the Color, the CPU reaches the wave RAM while the channel plays,
    /// and the length counters go off with the power.
    cgb: bool,
    registers: [u8; 0x20],
    wave_ram: [u8; 16],
    power: bool,
    pulse: [Pulse; 2],
    wave: Wave,
    noise: Noise,
    /// The frame sequencer's next step (of 8, at 512 Hz), clocked as bit
    /// 12 of the timer's counter falls (13 in double speed).
    frame_step: u8,
    frame_bit: bool,
    /// In double speed, the channels step every other machine cycle.
    odd: bool,

    /// Each channel's output (0-15), the stereo mix of them, and whether
    /// one changed since.
    outputs: [u8; 4],
    level: [f32; 2],
    dirty: bool,

    /// Resampling: the mix summed over the cycles of the sample being made
    /// (each level times the cycles it lasted, added when it changes), the
    /// cycles of the sample so far and of them those added, and the
    /// high-pass filter's capacitors.
    sum: [f32; 2],
    count: u32,
    counted: u32,
    phase: u32,
    capacitor: [f32; 2],
    pub samples: Vec<i16>,
}

impl Apu {
    pub fn new(cgb: bool) -> Apu {
        Apu {
            cgb,
            registers: [0; 0x20],
            wave_ram: [0; 16],
            power: false,
            pulse: [Pulse::default(); 2],
            wave: Wave::default(),
            noise: Noise { lfsr: 0x7fff, timer: 2, ..Noise::default() },
            frame_step: 0,
            frame_bit: false,
            odd: false,
            outputs: [0; 4],
            level: [0.0; 2],
            dirty: true,
            sum: [0.0; 2],
            count: 0,
            counted: 0,
            phase: 0,
            capacitor: [0.0; 2],
            samples: Vec::with_capacity(2048),
        }
    }

    /// As the boot ROM leaves it: on, after its chime on the first channel,
    /// which is still on at volume 0.
    pub fn after_boot(&mut self) {
        self.write(0xff26, 0x80);
        self.write(0xff11, 0x80);
        self.write(0xff12, 0xf3);
        self.write(0xff25, 0xf3);
        self.write(0xff24, 0x77);
        let pulse = &mut self.pulse[0];
        pulse.freq = 0x7c1;
        pulse.timer = 2048 - pulse.freq;
        pulse.on = true;
        pulse.envelope.volume = 0;
        pulse.envelope.timer = 3;
    }

    /// One machine cycle, given the timer's counter after it; in double
    /// speed, half as long.
    pub fn tick(&mut self, counter: u16, double_speed: bool) {
        let bit = counter & if double_speed { 0x2000 } else { 0x1000 } != 0;
        if self.frame_bit && !bit && self.power {
            self.step_frame();
        }
        self.frame_bit = bit;
        if double_speed {
            self.odd = !self.odd;
            if self.odd {
                return;
            }
        }
        if self.power {
            self.clock_channels();
        }
        self.mix();
    }

    /// The Color's PCM12 and PCM34: two channels' outputs as nibbles.
    pub fn pcm(&self, pair: usize) -> u8 {
        self.outputs[pair * 2] | self.outputs[pair * 2 + 1] << 4
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
        self.refresh();
    }

    /// Steps the timers of the channels that play; the mix is worked out
    /// again only when an output changes.
    fn clock_channels(&mut self) {
        for i in 0..2 {
            if self.pulse[i].on && self.pulse[i].clock_timer() {
                self.set_output(i, self.pulse[i].output());
            }
        }
        let wave = &mut self.wave;
        wave.read_now = false;
        if wave.on {
            // Two steps of two clocks; the CPU's access falls on the second.
            for half in 0..2 {
                wave.timer -= 1;
                if wave.timer == 0 {
                    wave.timer = i32::from(2048 - wave.freq);
                    wave.position = (wave.position + 1) & 31;
                    wave.sample = self.wave_ram[usize::from(wave.position >> 1)];
                    wave.read_now = half == 1;
                    let output = wave.output();
                    if self.outputs[2] != output {
                        self.outputs[2] = output;
                        self.dirty = true;
                    }
                }
            }
        }
        if self.noise.on && self.noise.clock_timer() {
            self.set_output(3, self.noise.output());
        }
    }

    fn set_output(&mut self, channel: usize, output: u8) {
        if self.outputs[channel] != output {
            self.outputs[channel] = output;
            self.dirty = true;
        }
    }

    /// Every channel's output again, after something other than its timer
    /// changed it.
    fn refresh(&mut self) {
        self.outputs = [self.pulse[0].output(), self.pulse[1].output(), self.wave.output(), self.noise.output()];
        self.dirty = true;
    }

    /// The mix, summed into the 48 kHz sample being made; each finished
    /// sample goes through the high-pass filter.
    fn mix(&mut self) {
        self.count += 1;
        if self.dirty {
            // The cycles before this one had the last level.
            self.add_level(self.count - 1);
            self.remix();
        }
        self.phase += SAMPLE_HZ;
        if self.phase >= CYCLE_HZ {
            self.sample();
        }
    }

    /// Adds the level to the sum for the cycles since it was last added, up
    /// to the given one.
    fn add_level(&mut self, upto: u32) {
        let cycles = (upto - self.counted) as f32;
        self.sum[0] += self.level[0] * cycles;
        self.sum[1] += self.level[1] * cycles;
        self.counted = upto;
    }

    /// The stereo mix of the channels' outputs, through their DACs, the
    /// panning and the master volume.
    #[cold]
    fn remix(&mut self) {
        self.dirty = false;
        let dacs =
            [self.pulse[0].envelope.dac(), self.pulse[1].envelope.dac(), self.wave.dac, self.noise.envelope.dac()];
        let (panning, volume) = (self.registers[0x15], self.registers[0x14]);
        let mut level = [0i32; 2];
        for i in 0..4 {
            if !dacs[i] {
                continue;
            }
            // The DAC turns 0-15 into +1 to -1.
            let analog = 15 - 2 * i32::from(self.outputs[i]);
            if panning >> (i + 4) & 1 != 0 {
                level[0] += analog;
            }
            if panning >> i & 1 != 0 {
                level[1] += analog;
            }
        }
        self.level =
            [(level[0] * (i32::from(volume >> 4 & 7) + 1)) as f32, (level[1] * (i32::from(volume & 7) + 1)) as f32];
    }

    /// A finished 48 kHz sample, through the high-pass filter.
    #[inline(never)]
    fn sample(&mut self) {
        self.phase -= CYCLE_HZ;
        self.add_level(self.count);
        for side in 0..2 {
            let x = self.sum[side] / self.count as f32;
            let y = x - self.capacitor[side];
            self.capacitor[side] = x - y * CHARGE;
            self.sum[side] = 0.0;
            // All four channels at full volume swing 480 each way.
            self.samples.push((y * 64.0).clamp(-32768.0, 32767.0) as i16);
        }
        self.count = 0;
        self.counted = 0;
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xff30..=0xff3f => {
                // While the wave channel plays, the CPU reaches the byte it
                // is reading, and on the DMG only as it reads it.
                if !self.wave.on {
                    self.wave_ram[usize::from(addr - 0xff30)]
                } else if self.wave.read_now || self.cgb {
                    self.wave_ram[usize::from(self.wave.position >> 1)]
                } else {
                    0xff
                }
            }
            0xff26 => {
                let on = [self.pulse[0].on, self.pulse[1].on, self.wave.on, self.noise.on];
                let status = on.iter().enumerate().fold(0, |s, (i, &on)| s | u8::from(on) << i);
                0x70 | u8::from(self.power) << 7 | status
            }
            _ => {
                let i = usize::from(addr - 0xff10);
                self.registers[i] | READ_MASKS[i]
            }
        }
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        if let 0xff30..=0xff3f = addr {
            if !self.wave.on {
                self.wave_ram[usize::from(addr - 0xff30)] = value;
            } else if self.wave.read_now || self.cgb {
                self.wave_ram[usize::from(self.wave.position >> 1)] = value;
            }
            return;
        }
        if addr == 0xff26 {
            self.set_power(value & 0x80 != 0);
            return;
        }
        if !self.power {
            // Off, only the DMG's length counters can be written.
            if self.cgb {
                return;
            }
            match addr {
                0xff11 | 0xff16 => self.pulse[usize::from(addr == 0xff16)].length.count = 64 - u16::from(value & 0x3f),
                0xff1b => self.wave.length.count = 256 - u16::from(value),
                0xff20 => self.noise.length.count = 64 - u16::from(value & 0x3f),
                _ => {}
            }
            return;
        }
        self.registers[usize::from(addr - 0xff10)] = value;
        // The frame sequencer's next step does not clock lengths.
        let first_half = self.frame_step & 1 == 1;
        match addr {
            0xff10 => {
                let pulse = &mut self.pulse[0];
                // Leaving subtraction after the sweep has subtracted stops the
                // channel.
                if pulse.negated && pulse.sweep & 8 != 0 && value & 8 == 0 {
                    pulse.on = false;
                }
                pulse.sweep = value;
            }
            0xff11 | 0xff16 => {
                let pulse = &mut self.pulse[usize::from(addr == 0xff16)];
                pulse.duty = value >> 6;
                pulse.length.count = 64 - u16::from(value & 0x3f);
            }
            0xff12 | 0xff17 => {
                let pulse = &mut self.pulse[usize::from(addr == 0xff17)];
                pulse.envelope.register = value;
                pulse.on &= pulse.envelope.dac();
            }
            0xff13 | 0xff18 => {
                let pulse = &mut self.pulse[usize::from(addr == 0xff18)];
                pulse.freq = pulse.freq & 0x700 | u16::from(value);
            }
            0xff14 | 0xff19 => {
                let pulse = &mut self.pulse[usize::from(addr == 0xff19)];
                pulse.freq = pulse.freq & 0xff | u16::from(value & 7) << 8;
                pulse.on &= pulse.length.write(value, 64, first_half);
                if value & 0x80 != 0 {
                    pulse.trigger();
                }
            }
            0xff1a => {
                self.wave.dac = value & 0x80 != 0;
                self.wave.on &= self.wave.dac;
            }
            0xff1b => self.wave.length.count = 256 - u16::from(value),
            0xff1c => self.wave.shift = [4, 0, 1, 2][usize::from(value >> 5 & 3)],
            0xff1d => self.wave.freq = self.wave.freq & 0x700 | u16::from(value),
            0xff1e => {
                let wave = &mut self.wave;
                wave.freq = wave.freq & 0xff | u16::from(value & 7) << 8;
                wave.on &= wave.length.write(value, 256, first_half);
                if value & 0x80 != 0 {
                    self.trigger_wave();
                }
            }
            0xff20 => self.noise.length.count = 64 - u16::from(value & 0x3f),
            0xff21 => {
                self.noise.envelope.register = value;
                self.noise.on &= self.noise.envelope.dac();
            }
            0xff22 => self.noise.register = value,
            0xff23 => {
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
        self.refresh();
    }

    /// Triggering the wave channel restarts it from the first sample, after
    /// a delay. On the DMG, triggering it as it reads its RAM garbles the
    /// RAM's first bytes with the ones being read.
    fn trigger_wave(&mut self) {
        if !self.cgb && self.wave.on && self.wave.timer == 1 {
            let next = usize::from((self.wave.position + 1) & 31) >> 1;
            if next < 4 {
                self.wave_ram[0] = self.wave_ram[next];
            } else {
                let block = next & !3;
                self.wave_ram.copy_within(block..block + 4, 0);
            }
        }
        let wave = &mut self.wave;
        wave.on = wave.dac;
        wave.position = 0;
        wave.timer = i32::from(2048 - wave.freq) + 3;
    }

    /// Off, the APU clears its registers (but not the wave RAM, nor on the
    /// DMG the length counters); on, the frame sequencer and duty steps
    /// start over.
    fn set_power(&mut self, on: bool) {
        if on == self.power {
            return;
        }
        self.power = on;
        if !on {
            self.registers = [0; 0x20];
            let lengths = [self.pulse[0].length.count, self.pulse[1].length.count, self.wave.length.count,
                self.noise.length.count];
            self.pulse = [Pulse::default(); 2];
            self.wave = Wave::default();
            self.noise = Noise { lfsr: 0x7fff, timer: 2, ..Noise::default() };
            if self.cgb {
                return self.refresh();
            }
            self.pulse[0].length.count = lengths[0];
            self.pulse[1].length.count = lengths[1];
            self.wave.length.count = lengths[2];
            self.noise.length.count = lengths[3];
        } else {
            self.frame_step = 0;
            self.pulse[0].timer = 2048;
            self.pulse[1].timer = 2048;
            self.wave.timer = 2048;
            self.noise.timer = self.noise.period();
        }
        self.refresh();
    }
}
