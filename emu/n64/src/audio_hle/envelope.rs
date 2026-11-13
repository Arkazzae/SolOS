//! Envelopes: SETVOL's values, ENVSETUP1 and ENVSETUP2, and ENVMIXER, which mixes a note into the dry
//! and wet buffers by its volumes as they move. ENVMIXER comes in three kinds: volumes ramping toward
//! SETVOL's targets, eight lanes of 16.16 with the note's state kept in RDRAM; volumes from ENVSETUP's
//! values, stepping after each block of 8 samples; and volumes ENVSETUP1 and ENVSETUP2 leave in
//! registers right before it, stepping each 8 samples, the reverb's too.

use super::routines::{Cost, Rdram};
use super::{align, clamp, inside, mix, put_samples, Acc, Blocks, Inside, State, Wrapping};
use crate::N64;

/// How a ramping ENVMIXER's volumes move toward their targets, a block at a time.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Ramp {
    /// Times the rate (VMUDL, VMADM, VMADN, VMADH).
    Multiply,
    /// Plus the rate (VADDC, VADD).
    Add,
}

/// SETVOL: with `three`, three of a note's values a command (w0's low half, w1's halves), by flags 4
/// and 2 in w0's second byte: the left volume with the dry and wet gains (both), the right target and
/// rate (4), or the left target and rate. Otherwise one thing a command, by its flags: with 8 the dry
/// and wet gains (w0's and w1's low halves); else a side's (flag 2 the left) volume (flag 4, w0's low
/// half), or its target (w0's low half) and rate (w1).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct SetVol {
    pub three: bool,
    pub cost: Cost,
}

/// One of SETVOL's values, as a store of 16 bits sets it: a side's (0 the left, 1 the right) target,
/// rate's upper or lower half and volume, and the dry and wet gains.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Value {
    Target(usize),
    RateHigh(usize),
    RateLow(usize),
    Volume(usize),
    Dry,
    Wet,
}

impl Value {
    fn set(self, s: &mut State, v: u16) {
        match self {
            Value::Target(k) => s.target[k] = v as i16,
            Value::RateHigh(k) => s.rate[k] = (s.rate[k] as u32 & 0xffff | u32::from(v) << 16) as i32,
            Value::RateLow(k) => s.rate[k] = (s.rate[k] as u32 & !0xffff | u32::from(v)) as i32,
            Value::Volume(k) => s.volume[k] = v as i16,
            Value::Dry => s.dry = v as i16,
            Value::Wet => s.wet = v as i16,
        }
    }
}

/// A store of w1's low half as one of SETVOL's values: where a command's entry lands in SETVOL's code.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct SetValue {
    pub value: Value,
    pub cost: Cost,
}

/// ENVSETUP1 (the reverb's volume in w0's third byte, shifted up 8, its step in w0's low half, the left
/// and right volumes' steps in w1's halves) and ENVSETUP2 (the left and right volumes, w1's halves).
/// With `registers`, they leave them in registers for an ENVMIXER right after them; otherwise they keep
/// them for ENVMIXER, which reads them back whenever it comes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct EnvSetup {
    pub registers: bool,
    pub cost: Cost,
}

/// What ENVSETUP1 and ENVSETUP2 keep for an ENVMIXER that reads them back: the reverb's volume, the
/// left and right volumes and their steps.
#[derive(Clone, Copy, Default)]
pub(super) struct Envelope {
    reverb: u16,
    volumes: [u16; 2],
    steps: [u16; 2],
}

/// ENVMIXER's kinds.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum EnvMixer {
    Ramping(Ramping),
    /// The input (w0's second byte, in 16-byte units from `base`), w0's third byte in samples rounded
    /// up to 8, times the left and right volumes ENVSETUP2 kept (negated where w0's bits 1 and 0 ask)
    /// into the dry buffers (w1's top two bytes, in 16-byte units from `base`), and times the reverb's
    /// volume too into the wet ones (w1's low two bytes; swapped with bit 2), 8 samples at a time; the
    /// left and right volumes step by ENVSETUP1's steps after each block, the reverb's does not.
    Stepping { base: u16, cost: Cost },
    /// The input (w0's second byte, in 16-byte units) times the left and right volumes ENVSETUP1 and
    /// ENVSETUP2 left in registers right before it into the dry buffers (w1's top two bytes, in 16-byte
    /// units), and those times the reverb's volume into the wet ones (w1's low two bytes), 16 samples a
    /// round for w0's third byte in samples (at least one round): the dry sides negated bit by bit where
    /// w0's bits 1 and 0 ask, the wet ones with 0xfffc and 0xfffe (bits 3 and 2, as the microcode has
    /// them), swapped with bit 4. The volumes, a block and the block after in a vector register, step
    /// each 8 samples (VADDC, wrapping) by the steps in scalar registers, which it doubles.
    Registers { cost: Cost },
}

/// A ramping ENVMIXER: volumes of 8 lanes of 16.16 (upper and lower halves) moving toward SETVOL's
/// targets by its rates and held there, the note's state (the lanes, the targets, rates and gains)
/// kept in RDRAM. A note starting gives lane n the volume plus (n + 1) eighths of the step (the last
/// one 0xffff / 0x10000), each block of 8 samples after takes the step once more.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Ramping {
    pub words: RampingWords,
    pub rdram: Rdram,
    /// Where its code reads the eighths in the data (1/8 to 8/8, the last as 0xffff).
    pub eighths: u16,
    pub ramp: Ramp,
    /// Whether its holds compare signed (VLT rising, for a rate of 0 too; VGE falling), leaving VCC,
    /// rather than unsigned (VCL, rising only for a rate above 0), whether a continuing note's targets,
    /// rates and gains are loaded over SETVOL's own (so a note starting after it without SETVOL goes on
    /// with them), and whether its first VSUB takes the carries a task before left in VCO (it leaves
    /// none): all three, or none.
    pub signed: bool,
    /// Whether it turns a side's input over, bit by bit, where a gain is odd: the dry gain's low bit
    /// for the left (less VCO's carry), the wet gain's for the right.
    pub flips: bool,
    /// Whether it puts the state together in the scratch (s7) and saves it from there.
    pub stages: bool,
    pub cost: Cost,
    /// Cycles more for each side whose volume falls.
    pub falling: u16,
}

/// How a ramping ENVMIXER reads its command.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum RampingWords {
    /// Flags in w0's second byte (1 a note starts, 8 the wet buffers too), its state's address in w1;
    /// SETBUFF's input, output (the dry left) and count, and auxiliary buffers.
    SetBuff,
    /// Fixed buffers from `base`: the input, the dry left and right, the wet left and right (always
    /// all four), `count` bytes; the right volume in w0's low half, flag 1 in w0's second byte, its
    /// state's address in w1.
    Note { base: u16, input: u16, dry: [u16; 2], wet: [u16; 2], count: u16 },
}

/// How many blocks of 8 samples a ramping ENVMIXER mixes, from its count left: 16 samples' worth a
/// block until none is left, and with a note starting at least two (it mixes its first block before
/// the loop's test).
pub(super) fn ramping_rounds(left: i32, init: bool) -> usize {
    (left.max(0) as usize).div_ceil(16).max(if init { 2 } else { 1 })
}

/// One block of a ramping ENVMIXER's mixing: 8 samples of the input, turned over where asked, times
/// each lane's volume times a buffer's gain (VMULF, doubled and rounded at bit 16, so at bit 15 here),
/// mixed into the dry left and right buffers and with `aux` the wet ones, in the microcode's order.
#[inline(always)]
fn ramping_round<B: Blocks>(dmem: &mut [u8; 0x1000], input: usize, out: &[usize; 4], aux: bool,
    high: &[[i16; 8]; 2], flip: &[[i16; 8]; 2], gains: [i16; 2]) {
    let x = B::read(dmem, input);
    for k in 0..2 {
        for (buffer, g) in [(k, gains[0]), (2 + k, gains[1])].into_iter().take(if aux { 2 } else { 1 }) {
            let block = B::read(dmem, out[buffer]);
            let volume: [i16; 8] = std::array::from_fn(|n| clamp(i32::from(high[k][n]) * i32::from(g) + 0x4000 >> 15));
            B::write(dmem, out[buffer], std::array::from_fn(|n| mix(block[n], x[n] ^ flip[k][n], volume[n])));
        }
    }
}

/// A stepping ENVMIXER's blocks of 8 samples.
#[allow(clippy::too_many_arguments)]
fn stepping_blocks<B: Blocks, const SWAP: bool>(dmem: &mut [u8; 0x1000], input: usize, out: [usize; 4],
    mut volume: [u16; 2], ramp: [u16; 2], reverb: i32, negate: [i32; 2], blocks: usize) {
    for block in 0..blocks {
        let at = 16 * block;
        let x = B::read(dmem, input + at);
        let side: [[i32; 8]; 2] = std::array::from_fn(|k| {
            std::array::from_fn(|n| (i32::from(x[n]) * i32::from(volume[k]) >> 16) ^ negate[k])
        });
        for k in 0..2 {
            let dry = B::read(dmem, out[k] + at);
            B::write(dmem, out[k] + at, std::array::from_fn(|n| clamp(i32::from(dry[n]) + side[k][n])));
        }
        if reverb != 0 {
            for k in 0..2 {
                let from = &side[if SWAP { 1 - k } else { k }];
                let wet = B::read(dmem, out[2 + k] + at);
                B::write(dmem, out[2 + k] + at,
                    std::array::from_fn(|n| clamp(i32::from(wet[n]) + (from[n] * reverb >> 16))));
            }
        }
        for k in 0..2 {
            volume[k] = volume[k].wrapping_add(ramp[k]);
        }
    }
}

/// The rounds of 16 samples of an ENVMIXER with its volumes in registers: the outputs read before they
/// are written, in the microcode's order.
fn register_rounds<B: Blocks, const SWAP: bool>(dmem: &mut [u8; 0x1000], env: &mut [u16; 8], steps: [u16; 8],
    input: usize, buffers: [usize; 4], negate: [i16; 4], rounds: usize) {
    // Where the wet sides go among the four buffers.
    let wet_to = if SWAP { [3, 2] } else { [2, 3] };
    let times = |x: i16, v: u16| ((i32::from(x) * i32::from(v)) >> 16) as i16;
    let [dry_left, dry_right, wet_left, wet_right] = buffers;
    let mut first = B::read(dmem, input);
    for round in 0..rounds {
        let at = 32 * round;
        let x = [first, B::read(dmem, input + at + 16)];
        // The two blocks' dry left, dry right, wet left and wet right, as read.
        let mut sums = buffers.map(|to| [B::read(dmem, to + at), B::read(dmem, to + at + 16)]);
        for half in 0..2 {
            for n in 0..8 {
                for k in 0..2 {
                    let dry = times(x[half][n], env[2 * k + half]) ^ negate[k];
                    let wet = times(dry, env[4 + half]) ^ negate[2 + k];
                    sums[k][half][n] = sums[k][half][n].saturating_add(dry);
                    sums[wet_to[k]][half][n] = sums[wet_to[k]][half][n].saturating_add(wet);
                }
            }
        }
        B::write(dmem, dry_left + at, sums[0][0]);
        B::write(dmem, dry_right + at, sums[1][0]);
        B::write(dmem, dry_left + at + 16, sums[0][1]);
        B::write(dmem, dry_right + at + 16, sums[1][1]);
        B::write(dmem, wet_left + at, sums[2][0]);
        B::write(dmem, wet_right + at, sums[3][0]);
        first = B::read(dmem, input + at + 32);
        B::write(dmem, wet_left + at + 16, sums[2][1]);
        B::write(dmem, wet_right + at + 16, sums[3][1]);
        for (v, d) in env.iter_mut().zip(steps) {
            *v = v.wrapping_add(d);
        }
    }
}

impl N64 {
    /// SETVOL.
    pub(super) fn set_vol(&mut self, s: &mut State, v: &SetVol, w0: u32, w1: u32) -> u64 {
        let flags = w0 >> 16 & 0xff;
        if v.three {
            let values = match flags & 6 {
                6 => [Value::Volume(0), Value::Dry, Value::Wet],
                4 => [Value::Target(1), Value::RateHigh(1), Value::RateLow(1)],
                _ => [Value::Target(0), Value::RateHigh(0), Value::RateLow(0)],
            };
            for (value, half) in values.into_iter().zip([w0 as u16, (w1 >> 16) as u16, w1 as u16]) {
                value.set(s, half);
            }
        } else if flags & 8 != 0 {
            s.dry = w0 as i16;
            s.wet = w1 as i16;
        } else {
            let side = usize::from(flags & 2 == 0);
            if flags & 4 != 0 {
                s.volume[side] = w0 as i16;
            } else {
                s.target[side] = w0 as i16;
                s.rate[side] = w1 as i32;
            }
        }
        v.cost.cycles(0, false)
    }

    pub(super) fn set_value(&mut self, s: &mut State, v: &SetValue, w1: u32) -> u64 {
        v.value.set(s, w1 as u16);
        v.cost.cycles(0, false)
    }

    pub(super) fn env_setup1(&mut self, s: &mut State, v: &EnvSetup, w0: u32, w1: u32) -> u64 {
        let reverb = (w0 >> 8 & 0xff00) as u16;
        if v.registers {
            s.volume_lanes = [0, 0, 0, 0, reverb, reverb.wrapping_add(w0 as u16), 0, 0];
            s.volume_steps = [(w1 >> 16) as u16, w1 as u16, w0 as u16];
        } else {
            s.envelope.reverb = reverb;
            s.envelope.steps = [(w1 >> 16) as u16, w1 as u16];
        }
        v.cost.cycles(0, false)
    }

    pub(super) fn env_setup2(&mut self, s: &mut State, v: &EnvSetup, w1: u32) -> u64 {
        let (left, right) = ((w1 >> 16) as u16, w1 as u16);
        if v.registers {
            s.volume_lanes[..4].copy_from_slice(&[left, left.wrapping_add(s.volume_steps[0]), right,
                right.wrapping_add(s.volume_steps[1])]);
        } else {
            s.envelope.volumes = [left, right];
        }
        v.cost.cycles(0, false)
    }

    /// ENVMIXER.
    pub(super) fn env_mixer(&mut self, s: &mut State, v: &EnvMixer, w0: u32, w1: u32) -> u64 {
        match v {
            EnvMixer::Ramping(v) => {
                let flags = w0 >> 16 & 0xff;
                let state = v.rdram.address(s, w1);
                let aux = match v.words {
                    RampingWords::SetBuff => flags & 8 != 0,
                    RampingWords::Note { base, input, dry, wet, count } => {
                        let at = |o: u16| usize::from(base) + usize::from(o);
                        s.volume[1] = w0 as i16;
                        (s.input, s.output, s.count) = (at(input), at(dry[0]), usize::from(count));
                        (s.dry_right, s.wet_left, s.wet_right) = (at(dry[1]), at(wet[0]), at(wet[1]));
                        true
                    }
                };
                self.ramping_envmixer(s, v, flags & 1 != 0, aux, state);
                let falling = s.rate.iter().filter(|&&r| r < 0).count() as u64;
                v.cost.cycles(s.count, flags & 1 != 0) + u64::from(v.falling) * falling
            }
            EnvMixer::Stepping { base, cost } => {
                self.stepping_envmixer(s, usize::from(*base), w0, w1);
                cost.cycles((w0 >> 8 & 0xff) as usize, false)
            }
            EnvMixer::Registers { cost } => {
                self.register_envmixer(s, w0, w1);
                cost.cycles((w0 >> 8 & 0xff) as usize, false)
            }
        }
    }

    /// A ramping ENVMIXER, 8 samples at a time as the microcode does it: the input mixed into the dry
    /// left and right buffers (and with `aux` the wet ones) by each lane's volume times the dry and wet
    /// gain. The 80 bytes kept are the left lanes' upper and lower halves, the right ones', then the
    /// targets, rates and gains (which a note going on takes from there, not from SETVOL).
    fn ramping_envmixer(&mut self, s: &mut State, v: &Ramping, init: bool, aux: bool, state: usize) {
        let scratch = if v.stages { s.scratch } else { None };
        let mut params = [s.target[0], (s.rate[0] >> 16) as i16, s.rate[0] as i16, s.target[1],
            (s.rate[1] >> 16) as i16, s.rate[1] as i16, s.dry, s.wet];
        let (mut high, mut low) = ([[0i16; 8]; 2], [[0u16; 8]; 2]);
        if !init {
            // Staging, it loads the 80 bytes into the scratch.
            let saved = |n64: &Self, at: usize| match scratch {
                Some(scratch) => n64.dmem_sample(scratch + at),
                None => n64.rdram_half(state + at),
            };
            if let Some(scratch) = scratch {
                self.dma_in(scratch, state, 80);
            }
            for n in 0..8 {
                for k in 0..2 {
                    high[k][n] = saved(self, 32 * k + 2 * n);
                    low[k][n] = saved(self, 32 * k + 16 + 2 * n) as u16;
                }
                params[n] = saved(self, 64 + 2 * n);
            }
            // Loaded where SETVOL keeps its own, a note starting after this one without SETVOL goes
            // on with them.
            if v.signed {
                s.target = [params[0], params[3]];
                s.rate = [i32::from(params[1]) << 16 | i32::from(params[2] as u16),
                    i32::from(params[4]) << 16 | i32::from(params[5] as u16)];
                (s.dry, s.wet) = (params[6], params[7]);
            }
        }
        let rate = |k: usize| (params[3 * k + 1], params[3 * k + 2] as u16);
        let (signed, flips) = (v.signed, v.flips);
        // The first VSUB (for the left's turning over, where it does) takes the carries a task before
        // left in VCO, and this command leaves none.
        let carry = if signed { std::mem::take(&mut s.vco) } else { 0 };
        // A lane times the rate (VMUDL, VMADM, VMADN, VMADH).
        let times = |high: i16, low: u16, (rh, rl): (i16, u16)| {
            let acc = Acc(i64::from(low) * i64::from(rl) >> 16)
                .add(i64::from(high) * i64::from(rl))
                .add(i64::from(low) * i64::from(rh))
                .add(i64::from(high) * i64::from(rh) << 16);
            (acc.high(), acc.low())
        };
        // Rising, the smaller: unsigned (VCL, with no flags), or signed (VLT, for a rate of 0 too);
        // falling (VGE), the larger. The signed compares leave in VCC where the lanes were below the
        // target, rising, or not below it, falling.
        let hold = |high: &mut [i16; 8], k: usize| {
            let target = params[3 * k];
            let mut vcc = 0;
            for (n, v) in high.iter_mut().enumerate() {
                vcc |= u16::from((*v < target) == (rate(k).0 >= 0)) << n;
                *v = match (signed, rate(k).0) {
                    (true, 0..) => (*v).min(target),
                    (false, 1..) => (*v as u16).min(target as u16) as i16,
                    _ => (*v).max(target),
                };
            }
            vcc
        };
        let mut left = i32::from(s.count as u16 as i16);
        let (mut input, mut out) = (s.input, [s.output, s.dry_right, s.wet_left, s.wet_right]);
        if init {
            for k in 0..2 {
                // The first block's lanes go an eighth of the step further each.
                let volume = s.volume[k];
                let (dh, dl) = match v.ramp {
                    Ramp::Multiply => {
                        let (th, tl) = times(volume, 0, rate(k));
                        (clamp(i32::from(th) - i32::from(volume)), tl)
                    }
                    Ramp::Add => rate(k),
                };
                for n in 0..8 {
                    let f = i64::from(s.eighths[n]);
                    let acc = Acc(f * i64::from(dl) >> 16).add(f * i64::from(dh)).add(i64::from(volume) << 16);
                    (high[k][n], low[k][n]) = (acc.high(), acc.low());
                }
                let _ = hold(&mut high[k], k);
            }
        }
        let flip: [[i16; 8]; 2] = std::array::from_fn(|k| std::array::from_fn(|n| match (flips, k) {
            (false, _) => 0,
            (true, 0) => -(params[6] & 1) - (carry >> n & 1) as i16,
            _ => -(params[7] & 1),
        }));
        // The blocks of 8 samples the loop mixes take its buffers through `Inside` where they stay
        // before DMEM's end.
        let blocks = 16 * ramping_rounds(left, init);
        let inside = inside(input, blocks) && out[..if aux { 4 } else { 2 }].iter().all(|&at| inside(at, blocks));
        // A note starting mixes its first block before the loop's test.
        let mut first = init;
        loop {
            if !first {
                for k in 0..2 {
                    for n in 0..8 {
                        (high[k][n], low[k][n]) = match v.ramp {
                            Ramp::Multiply => times(high[k][n], low[k][n], rate(k)),
                            Ramp::Add => {
                                let sum = u32::from(low[k][n]) + u32::from(rate(k).1);
                                (clamp(i32::from(high[k][n]) + i32::from(rate(k).0) + (sum >> 16) as i32), sum as u16)
                            }
                        };
                    }
                    let vcc = hold(&mut high[k], k);
                    if signed {
                        s.vcc = Some(vcc);
                    }
                }
            }
            let dmem = self.dmem_mut();
            if inside {
                ramping_round::<Inside>(dmem, input, &out, aux, &high, &flip, [params[6], params[7]]);
            } else {
                ramping_round::<Wrapping>(dmem, input, &out, aux, &high, &flip, [params[6], params[7]]);
            }
            input += 16;
            for at in &mut out {
                *at += 16;
            }
            left -= 16;
            if left <= 0 && !first {
                break;
            }
            first = false;
        }
        let mut bytes = [0; 80];
        for k in 0..2 {
            put_samples(&mut bytes[32 * k..], &high[k]);
            put_samples(&mut bytes[32 * k + 16..], &low[k].map(|v| v as i16));
        }
        put_samples(&mut bytes[64..], &params);
        if let Some(scratch) = scratch {
            // Put together in the scratch (over the task's header) and saved from there.
            let dmem = self.dmem_mut();
            for (i, &b) in bytes.iter().enumerate() {
                dmem[(scratch + i) & 0xfff] = b;
            }
            self.audio_save(s, state & !7, scratch, 80);
            return;
        }
        self.audio_store(s, state, &bytes);
    }

    /// A stepping ENVMIXER (at 0 the reverb's volume leaves the wet buffers as they are).
    fn stepping_envmixer(&mut self, s: &mut State, base: usize, w0: u32, w1: u32) {
        let Envelope { reverb, volumes, steps } = s.envelope;
        let input = base + ((w0 >> 12) & 0xff0) as usize;
        let blocks = align((w0 >> 8 & 0xff) as usize, 8) / 8;
        // Negated bit by bit (one less than minus) where asked.
        let negate = [w0 & 2, w0 & 1].map(|bit| if bit != 0 { -1 } else { 0 });
        let out = [w1 >> 24, w1 >> 16, w1 >> 8, w1].map(|b| base + ((b & 0xff) << 4) as usize);
        let dmem = self.dmem_mut();
        let inside = inside(input, 16 * blocks) && out.iter().all(|&at| inside(at, 16 * blocks));
        // The wet sides swapped where asked.
        let mix = match (inside, w0 & 4 != 0) {
            (true, false) => stepping_blocks::<Inside, false>,
            (true, true) => stepping_blocks::<Inside, true>,
            (false, false) => stepping_blocks::<Wrapping, false>,
            (false, true) => stepping_blocks::<Wrapping, true>,
        };
        mix(dmem, input, out, volumes, steps, i32::from(reverb), negate, blocks);
    }

    /// An ENVMIXER with its volumes in registers. Each 16 samples' outputs are read before they are
    /// written, in the microcode's order; it leaves no carries in VCO.
    fn register_envmixer(&mut self, s: &mut State, w0: u32, w1: u32) {
        let input = (w0 >> 12 & 0xff0) as usize;
        let buffers = [24, 16, 8, 0].map(|shift| ((w1 >> shift & 0xff) << 4) as usize);
        let negate = [-((w0 as i16 & 2) >> 1), -(w0 as i16 & 1), -((w0 as i16 & 8) >> 1), -((w0 as i16 & 4) >> 1)];
        let step = s.volume_steps.map(|v| v.wrapping_mul(2));
        let steps = [step[0], step[0], step[1], step[1], step[2], step[2], 0, 0];
        // 16 samples a round, at least one round; the input is read 16 samples ahead.
        let rounds = ((w0 >> 8 & 0xff) as usize).div_ceil(16).max(1);
        let dmem = self.dmem_mut();
        let inside = inside(input, 32 * rounds + 16) && buffers.iter().all(|&at| inside(at, 32 * rounds));
        let env = &mut s.volume_lanes;
        // The wet sides swapped where asked.
        match (inside, w0 & 0x10 != 0) {
            (true, false) => register_rounds::<Inside, false>(dmem, env, steps, input, buffers, negate, rounds),
            (true, true) => register_rounds::<Inside, true>(dmem, env, steps, input, buffers, negate, rounds),
            (false, false) => register_rounds::<Wrapping, false>(dmem, env, steps, input, buffers, negate, rounds),
            (false, true) => register_rounds::<Wrapping, true>(dmem, env, steps, input, buffers, negate, rounds),
        }
        s.vco = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::super::recognise::Microcode;
    use super::super::tests::{alike_wherever, halves, machine, run, staged, BASE, DATA, LIST, MULTIPLYING, STAGED,
        STATE};
    use super::*;

    #[test]
    fn a_ramping_envmixer_goes_by_eighths_then_a_block_at_a_time() {
        // 16 samples of 0.5 in at 0, dry left out at 0x40 (right at 0x80), both volumes 0x1000; the one
        // that adds adds 8.0 a block, the one that multiplies multiplies by 1.5 and is held at 0x1a00.
        for (envmixer, rate, target, high, low) in [
            (STAGED[3], 0x0008_0000, 0x7fff, [0x1009, 0x100a, 0x100b, 0x100c, 0x100d, 0x100e, 0x100f, 0x100f], 0xfff8),
            (MULTIPLYING, 0x0001_8000, 0x1a00, [0x1980, 0x1a00, 0x1a00, 0x1a00, 0x1a00, 0x1a00, 0x1a00, 0x1a00], 0xf400),
        ] {
            let mut n64 = staged(envmixer, &[
                (0x0800_0000, 0x0040_0020),
                (0x0808_0080, 0),
                (0x0902_0000 | target, rate),
                (0x0900_0000 | target, rate),
                (0x0906_1000, 0),
                (0x0904_1000, 0),
                (0x0908_7fff, 0),
                (0x0301_0000, STATE as u32),
            ]);
            for i in 0..16 {
                n64.set_dmem_sample(BASE + 2 * i, 0x4000);
            }
            n64.run_audio(usize::MAX);
            // The state is the last block's lanes: in the first, the last lane goes 0xffff / 0x10000 of
            // the way.
            for side in 0..2 {
                assert_eq!(halves(&n64, STATE + 32 * side), high, "{envmixer:x}");
                assert_eq!(halves(&n64, STATE + 32 * side + 16), [0, 0, 0, 0, 0, 0, 0, low], "{envmixer:x}");
            }
            assert_eq!(halves(&n64, STATE + 64), [target as u16, (rate >> 16) as u16, rate as u16, target as u16,
                (rate >> 16) as u16, rate as u16, 0x7fff, 0]);
            // The input times the volume times the dry gain, rounded once.
            let out = n64.dmem_sample(BASE + 0x40 + 16);
            assert_eq!(out, mix(0, 0x4000, (i32::from(high[0]) * 0x7fff * 2 + 0x8000 >> 16) as i16));
            assert_eq!(n64.dmem_sample(BASE + 0x80 + 16), out);
        }
    }

    #[test]
    fn ramping_rounds_count_the_loops_blocks() {
        for count in -40..300 {
            for init in [false, true] {
                let (mut left, mut first, mut blocks) = (count, init, 0);
                loop {
                    blocks += 1;
                    left -= 16;
                    if left <= 0 && !first {
                        break;
                    }
                    first = false;
                }
                assert_eq!(ramping_rounds(count, init), blocks, "{count} {init}");
            }
        }
    }

    #[test]
    fn a_ramping_envmixer_mixes_alike_wherever_its_buffers_are() {
        // 0x40 bytes (4 blocks) from 0x200 into 0x300 and 0x380, and with the auxiliary flag 0x400
        // and 0x480: moved on so that the last ends at DMEM's end, and so that a block of it, or of
        // the others, runs past the end.
        let kinds = [(0, STAGED[3], 1), (8, MULTIPLYING, 1), (8, STAGED[3], 0), (0, MULTIPLYING, 0)];
        for (aux, envmixer, flags) in kinds {
            let last = if aux != 0 { 0x480 } else { 0x380 };
            let edge = 0x1000 - BASE - last - 0x40;
            alike_wherever(&[edge, edge + 2, edge + 8, edge + 0x7a, edge + 0x108, 0x7f8], |n64, d| {
                let d = d as u32;
                run(n64, &[(STAGED[8], 0x0800_0200 + d, (0x300 + d) << 16 | 0x40),
                    (STAGED[8], 0x0808_0380 + d, (0x400 + d) << 16 | (0x480 + d)), (STAGED[9], 0x0902_7000, 0x0003_8000),
                    (STAGED[9], 0x0900_1000, 0xfffe_8000), (STAGED[9], 0x0906_3000, 0), (STAGED[9], 0x0904_6000, 0),
                    (STAGED[9], 0x0908_7abc, 0x5678), (envmixer, 0x0300_0000 | (flags | aux) << 16, STATE as u32)]);
            });
        }
    }

    /// A machine with a microcode of a table of 24 commands, whose ENVMIXER steps by ENVSETUP's kept
    /// values, its buffers from 0x450; a sound task of `commands`.
    fn stepping(commands: &[(u32, u32)]) -> N64 {
        let microcode = Microcode::with(0x2042_60a5_4103_40a0, None, &[(2, 0x51e0_57c9_2312_4b5b),
            (0x12, 0x5e83_1743_bdf9_f6f2), (0x13, 0x9718_9fc4_3c8d_6275), (0x14, 0x4c48_80e8_cdd5_183f),
            (0x15, 0xe996_02a7_7093_988f), (0x16, 0x4571_2eaa_4864_27c0)]);
        machine(microcode, 0x300, &[(0, &[0, 1, 2, 0xffff])], commands)
    }

    /// A note loaded, mixed by a stepping ENVMIXER into dry left at half and dry right at a quarter
    /// negated (bit by bit, one below minus), both saved; DMEM addresses from 0x450.
    #[test]
    fn a_note_mixed_into_left_and_negated_right() {
        const SOURCE: usize = 0x3000;
        const SAVED: usize = 0x4000;
        let mut n64 = stepping(&[
            (0x0200_0200, 0x200),                        // CLEARBUFF 0x200, 0x200 bytes
            (0x1401_0100, SOURCE as u32),               // LOADBUFF 16 bytes to 0x100
            (0x1200_0000, 0),                           // ENVSETUP1: no reverb, no ramps
            (0x1600_0000, 0x8000_4000),                 // ENVSETUP2: left 1/2, right 1/4
            (0x1310_0801, 0x2030_383c),                 // ENVMIXER 8 samples from 0x100, right negated
            (0x1501_0200, SAVED as u32),                // SAVEBUFF dry left
            (0x1501_0300, SAVED as u32 + 16),           // SAVEBUFF dry right
        ]);
        for i in 0..8 {
            n64.rdram[SOURCE + 2 * i..SOURCE + 2 * i + 2].copy_from_slice(&1000i16.to_be_bytes());
        }
        n64.run_audio(usize::MAX);
        let sample = |n64: &N64, at: usize| i16::from_be_bytes([n64.rdram[at], n64.rdram[at + 1]]);
        assert_eq!(n64.rsp.memory[0x450 + 0x100..0x450 + 0x102], 1000i16.to_be_bytes(), "loaded from 0x450 on");
        for i in 0..8 {
            assert_eq!((sample(&n64, SAVED + 2 * i), sample(&n64, SAVED + 16 + 2 * i)), (500, -251), "sample {i}");
        }
    }

    /// A stepping ENVMIXER of 64 samples from 0x100 into 0x200, 0x300, 0x400 and 0x500 (in 16-byte
    /// units from 0x450), with and without the reverb, negated and swapped: moved on so that the last
    /// buffer ends at DMEM's end, and past it.
    #[test]
    fn a_stepping_envmixer_mixes_alike_wherever_its_buffers_are() {
        let kinds = [((0x1200_0000, 0x0123_ff00), 0), ((0x1245_0000, 0x0123_ff00), 7), ((0x1280_0000, 0), 4)];
        for ((w0, w1), flags) in kinds {
            let edge = 0x1000 - 0x450 - 0x500 - 0x80;
            alike_wherever(&[edge, edge + 16, 0x7f0], |n64, d| {
                let unit = |at: usize| ((at + d) & 0xfff) as u32 >> 4;
                run(n64, &[(0x5e83_1743_bdf9_f6f2, w0, w1), (0x4571_2eaa_4864_27c0, 0x1600_0000, 0x8000_c123),
                    (0x9718_9fc4_3c8d_6275, 0x1300_4000 | unit(0x100) << 16 | flags,
                    unit(0x200) << 24 | unit(0x300) << 16 | unit(0x400) << 8 | unit(0x500))]);
            });
        }
    }

    /// A machine with a microcode of a table of 16 commands at 0 whose ENVMIXER mixes a note from fixed
    /// buffers from 0x4f0, turning its inputs over where a gain is odd or not, and whose SETVOL sets three
    /// values a command (and command 14 a store of the right rate's lower half where it does not turn);
    /// a sound task of `commands`.
    fn note(flips: bool, commands: &[(u32, u32)]) -> N64 {
        let mut routines = vec![(9, 0x36aa_15c6_d4a6_ce81)];
        if flips {
            routines.push((3, 0xc54f_64c6_31fe_67b2));
        } else {
            routines.extend([(3, 0x51d2_1406_7127_af26), (14, 0xb63b_9459_d11b_3d0d)]);
        }
        let microcode = Microcode::with(0xcb1b_2518_4499_c184, None, &routines);
        let eighths = [0x2000, 0x4000, 0x6000, 0x8000, 0xa000, 0xc000, 0xe000, 0xffff];
        let data: [(usize, &[u16]); 3] =
            [(0x20, &[0xf000, 0x0f00, 0x00f0, 0x000f]), (0xa0, &eighths), (0xb0, &[0x0c39, 0x66ad])];
        machine(microcode, 0x2c0, &data, commands)
    }

    #[test]
    fn a_note_turns_the_left_over_where_the_dry_gain_is_odd_less_vcos_carry() {
        // Both volumes at their targets of 0.5, the dry gain 1 less 2^-15 (odd), no wet; 0.25 in. The
        // ENVMIXER that turns inputs over turns the left's (lane 0 less the carry VCO kept from the task
        // before); the other does not.
        for flips in [true, false] {
            let mut n64 = note(flips, &[
                (0x0906_4000, 0x7fff_0000),
                (0x0904_4000, 0),
                (0x0900_4000, 0),
                (0x0301_4000, STATE as u32),
            ]);
            for i in 0..0x170 / 2 {
                n64.set_dmem_sample(0x4f0 + 2 * i, 0x2000);
            }
            n64.rsp.vu.set_control(0, 1);
            n64.run_audio(usize::MAX);
            let gain = ((0x4000i32 * 0x7fff + 0x4000) >> 15) as i16;
            let left = |n: usize| n64.dmem_sample(0x4f0 + 0x4e0 + 2 * n);
            let turned = |x: i16| mix(0, if flips { x } else { 0x2000 }, gain);
            assert_eq!([left(0), left(1), left(0x170 / 2 - 1)], [turned(0x2000 ^ !1), turned(!0x2000), turned(!0x2000)]);
            assert_eq!(n64.dmem_sample(0x4f0 + 0x650), mix(0, 0x2000, gain));
            // It leaves no carries, and in VCC where the right's lanes were below its target.
            assert_eq!((n64.rsp.vu.control(0), n64.rsp.vu.control(1)), (0, 0));
        }
    }

    #[test]
    fn a_command_whose_entry_lands_in_setvol_stores_there() {
        // The right rate's low half, kept with the note's state.
        let mut n64 = note(false, &[
            (0x0906_4000, 0x7fff_0000),
            (0x0904_4000, 0x0001_0000),
            (0x0900_4000, 0),
            (0x0e00_0000, 0x1234_8000),
            (0x0301_4000, STATE as u32),
        ]);
        let microcode = n64.audio.known[0].1.unwrap();
        assert!(microcode.runs(&n64.rdram, LIST, 40));
        n64.run_audio(usize::MAX);
        assert_eq!(n64.rdram_half(STATE + 64 + 8), 1);
        assert_eq!(n64.rdram_half(STATE + 64 + 10) as u16, 0x8000);
        // Where it lands elsewhere, a task with one goes to the RSP.
        let n64 = note(true, &[(0x0e00_0000, 0)]);
        assert!(!n64.audio.known[0].1.unwrap().runs(&n64.rdram, LIST, 8));
        assert_eq!(n64.audio.known[0].0, DATA as u32);
    }

    /// An ENVMIXER with its volumes in registers: 64 samples from 0x100 into 0x200, 0x300, 0x400 and
    /// 0x500 (in 16-byte units), as they are, negated and swapped; moved on so that the last ends at
    /// DMEM's end, and past.
    #[test]
    fn a_register_envmixer_mixes_alike_wherever_its_buffers_are() {
        for flags in [0, 0x1b] {
            alike_wherever(&[0xa80, 0xa90, 0x7f0], |n64, d| {
                let unit = |at: usize| ((at + d) & 0xfff) as u32 >> 4;
                run(n64, &[(0x5df4_5da9_8463_92e5, 0x0145_1234, 0x0321_ff00),
                    (0x5c86_db39_7f54_7678, 0x0200_0000, 0x8000_c123),
                    (0x8a50_6ae4_0b5f_aa83, 0x0300_4000 | unit(0x100) << 16 | flags,
                    unit(0x200) << 24 | unit(0x300) << 16 | unit(0x400) << 8 | unit(0x500))]);
            });
        }
    }
}
