//! The SN76489 inside the VDP: three square waves and a noise channel, with the Game Gear's per-channel stereo
//! switches (port $06). It runs on the Z80's clock (one step every 16 cycles) and each output sample is the
//! average of the steps that fall into it, so a fast square wave does not alias into the host's band.

use savestate::state;

pub const SAMPLE_HZ: u32 = 48_000;
pub(crate) const NTSC_CLOCK: u32 = 3_579_545;
pub(crate) const PAL_CLOCK: u32 = 3_546_895;
/// Peak amplitude of one channel at full volume, then 2 dB per step; the last step is silence.
const VOLUME: [i32; 16] = [
    2400, 1906, 1514, 1203, 955, 759, 603, 479, 380, 302, 240, 191, 151, 120, 96, 0,
];

#[derive(Clone)]
pub(crate) struct Sound {
    period: [u16; 3],
    volume: [u8; 4],
    latch: u8,
    counters: [u16; 4],
    outputs: [bool; 4],
    noise: u8,
    lfsr: u16,
    divider: u32,
    /// Game Gear port $06: bits 7-4 send channels 4-1 to the left, bits 3-0 to the right.
    stereo: u8,
    /// How far the 48 kHz clock is into the next sample, in units of 1/`SAMPLE_HZ` of a Z80 cycle (a sample is
    /// due when it reaches the Z80's clock rate).
    sample_phase: u64,
    sum: [i32; 2],
    steps: i32,
    pal: bool,
    // Output belongs to the current frame, and is regenerated after resume.
    samples: Vec<i16>,
}

state!(Sound {
    period,
    volume,
    latch,
    counters,
    outputs,
    noise,
    lfsr,
    divider,
    stereo,
    sample_phase,
    sum,
    steps,
    pal
});

impl Sound {
    pub fn new(pal: bool) -> Self {
        Self {
            period: [0; 3],
            volume: [15; 4],
            latch: 0,
            counters: [1; 4],
            outputs: [false; 4],
            noise: 0,
            lfsr: 0x8000,
            divider: 0,
            stereo: 0xff,
            sample_phase: 0,
            sum: [0; 2],
            steps: 0,
            pal,
            samples: Vec::with_capacity(2000),
        }
    }

    fn clock(&self) -> u64 {
        u64::from(if self.pal { PAL_CLOCK } else { NTSC_CLOCK })
    }

    pub fn begin_frame(&mut self) {
        self.samples.clear();
    }
    pub fn audio(&self) -> &[i16] {
        &self.samples
    }
    pub fn set_stereo(&mut self, value: u8) {
        self.stereo = value;
    }

    pub fn validate(&self) -> bool {
        self.latch < 8
            && self.noise < 8
            && self.volume.iter().all(|&n| n < 16)
            && self.period.iter().all(|&n| n < 1024)
            && self.divider < 16
            && self.sample_phase < self.clock()
            && (0..=16 * 8).contains(&self.steps)
            && self.sum.iter().all(|n| n.abs() <= 16 * 8 * 4 * 2400)
    }

    pub fn write(&mut self, n: u8) {
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

    fn shift_noise(&mut self) {
        let feedback = if self.noise & 4 != 0 {
            (self.lfsr ^ (self.lfsr >> 3)) & 1
        } else {
            self.lfsr & 1
        };
        self.lfsr = (self.lfsr >> 1) | (feedback << 15);
        self.outputs[3] = self.lfsr & 1 != 0;
    }

    /// One step of the chip, every 16 Z80 cycles.
    fn step(&mut self) {
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
        for ch in 0..4 {
            // Ultrasonic tone periods are how games play samples through the volume, and would only alias.
            if ch < 3 && self.period[ch] < 2 {
                continue;
            }
            let level =
                VOLUME[usize::from(self.volume[ch])] * if self.outputs[ch] { 1 } else { -1 };
            if self.stereo & (0x10 << ch) != 0 {
                self.sum[0] += level;
            }
            if self.stereo & (1 << ch) != 0 {
                self.sum[1] += level;
            }
        }
        self.steps += 1;
    }

    /// Runs the chip through `cycles` Z80 cycles.
    pub fn advance(&mut self, cycles: u32) {
        self.divider += cycles;
        let clock = self.clock();
        while self.divider >= 16 {
            self.divider -= 16;
            self.step();
            self.sample_phase += u64::from(SAMPLE_HZ) * 16;
            if self.sample_phase >= clock {
                self.sample_phase -= clock;
                let n = self.steps.max(1);
                for side in 0..2 {
                    self.samples
                        .push((self.sum[side] / n).clamp(-32768, 32767) as i16);
                    self.sum[side] = 0;
                }
                self.steps = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_and_the_sample_count_of_a_second() {
        let mut s = Sound::new(false);
        s.advance(NTSC_CLOCK);
        assert!(
            (47_999..=48_001).contains(&(s.audio().len() / 2)),
            "{}",
            s.audio().len()
        );
        assert!(s.audio().iter().all(|&x| x == 0));
        let mut p = Sound::new(true);
        p.advance(PAL_CLOCK);
        assert!((47_999..=48_001).contains(&(p.audio().len() / 2)));
        s.begin_frame();
        assert!(s.audio().is_empty());
    }

    #[test]
    fn tone_period_latch_and_volume() {
        let mut s = Sound::new(false);
        // Channel 0: period $125, volume 0 (loudest).
        s.write(0x85);
        s.write(0x12);
        s.write(0x90);
        assert_eq!(s.period[0], 0x125);
        s.advance(NTSC_CLOCK / 100);
        assert!(s.audio().iter().any(|&x| x > 1000));
        assert!(s.audio().iter().any(|&x| x < -1000));
        // Both ears carry it on the Master System.
        assert!(s.audio().chunks(2).all(|f| f[0] == f[1]));
        s.write(0x9f);
        // The sample being averaged still holds the loud steps.
        s.advance(1_000);
        s.begin_frame();
        s.advance(10_000);
        assert!(s.audio().iter().all(|&x| x == 0));
    }

    #[test]
    fn game_gear_stereo_register_routes_channels() {
        let mut s = Sound::new(false);
        s.write(0x85);
        s.write(0x12);
        s.write(0x90);
        // Channel 1 on the left only (bit 4 set, bit 0 clear).
        s.set_stereo(0x10);
        s.advance(NTSC_CLOCK / 100);
        assert!(s.audio().chunks(2).any(|f| f[0] != 0));
        assert!(s.audio().chunks(2).all(|f| f[1] == 0));
        s.set_stereo(0x01);
        s.advance(1_000);
        s.begin_frame();
        s.advance(NTSC_CLOCK / 100);
        assert!(s.audio().chunks(2).all(|f| f[0] == 0));
        assert!(s.audio().chunks(2).any(|f| f[1] != 0));
        s.set_stereo(0);
        s.advance(1_000);
        s.begin_frame();
        s.advance(NTSC_CLOCK / 100);
        assert!(s.audio().iter().all(|&x| x == 0));
    }

    #[test]
    fn noise_channel_runs_and_a_noise_write_resets_the_shift_register() {
        let mut s = Sound::new(false);
        s.write(0xe4);
        s.write(0xf0);
        assert_eq!(s.noise, 4);
        s.advance(NTSC_CLOCK / 50);
        assert!(s.audio().iter().any(|&x| x > 0) && s.audio().iter().any(|&x| x < 0));
        s.write(0xe4);
        assert_eq!(s.lfsr, 0x8000);
    }
}
