//! The SPU: 24 voices playing SPU-ADPCM samples from 512 KB of sound RAM
//! at 44100 Hz, each with its pitch (and pitch modulation by the voice
//! before it), 4-point Gaussian interpolation, an ADSR envelope and a
//! stereo volume that can sweep; a noise generator; CD audio mixed in;
//! reverb over a work area in sound RAM; an interrupt when a voice or a
//! transfer touches a set address. It runs a sample every 768 CPU cycles,
//! brought up to date when the CPU touches it and at the end of a frame.

use crate::Psx;
use savestate::state;

/// CPU cycles a sample.
pub const CYCLES_PER_SAMPLE: u64 = 768;

const RAM: usize = 0x8_0000;

/// The 4-point Gaussian interpolation table (psx-spx).
const GAUSS: [i32; 512] = [
    -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
    0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 3, 3,
    3, 4, 4, 5, 5, 6, 7, 7, 8, 9, 9, 10, 11, 12, 13, 14,
    15, 16, 17, 18, 19, 21, 22, 24, 25, 27, 28, 30, 32, 33, 35, 37,
    39, 41, 44, 46, 48, 51, 53, 56, 58, 61, 64, 67, 70, 73, 77, 80,
    84, 87, 91, 95, 99, 103, 107, 111, 116, 120, 125, 130, 135, 140, 145, 150,
    156, 161, 167, 173, 179, 186, 192, 199, 205, 212, 219, 227, 234, 242, 250, 257,
    266, 274, 283, 291, 300, 309, 319, 328, 338, 348, 358, 369, 379, 390, 401, 412,
    424, 436, 448, 460, 473, 485, 498, 512, 525, 539, 553, 567, 582, 597, 612, 627,
    643, 659, 675, 692, 708, 726, 743, 761, 779, 797, 816, 835, 854, 874, 894, 914,
    935, 956, 977, 999, 1020, 1043, 1066, 1089, 1112, 1136, 1160, 1184, 1209, 1234, 1260, 1286,
    1312, 1339, 1366, 1394, 1422, 1450, 1479, 1508, 1537, 1567, 1597, 1628, 1659, 1691, 1723, 1756,
    1789, 1822, 1856, 1890, 1924, 1959, 1995, 2031, 2067, 2104, 2141, 2179, 2217, 2256, 2295, 2334,
    2374, 2415, 2456, 2497, 2539, 2582, 2624, 2668, 2712, 2756, 2801, 2846, 2892, 2938, 2985, 3032,
    3079, 3128, 3176, 3225, 3275, 3325, 3376, 3427, 3479, 3531, 3584, 3637, 3691, 3745, 3799, 3855,
    3910, 3967, 4023, 4081, 4138, 4197, 4255, 4315, 4374, 4435, 4495, 4557, 4619, 4681, 4744, 4807,
    4871, 4935, 5000, 5065, 5131, 5197, 5264, 5332, 5399, 5468, 5536, 5606, 5676, 5746, 5817, 5888,
    5959, 6032, 6104, 6177, 6251, 6325, 6400, 6475, 6550, 6626, 6702, 6779, 6856, 6934, 7012, 7091,
    7170, 7249, 7329, 7409, 7490, 7571, 7653, 7735, 7817, 7900, 7983, 8066, 8150, 8234, 8319, 8404,
    8489, 8575, 8661, 8748, 8834, 8922, 9009, 9097, 9185, 9273, 9362, 9451, 9541, 9630, 9720, 9811,
    9901, 9992, 10083, 10174, 10266, 10358, 10450, 10542, 10635, 10727, 10820, 10913, 11007, 11100, 11194, 11288,
    11382, 11477, 11571, 11666, 11761, 11856, 11951, 12046, 12141, 12237, 12332, 12428, 12524, 12620, 12716, 12812,
    12908, 13004, 13100, 13196, 13292, 13388, 13484, 13580, 13676, 13772, 13868, 13964, 14060, 14156, 14252, 14348,
    14443, 14539, 14634, 14729, 14824, 14919, 15014, 15108, 15203, 15297, 15391, 15485, 15578, 15671, 15764, 15857,
    15948, 16041, 16133, 16226, 16317, 16409, 16500, 16592, 16682, 16773, 16863, 16953, 17042, 17131, 17220, 17308,
    17396, 17484, 17571, 17658, 17744, 17830, 17916, 18001, 18086, 18170, 18254, 18337, 18420, 18502, 18584, 18665,
    18746, 18826, 18905, 18985, 19063, 19141, 19219, 19295, 19372, 19447, 19522, 19597, 19671, 19744, 19816, 19888,
    19959, 20030, 20100, 20169, 20238, 20306, 20373, 20439, 20505, 20570, 20634, 20698, 20760, 20822, 20884, 20944,
    21004, 21063, 21121, 21178, 21235, 21290, 21345, 21399, 21452, 21505, 21556, 21607, 21657, 21706, 21754, 21801,
    21848, 21893, 21938, 21982, 22025, 22066, 22107, 22148, 22187, 22225, 22262, 22299, 22334, 22369, 22402, 22435,
    22467, 22498, 22527, 22556, 22584, 22611, 22637, 22662, 22686, 22709, 22731, 22752, 22772, 22791, 22809, 22826,
    22842, 22857, 22872, 22885, 22897, 22908, 22918, 22927, 22935, 22942, 22948, 22953, 22957, 22960, 22962, 22963,
];

/// SPU-ADPCM's prediction filters: weights of the last two samples, /64.
const FILTERS: [(i32, i32); 5] = [(0, 0), (60, 0), (115, -52), (98, -55), (122, -60)];

#[derive(Clone, Copy, Default, PartialEq)]
enum Phase {
    #[default]
    Off,
    Attack,
    Decay,
    Sustain,
    Release,
}

impl savestate::State for Phase {
    fn save(&self, out: &mut Vec<u8>) {
        out.push(*self as u8);
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), savestate::BadState> {
        use Phase::*;
        *self = *[Off, Attack, Decay, Sustain, Release].get(usize::from(savestate::take(input, 1)?[0])).ok_or(savestate::BadState)?;
        Ok(())
    }
}

/// An envelope's step: the volume level and the counter that decides
/// when it moves, by psx-spx's formula.
#[derive(Clone, Copy, Default)]
struct Envelope {
    level: i16,
    counter: u32,
}

state!(Envelope { level, counter });

impl Envelope {
    /// One tick of a rate (shift and step), linear or exponential,
    /// increasing or decreasing (phase-inverted for negative volumes).
    fn tick(&mut self, shift: u32, step: u32, exponential: bool, decreasing: bool, negative: bool) {
        // A rate of all ones never steps.
        if step | shift << 2 == 0x7f {
            return;
        }
        let level = i32::from(self.level);
        // Exponential increase slows down above 0x6000: the step is cut at
        // fast rates, the counter's increment at slow ones, both at 10.
        let slowed = exponential && !decreasing && level > 0x6000;
        let mut increment = 0x8000u32 >> shift.saturating_sub(11);
        if slowed && shift >= 10 {
            increment >>= if shift == 10 { 1 } else { 2 };
        }
        // Most ticks only count: the step is worked out when one is taken.
        self.counter += increment.max(1);
        if self.counter & 0x8000 == 0 {
            return;
        }
        self.counter = 0;
        let mut adsr_step = 7 - step as i32;
        if decreasing != negative {
            adsr_step = !adsr_step;
        }
        adsr_step <<= 11u32.saturating_sub(shift);
        if slowed && shift <= 10 {
            adsr_step >>= if shift == 10 { 1 } else { 2 };
        } else if exponential && decreasing {
            adsr_step = (adsr_step * level) >> 15;
        }
        let new = level + adsr_step;
        self.level = if !decreasing {
            new.clamp(-0x8000, 0x7fff)
        } else if negative {
            new.clamp(-0x8000, 0)
        } else {
            new.max(0)
        } as i16;
    }
}

/// A voice's volume on one side: direct, or sweeping.
#[derive(Clone, Copy, Default)]
struct Volume {
    register: u16,
    envelope: Envelope,
}

state!(Volume { register, envelope });

impl Volume {
    fn set(&mut self, value: u16) {
        self.register = value;
        if value & 0x8000 == 0 {
            self.envelope.level = (value << 1) as i16;
        }
    }

    fn tick(&mut self) {
        let r = u32::from(self.register);
        if r & 0x8000 == 0 {
            return;
        }
        let (exponential, decreasing, negative) = (r & 0x4000 != 0, r & 0x2000 != 0, r & 0x1000 != 0);
        self.envelope.tick(r >> 2 & 0x1f, r & 3, exponential, decreasing, negative);
    }

    fn level(&self) -> i32 {
        i32::from(self.envelope.level)
    }
}

#[derive(Clone, Copy, Default)]
struct Voice {
    volume: [Volume; 2],
    pitch: u16,
    start: u16,
    adsr: u32,
    repeat: u16,
    /// The block playing, the pitch counter in it, its decoded samples
    /// with the last three of the block before (for interpolation).
    address: u32,
    counter: u32,
    samples: [i16; 31],
    flags: u8,
    /// The last two samples decoded, for the prediction filter.
    history: [i16; 2],
    /// A loop address written after key on stays, whatever the samples
    /// say.
    keep_repeat: bool,
    phase: Phase,
    envelope: Envelope,
    /// The voice's output before its volume: pitch modulation reads it.
    out: i16,
    decoded: bool,
}

state!(Voice { volume, pitch, start, adsr, repeat, address, counter, samples, flags, history, keep_repeat, phase,
    envelope, out, decoded });

#[derive(Clone)]
pub struct Spu {
    ram: Box<[u8; RAM]>,
    /// Registers as last written, for those that read back as such.
    regs: [u16; 0x200],
    voices: [Voice; 24],
    main: [Volume; 2],
    attr: u16,
    irq: bool,
    endx: u32,
    /// The transfer's address in bytes (TSA counts in eights).
    transfer: u32,
    noise_timer: i32,
    noise: u16,
    /// Reverb: the current buffer address, and the last output (computed
    /// at 22050 Hz, held for two samples).
    reverb_address: u32,
    reverb_out: [i32; 2],
    reverb_in: [i32; 2],
    odd: bool,
    /// Where the four capture buffers are written next (in samples).
    capture: u32,
    /// The cycle the SPU has run up to.
    pub ran_to: u64,
    /// The samples made since the frame began (stereo, interleaved).
    pub output: Vec<i16>,
}

impl Default for Spu {
    fn default() -> Spu {
        Spu {
            ram: vec![0; RAM].into_boxed_slice().try_into().unwrap(),
            regs: [0; 0x200],
            voices: [Voice::default(); 24],
            main: [Volume::default(); 2],
            attr: 0,
            irq: false,
            endx: 0,
            transfer: 0,
            noise_timer: 0,
            noise: 0,
            reverb_address: 0,
            reverb_out: [0; 2],
            reverb_in: [0; 2],
            odd: false,
            capture: 0,
            ran_to: 0,
            output: Vec::with_capacity(2048),
        }
    }
}

state!(Spu { ram, regs, voices, main, attr, irq, endx, transfer, noise_timer, noise, reverb_address, reverb_out,
    reverb_in, odd, capture, ran_to });

fn clamp16(v: i32) -> i32 {
    v.clamp(-0x8000, 0x7fff)
}

impl Spu {
    fn ram16(&self, addr: u32) -> i16 {
        let i = addr as usize & (RAM - 2);
        i16::from_le_bytes([self.ram[i], self.ram[i + 1]])
    }

    fn set_ram16(&mut self, addr: u32, value: i16) {
        let i = addr as usize & (RAM - 2);
        self.ram[i..i + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn reg(&self, offset: u32) -> u16 {
        self.regs[(offset / 2) as usize & 0x1ff]
    }

    /// Whether the interrupt goes off for an access at `addr` (bytes) to
    /// `len` bytes.
    fn irq_hit(&mut self, addr: u32, len: u32) -> bool {
        if self.attr & 0x40 == 0 || self.irq {
            return false;
        }
        let at = u32::from(self.reg(0x1a4)) * 8;
        if at.wrapping_sub(addr) < len {
            self.irq = true;
            return true;
        }
        false
    }

    pub fn read(&self, offset: u32) -> u16 {
        match offset {
            0x000..=0x17f if offset & 0xf == 0xc => self.voices[offset as usize >> 4].envelope.level as u16,
            0x000..=0x17f if offset & 0xf == 0xe => self.voices[offset as usize >> 4].repeat,
            0x19c => self.endx as u16,
            0x19e => (self.endx >> 16) as u16,
            0x1aa => self.attr,
            0x1ae => {
                let mut s = self.attr & 0x3f;
                s |= u16::from(self.irq) << 6;
                s |= (self.attr & 0x20) << 2;
                s |= u16::from(self.capture >= 0x100) << 11;
                s
            }
            0x1b8 => self.main[0].envelope.level as u16,
            0x1ba => self.main[1].envelope.level as u16,
            0x200..=0x25f => {
                let v = &self.voices[(offset as usize - 0x200) >> 2];
                v.volume[(offset as usize >> 1) & 1].envelope.level as u16
            }
            _ => self.reg(offset),
        }
    }

    /// A register written; whether the interrupt went off.
    pub fn write(&mut self, offset: u32, value: u16) -> bool {
        self.regs[(offset / 2) as usize & 0x1ff] = value;
        match offset {
            0x000..=0x17f => {
                let v = &mut self.voices[offset as usize >> 4];
                match offset & 0xf {
                    0x0 => v.volume[0].set(value),
                    0x2 => v.volume[1].set(value),
                    0x4 => v.pitch = value,
                    0x6 => v.start = value,
                    0x8 => v.adsr = v.adsr & 0xffff_0000 | u32::from(value),
                    0xa => v.adsr = v.adsr & 0xffff | u32::from(value) << 16,
                    0xc => v.envelope.level = value as i16,
                    _ => {
                        v.repeat = value;
                        v.keep_repeat = true;
                    }
                }
            }
            0x180 => self.main[0].set(value),
            0x182 => self.main[1].set(value),
            0x188 => self.key_on(u32::from(value)),
            0x18a => self.key_on(u32::from(value) << 16),
            0x18c => self.key_off(u32::from(value)),
            0x18e => self.key_off(u32::from(value) << 16),
            0x1a2 => self.reverb_address = u32::from(value) * 8,
            0x1a6 => self.transfer = u32::from(value) * 8,
            0x1a8 => {
                let at = self.transfer;
                self.set_ram16(at, value as i16);
                self.transfer = (at + 2) & (RAM as u32 - 1);
                return self.irq_hit(at, 2);
            }
            0x1aa => {
                self.attr = value;
                // Clearing the enable acknowledges the interrupt.
                if value & 0x40 == 0 {
                    self.irq = false;
                }
            }
            _ => {}
        }
        false
    }

    fn key_on(&mut self, bits: u32) {
        for (n, v) in self.voices.iter_mut().enumerate() {
            if bits >> n & 1 != 0 {
                v.address = u32::from(v.start) * 8;
                v.counter = 0;
                v.phase = Phase::Attack;
                v.envelope = Envelope::default();
                v.keep_repeat = false;
                v.decoded = false;
                v.history = [0; 2];
                v.samples = [0; 31];
                self.endx &= !(1 << n);
            }
        }
    }

    fn key_off(&mut self, bits: u32) {
        for (n, v) in self.voices.iter_mut().enumerate() {
            if bits >> n & 1 != 0 && v.phase != Phase::Off {
                v.phase = Phase::Release;
                v.envelope.counter = 0;
            }
        }
    }

    /// DMA channel 4: a word written to sound RAM; whether the interrupt
    /// went off.
    pub fn dma_write(&mut self, word: u32) -> bool {
        let at = self.transfer;
        self.set_ram16(at, word as i16);
        self.set_ram16(at + 2, (word >> 16) as i16);
        self.transfer = (at + 4) & (RAM as u32 - 1);
        self.irq_hit(at, 4)
    }

    pub fn dma_read(&mut self) -> (u32, bool) {
        let at = self.transfer;
        let word = self.ram16(at) as u16 as u32 | (self.ram16(at + 2) as u16 as u32) << 16;
        self.transfer = (at + 4) & (RAM as u32 - 1);
        (word, self.irq_hit(at, 4))
    }

    /// Decodes the voice's next 16-byte block; whether that touched the
    /// interrupt address.
    fn decode(&mut self, n: usize) -> bool {
        let address = self.voices[n].address & (RAM as u32 - 16);
        let hit = self.irq_hit(address, 16);
        let block: [u8; 16] = self.ram[address as usize..address as usize + 16].try_into().unwrap();
        let v = &mut self.voices[n];
        let shift = match block[0] & 15 {
            s @ 0..=12 => u32::from(s),
            _ => 9,
        };
        let (f0, f1) = FILTERS[usize::from(block[0] >> 4 & 7).min(4)];
        v.flags = block[1];
        // The last three samples of the block before stay for interpolation.
        v.samples.copy_within(28..31, 0);
        let [mut old, mut older] = v.history.map(i32::from);
        for i in 0..28 {
            let nibble = (block[2 + i / 2] >> (4 * (i & 1))) & 15;
            let t = (i32::from((nibble << 4) as i8) << 8) >> shift;
            let s = clamp16(t + ((old * f0 + older * f1 + 32) >> 6));
            v.samples[3 + i] = s as i16;
            older = old;
            old = s;
        }
        v.history = [old as i16, older as i16];
        if v.flags & 4 != 0 && !v.keep_repeat {
            v.repeat = (address / 8) as u16;
        }
        v.decoded = true;
        hit
    }

    /// One voice's sample, moving it on (with the voices pitch modulated
    /// and those playing noise); whether the interrupt went off.
    #[inline(always)]
    fn voice(&mut self, n: usize, noise: i16, pmon: u32, non: u32) -> (i32, bool) {
        let mut hit = false;
        if !self.voices[n].decoded {
            hit |= self.decode(n);
        }
        let modulator = if n > 0 && pmon >> n & 1 != 0 { Some(self.voices[n - 1].out) } else { None };
        let v = &mut self.voices[n];
        // A silent voice (its envelope at 0) still moves on, but what it
        // reads makes no sound: no need to interpolate it.
        let sample = if v.envelope.level == 0 {
            0
        } else if non >> n & 1 != 0 {
            i32::from(noise)
        } else {
            let i = (v.counter >> 4 & 0xff) as usize;
            let at = (v.counter >> 12) as usize;
            let s = &v.samples[at..at + 4];
            let out = (GAUSS[0xff - i] * i32::from(s[0])) >> 15;
            let out = out + ((GAUSS[0x1ff - i] * i32::from(s[1])) >> 15);
            let out = out + ((GAUSS[0x100 + i] * i32::from(s[2])) >> 15);
            out + ((GAUSS[i] * i32::from(s[3])) >> 15)
        };
        let out = (sample * i32::from(v.envelope.level)) >> 15;
        v.out = out as i16;
        // The pitch counter, by the voice before's output when modulated.
        let mut step = u32::from(v.pitch);
        if let Some(m) = modulator {
            let factor = i32::from(m) + 0x8000;
            step = ((i32::from(step as u16 as i16) * factor) >> 15) as u32 & 0xffff;
        }
        v.counter += step.min(0x4000);
        if v.counter >= 28 << 12 {
            v.counter -= 28 << 12;
            // The block ends: its flags say where the voice goes on.
            if v.flags & 1 != 0 {
                self.endx |= 1 << n;
                v.address = u32::from(v.repeat) * 8;
                if v.flags & 2 == 0 && non >> n & 1 == 0 {
                    v.phase = Phase::Release;
                    v.envelope.level = 0;
                }
            } else {
                v.address = (v.address + 16) & (RAM as u32 - 1);
            }
            hit |= self.decode(n);
        }
        self.envelope(n);
        (out, hit)
    }

    /// The voice's ADSR envelope, a tick on.
    fn envelope(&mut self, n: usize) {
        let v = &mut self.voices[n];
        let (a1, a2) = (v.adsr & 0xffff, v.adsr >> 16);
        let e = &mut v.envelope;
        match v.phase {
            Phase::Off => {}
            Phase::Attack => {
                e.tick(a1 >> 10 & 0x1f, a1 >> 8 & 3, a1 & 0x8000 != 0, false, false);
                if e.level == 0x7fff {
                    v.phase = Phase::Decay;
                    e.counter = 0;
                }
            }
            Phase::Decay => {
                e.tick((a1 >> 4 & 0xf) << 2 >> 2, 0, true, true, false);
                let sustain = ((a1 & 0xf) + 1) * 0x800;
                if i32::from(e.level) <= sustain as i32 {
                    v.phase = Phase::Sustain;
                    e.counter = 0;
                }
            }
            Phase::Sustain => {
                e.tick(a2 >> 8 & 0x1f, a2 >> 6 & 3, a2 & 0x8000 != 0, a2 & 0x4000 != 0, false);
            }
            Phase::Release => {
                e.tick(a2 & 0x1f, 0, a2 & 0x20 != 0, true, false);
                if e.level == 0 {
                    v.phase = Phase::Off;
                }
            }
        }
    }

    fn noise_tick(&mut self) {
        let step = 4 + i32::from(self.attr >> 8 & 3);
        let shift = u32::from(self.attr >> 10 & 15);
        self.noise_timer -= step;
        let n = self.noise;
        let parity = (n >> 15 ^ n >> 12 ^ n >> 11 ^ n >> 10 ^ 1) & 1;
        if self.noise_timer < 0 {
            self.noise = n.wrapping_mul(2) | parity;
            self.noise_timer += 0x20000 >> shift;
            if self.noise_timer < 0 {
                self.noise_timer += 0x20000 >> shift;
            }
        }
    }

    /// A reverb address: the buffer address plus an offset (in bytes),
    /// wrapped within the work area.
    /// The offset is signed (an address minus a displacement can go below
    /// the buffer's start). No division in the usual case: the 32-bit ARM
    /// has none.
    fn reverb_at(&self, offset: u32) -> u32 {
        let base = u32::from(self.reg(0x1a2)) * 8;
        let size = (RAM as u32).saturating_sub(base).max(2) as i32;
        let at = self.reverb_address.wrapping_sub(base) as i32 + offset as i32;
        let at = if (0..size).contains(&at) {
            at
        } else if (size..2 * size).contains(&at) {
            at - size
        } else {
            at.rem_euclid(size)
        };
        base + at as u32
    }

    fn rev(&self, offset: u32) -> i32 {
        i32::from(self.ram16(self.reverb_at(offset)))
    }

    fn set_rev(&mut self, offset: u32, value: i32) {
        let at = self.reverb_at(offset);
        self.set_ram16(at, clamp16(value) as i16);
    }

    /// The reverb at 22050 Hz: both sides from the input, by psx-spx's
    /// formula, into the buffer (if enabled) and out.
    fn reverb(&mut self, input: [i32; 2]) {
        // The reverb registers (1F801DC0h on), copied out of the way.
        let regs: [u16; 32] = std::array::from_fn(|i| self.reg(0x1c0 + 2 * i as u32));
        let evol = [i32::from(self.reg(0x184) as i16), i32::from(self.reg(0x186) as i16)];
        let r = |o: u32| i32::from(regs[(o as usize - 0x1c0) / 2] as i16);
        let a = |o: u32| u32::from(regs[(o as usize - 0x1c0) / 2]) * 8;
        let mul = |x: i32, v: i32| (x * v) >> 15;
        let (v_iir, v_wall, v_apf1, v_apf2) = (r(0x1c4), r(0x1ce), r(0x1d0), r(0x1d2));
        let comb = [r(0x1c6), r(0x1c8), r(0x1ca), r(0x1cc)];
        let (d_apf1, d_apf2) = (a(0x1c0), a(0x1c2));
        let vin = [r(0x1fc), r(0x1fe)];
        let write = self.attr & 0x80 != 0;
        let mut out = [0; 2];
        for side in 0..2u32 {
            let s = 2 * side;
            let lin = mul(input[side as usize], vin[side as usize]);
            if write {
                // Same side, then different side reflection.
                let (m_same, d_same) = (a(0x1d4 + s), a(0x1e0 + s));
                let prev = self.rev(m_same.wrapping_sub(2));
                let v = mul(lin + mul(self.rev(d_same), v_wall) - prev, v_iir) + prev;
                self.set_rev(m_same, v);
                let (m_diff, d_diff) = (a(0x1e4 + s), a(0x1f0 + (2 - s)));
                let prev = self.rev(m_diff.wrapping_sub(2));
                let v = mul(lin + mul(self.rev(d_diff), v_wall) - prev, v_iir) + prev;
                self.set_rev(m_diff, v);
            }
            let combs = [a(0x1d8 + s), a(0x1dc + s), a(0x1e8 + s), a(0x1ec + s)];
            let mut o: i32 = combs.iter().zip(comb).map(|(&m, v)| mul(self.rev(m), v)).sum();
            let (m_apf1, m_apf2) = (a(0x1f4 + s), a(0x1f8 + s));
            let back = self.rev(m_apf1.wrapping_sub(d_apf1));
            o -= mul(v_apf1, back);
            if write {
                self.set_rev(m_apf1, o);
            }
            o = mul(clamp16(o), v_apf1) + back;
            let back = self.rev(m_apf2.wrapping_sub(d_apf2));
            o -= mul(v_apf2, back);
            if write {
                self.set_rev(m_apf2, o);
            }
            o = mul(clamp16(o), v_apf2) + back;
            out[side as usize] = clamp16(o);
        }
        self.reverb_out = [mul(out[0], evol[0]), mul(out[1], evol[1])];
        let base = u32::from(self.reg(0x1a2)) * 8;
        self.reverb_address = ((self.reverb_address + 2) & (RAM as u32 - 2)).max(base);
    }

    /// One sample of all of it, with the CD's sample mixed in; whether the
    /// interrupt went off.
    fn tick(&mut self, cd: [i16; 2]) -> bool {
        self.noise_tick();
        let noise = self.noise as i16;
        let pair = |low: u32| u32::from(self.reg(low)) | u32::from(self.reg(low + 2)) << 16;
        let (pmon, non, eon) = (pair(0x190), pair(0x194), pair(0x198));
        let mut mix = [0i32; 2];
        let mut reverb = [0i32; 2];
        let mut hit = false;
        for n in 0..24 {
            let (out, irq) = self.voice(n, noise, pmon, non);
            hit |= irq;
            let v = &mut self.voices[n];
            for side in 0..2 {
                // A silent voice adds nothing.
                if out != 0 {
                    let s = (out * v.volume[side].level()) >> 15;
                    mix[side] += s;
                    if eon >> n & 1 != 0 {
                        reverb[side] += s;
                    }
                }
                v.volume[side].tick();
            }
        }
        // CD audio, and the capture buffers (CD left and right, voices 1
        // and 3).
        let cd = cd.map(i32::from);
        let cd_volume = [i32::from(self.reg(0x1b0) as i16), i32::from(self.reg(0x1b2) as i16)];
        if self.attr & 1 != 0 {
            for side in 0..2 {
                let s = (cd[side] * cd_volume[side]) >> 15;
                mix[side] += s;
                if self.attr & 4 != 0 {
                    reverb[side] += s;
                }
            }
        }
        let at = self.capture * 2;
        self.set_ram16(at, cd[0] as i16);
        self.set_ram16(0x400 + at, cd[1] as i16);
        self.set_ram16(0x800 + at, self.voices[1].out);
        self.set_ram16(0xc00 + at, self.voices[3].out);
        hit |= [0, 0x400, 0x800, 0xc00].iter().any(|&b| self.irq_hit(b + at, 2));
        self.capture = (self.capture + 1) & 0x1ff;
        // Reverb runs at half the rate, left and right on alternate samples
        // on the chip; here both together every second sample.
        if self.odd {
            let input = [(self.reverb_in[0] + reverb[0]) >> 1, (self.reverb_in[1] + reverb[1]) >> 1];
            self.reverb(input);
        } else {
            self.reverb_in = reverb;
        }
        self.odd = !self.odd;
        let unmuted = self.attr & 0x4000 != 0;
        for (side, mix) in mix.into_iter().enumerate() {
            let s = clamp16(mix + self.reverb_out[side]);
            let s = if unmuted { clamp16((s * self.main[side].level()) >> 15) } else { 0 };
            self.output.push(s as i16);
            self.main[side].tick();
        }
        hit
    }
}

impl Psx {
    /// Runs the SPU up to now.
    pub(crate) fn spu_sync(&mut self) {
        let mut hit = false;
        while self.cycles >= self.spu.ran_to + CYCLES_PER_SAMPLE {
            self.spu.ran_to += CYCLES_PER_SAMPLE;
            let cd = self.cdrom.audio_sample();
            hit |= self.spu.tick(cd);
        }
        if hit {
            self.request(crate::SPU);
        }
    }

    pub(crate) fn spu_read(&mut self, offset: u32) -> u16 {
        self.spu_sync();
        self.spu.read(offset)
    }

    pub(crate) fn spu_write(&mut self, offset: u32, value: u16) {
        self.spu_sync();
        if self.spu.write(offset, value) {
            self.request(crate::SPU);
        }
    }
}
