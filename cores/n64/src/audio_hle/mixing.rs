//! Mixing: MIXER (a buffer times a gain into another), INTERLEAVE (left and right into pairs) and
//! HILOGAIN (a buffer times a gain).

use super::routines::Cost;
use super::{align, clamp, inside, mix, read_block, read_samples, sample, set_sample, write_block, Blocks, Inside,
    State, Wrapping};
use crate::N64;

/// MIXER: the output (w1's low half, from `base`) times 0x7fff plus the input (w1's high half) times
/// the gain (w0's low half), with one rounding.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Mixer {
    pub base: u16,
    pub count: MixCount,
    pub order: MixOrder,
    pub cost: Cost,
}

/// How many bytes MIXER mixes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum MixCount {
    /// SETBUFF's count, rounded up to 32.
    SetBuff,
    /// w0's second byte in 16-byte units, rounded up to 32 (at least `least`).
    Units { least: u16 },
    /// Always this many.
    Fixed(u16),
}

/// The order of MIXER's reads and writes, which matters where the output overlaps the input.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum MixOrder {
    /// As if a sample at a time.
    Samples,
    /// 8 samples at a time, each block read before the one before it is written.
    Blocks,
    /// 16 samples a round, the next round's input and output read between the writes, 32 bytes ahead.
    Rounds,
}

/// INTERLEAVE: samples of a left and a right buffer in pairs.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Interleave {
    pub words: InterleaveWords,
    pub cost: Cost,
}

/// How INTERLEAVE reads its command.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum InterleaveWords {
    /// Into SETBUFF's output, SETBUFF's count rounded up to 16 bytes of each side, the left and right
    /// at w1's halves (from `base`); all read before any is written.
    SetBuff { base: u16 },
    /// Fixed buffers from `base`: into `to`, `count` bytes of `left` and `right`; all read before any
    /// is written.
    Fixed { base: u16, to: u16, left: u16, right: u16, count: u16 },
    /// Into w0's low half, the left and right at w1's halves, w0's second byte in 16-byte units rounded
    /// up to 8 bytes (at least 8) of each side, 4 samples of each read before they are written.
    Units,
}

/// HILOGAIN: w1's high half times a gain of 4.12 (w0's second byte), w0's low half in bytes rounded up
/// to 32 (at least 32), 8 samples at a time. Its loads and stores are of 16 bytes, so it runs here
/// only on 16-byte boundaries.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct HiLoGain {
    pub cost: Cost,
}

impl HiLoGain {
    pub(super) fn accepts(&self, w1: u32) -> bool {
        w1 >> 16 & 15 == 0
    }
}

/// MIXER's rounds of 16 samples.
fn mix_rounds<B: Blocks>(dmem: &mut [u8; 0x1000], gain: i16, from: usize, to: usize, count: usize) {
    let (mut in0, mut in1, mut out0, mut out1) =
        (B::read(dmem, from), B::read(dmem, from + 16), B::read(dmem, to), B::read(dmem, to + 16));
    for at in (0..count).step_by(32) {
        let a: [i16; 8] = std::array::from_fn(|n| mix(out0[n], in0[n], gain));
        let b: [i16; 8] = std::array::from_fn(|n| mix(out1[n], in1[n], gain));
        in0 = B::read(dmem, from + at + 32);
        B::write(dmem, to + at, a);
        out0 = B::read(dmem, to + at + 32);
        B::write(dmem, to + at + 16, b);
        in1 = B::read(dmem, from + at + 48);
        out1 = B::read(dmem, to + at + 48);
    }
}

impl N64 {
    pub(super) fn mixer(&mut self, s: &mut State, v: &Mixer, w0: u32, w1: u32) -> u64 {
        let base = usize::from(v.base);
        let (gain, from, to) = (w0 as i16, base + (w1 >> 16) as usize, base + (w1 & 0xffff) as usize);
        let units = (w0 >> 12 & 0xff0) as usize;
        let (count, work) = match v.count {
            MixCount::SetBuff => (align(s.count, 32), s.count),
            MixCount::Units { least } => (align(units, 32).max(usize::from(least)), units),
            MixCount::Fixed(count) => (usize::from(count), 0),
        };
        match v.order {
            MixOrder::Samples => self.mix_samples(gain, from, to, count / 2),
            MixOrder::Blocks => {
                let dmem = self.dmem_mut();
                let mut next = (read_block(dmem, to), read_block(dmem, from));
                for at in (0..count).step_by(16) {
                    let (out, x) = next;
                    next = (read_block(dmem, to + at + 16), read_block(dmem, from + at + 16));
                    write_block(dmem, to + at, std::array::from_fn(|n| mix(out[n], x[n], gain)));
                }
            }
            MixOrder::Rounds => {
                // The input and output are read 32 bytes ahead.
                let dmem = self.dmem_mut();
                if inside(from, count + 32) && inside(to, count + 32) {
                    mix_rounds::<Inside>(dmem, gain, from, to, count);
                } else {
                    mix_rounds::<Wrapping>(dmem, gain, from, to, count);
                }
            }
        }
        v.cost.cycles(work, false)
    }

    /// `count` samples of `from` times the gain mixed into `to`, a sample at a time. Where the output is
    /// not 2 to 14 bytes past the input, no sample written is one read later among the same 8, so 8 are
    /// read and then written at a time.
    fn mix_samples(&mut self, gain: i16, from: usize, to: usize, count: usize) {
        let dmem = self.dmem_mut();
        let mut i = 0;
        if !(2..=14).contains(&((to & 0xffe).wrapping_sub(from & 0xffe) & 0xffe)) {
            while i + 8 <= count {
                let (out, x) = (read_block(dmem, to + 2 * i), read_block(dmem, from + 2 * i));
                write_block(dmem, to + 2 * i, std::array::from_fn(|n| mix(out[n], x[n], gain)));
                i += 8;
            }
        }
        for i in i..count {
            let mixed = mix(sample(dmem, to + 2 * i), sample(dmem, from + 2 * i), gain);
            set_sample(dmem, to + 2 * i, mixed);
        }
    }

    pub(super) fn interleave(&mut self, s: &mut State, v: &Interleave, w0: u32, w1: u32) -> u64 {
        let units = (w0 >> 12 & 0xff0) as usize;
        let work = match v.words {
            InterleaveWords::SetBuff { base } => {
                let base = usize::from(base);
                if s.count != 0 {
                    let (left, right) = (base + (w1 >> 16) as usize, base + (w1 & 0xffff) as usize);
                    self.interleave_all(s.output, left, right, align(s.count, 16) / 2);
                }
                s.count
            }
            InterleaveWords::Fixed { base, to, left, right, count } => {
                let at = |o: u16| usize::from(base) + usize::from(o);
                self.interleave_all(at(to), at(left), at(right), usize::from(count) / 2);
                0
            }
            InterleaveWords::Units => {
                let (to, left, right) = ((w0 & 0xffff) as usize, (w1 >> 16) as usize, (w1 & 0xffff) as usize);
                let dmem = self.dmem_mut();
                for i in (0..align(units, 8).max(8) / 2).step_by(4) {
                    let (l, r): ([i16; 4], [i16; 4]) =
                        (read_samples(dmem, left + 2 * i), read_samples(dmem, right + 2 * i));
                    write_block(dmem, to + 4 * i, std::array::from_fn(|n| [l, r][n & 1][n / 2]));
                }
                units
            }
        };
        v.cost.cycles(work, false)
    }

    /// `frames` samples of the left and right buffers, all read before any is written, in pairs from
    /// `to`.
    fn interleave_all(&mut self, to: usize, left: usize, right: usize, frames: usize) {
        let before = *self.dmem();
        let dmem = self.dmem_mut();
        let mut i = 0;
        while i + 8 <= frames {
            let (l, r) = (read_block(&before, left + 2 * i), read_block(&before, right + 2 * i));
            for half in 0..2 {
                let pairs = std::array::from_fn(|n| if n & 1 == 0 { l[4 * half + n / 2] } else { r[4 * half + n / 2] });
                write_block(dmem, to + 4 * i + 16 * half, pairs);
            }
            i += 8;
        }
        for i in i..frames {
            set_sample(dmem, to + 4 * i, sample(&before, left + 2 * i));
            set_sample(dmem, to + 4 * i + 2, sample(&before, right + 2 * i));
        }
    }

    pub(super) fn hilogain(&mut self, v: &HiLoGain, w0: u32, w1: u32) -> u64 {
        let (count, buffer) = ((w0 & 0xffff) as usize, (w1 >> 16) as usize);
        let (whole, fraction) = ((w0 >> 20 & 15) as i32, ((w0 >> 16 & 15) << 12) as i32);
        let dmem = self.dmem_mut();
        // Each sample on its own, so 8 at a time.
        for at in (0..align(count, 32).max(32)).step_by(16) {
            let x = read_block(dmem, buffer + at);
            write_block(dmem, buffer + at, x.map(|x| {
                let x = i32::from(x);
                clamp((x * fraction >> 16) + x * whole)
            }));
        }
        v.cost.cycles(count, false)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{alike_wherever, run};

    /// MIXER, 32 bytes a round: 64 bytes from 0x600 into 0x700, read 32 bytes ahead.
    #[test]
    fn a_mixer_by_rounds_mixes_alike_wherever_its_buffers_are() {
        alike_wherever(&[0x8a0, 0x8a2, 0x8a8, 0x8c0, 0x7f8], |n64, d| {
            run(n64, &[(0x230a_88ce_5a25_311d, 0x0404_6543, (0x600 + d as u32) << 16 | (0x700 + d as u32))]);
        });
    }
}
