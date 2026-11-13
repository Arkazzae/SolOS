//! Filters: POLEF's two poles, and an 8-tap filter whose setup and run are two commands.

use super::routines::{Cost, Rdram};
use super::{align, clamp, inside, put_samples, write_block, Blocks, Inside, State, Wrapping};
use crate::N64;

/// POLEF: a two-pole filter of SETBUFF's input into its output (SETBUFF's count rounded up to 16
/// bytes, nothing for none) with the codebook's first two rows as coefficients (the second scaled by
/// the gain, w0's low half, and kept so), 8 samples at a time; the last 4 output samples kept in the
/// state at w1. It goes through the scratch (s7): the two outputs before come from there, the state's,
/// or starting with only the first 4 bytes cleared, what the command before left there.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Polef {
    /// Where its code reads the codebook in DMEM.
    pub book: u16,
    pub rdram: Rdram,
    pub cost: Cost,
}

/// An 8-tap filter: with flags of 2 or more (w0's second byte) its setup, w0's low half the count of
/// bytes the run filters, the 8 coefficients from RDRAM at w1 (at 8 bytes); otherwise its run, on the
/// buffer at w0's low half (flag 1 starting), its state at w1 (at 8 bytes). The setup leaves the count
/// in a register for the run, so a run comes here only right after its setup.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Filter {
    pub rdram: Rdram,
    pub setup: Cost,
    pub cost: Cost,
}

/// The filter's rounds of 8 samples, in place, after the 8 input samples before them: the input
/// samples of the last round.
fn filter<B: Blocks>(dmem: &mut [u8; 0x1000], taps: [i16; 8], previous: [i16; 8], buffer: usize,
    rounds: usize) -> [i16; 8] {
    // The 8 input samples before the block, then the block's.
    let mut window = [0i16; 16];
    window[..8].copy_from_slice(&previous);
    for round in 0..rounds {
        let at = buffer + 16 * round;
        window[8..].copy_from_slice(&B::read(dmem, at));
        // The sum of the products (doubled, as the accumulator has it) takes more than 32 bits: it
        // is the sum by the samples' upper bytes, shifted up 8, plus the sum by their lower ones,
        // rounded as the whole is (the lower sum's bits below 8 cannot carry into the result).
        let (mut high, mut low) = ([0i32; 8], [0i32; 8]);
        for k in 0..8 {
            for j in 0..8 {
                let x = window[8 + j - k];
                high[j] += i32::from(taps[k]) * i32::from(x >> 8);
                low[j] += i32::from(taps[k]) * i32::from(x & 0xff);
            }
        }
        B::write(dmem, at, std::array::from_fn(|j| clamp(high[j] + (low[j] + 0x4000 >> 8) >> 7)));
        window.copy_within(8.., 0);
    }
    window[..8].try_into().unwrap()
}

impl Filter {
    /// A setup, or a run right after one (`before` the command just before it, its routine and w0).
    pub(super) fn accepts(&self, w0: u32, before: Option<(&super::routines::Routine, u32)>) -> bool {
        w0 >> 16 & 0xff >= 2
            || before.is_some_and(|(routine, w0)| matches!(routine, super::routines::Routine::Filter(_))
                && w0 >> 16 & 0xff >= 2)
    }
}

impl N64 {
    pub(super) fn polef(&mut self, s: &mut State, v: &Polef, w0: u32, w1: u32) -> u64 {
        let state = v.rdram.address(s, w1);
        if s.count != 0 {
            self.polef_blocks(s, usize::from(v.book), w0 >> 16 & 1 != 0, w0 as i16, state);
        }
        v.cost.cycles(0, false)
    }

    /// POLEF's blocks. The scaling multiplies by the gain shifted up 2 in 16 bits, unsigned (VMUDM).
    fn polef_blocks(&mut self, s: &mut State, book: usize, init: bool, gain: i16, state: usize) {
        let scratch = s.scratch.unwrap_or_default();
        self.dmem_clear(scratch, 4);
        if !init {
            self.dma_in(scratch, state, 8);
        }
        let (mut l1, mut l2) = (self.dmem_sample(scratch + 4), self.dmem_sample(scratch + 6));
        // The codebook's second row scaled by the gain goes back in its place.
        let (first, before) = (self.dmem_block(book), self.dmem_block(book + 16));
        let times = i64::from((gain as u16) << 2);
        let scaled: [i16; 8] = std::array::from_fn(|i| ((i64::from(before[i]) * times) >> 16) as i16);
        write_block(self.dmem_mut(), book + 16, scaled);
        let (mut input, mut output) = (s.input, s.output);
        let mut count = align(s.count, 16);
        let mut frame = self.dmem_block(input);
        while count != 0 {
            let mut block = [0i16; 8];
            for (i, v) in block.iter_mut().enumerate() {
                // The accumulator's upper 32 bits (wrapping), shifted down 14 and clamped.
                let accu = i64::from(frame[i]) * i64::from(gain) + i64::from(first[i]) * i64::from(l1)
                    + i64::from(before[i]) * i64::from(l2)
                    + (0..i).map(|k| i64::from(scaled[k]) * i64::from(frame[i - 1 - k])).sum::<i64>();
                *v = clamp(accu as i32 >> 14);
            }
            // The next input is read before this output is written.
            input += 16;
            frame = self.dmem_block(input);
            write_block(self.dmem_mut(), output, block);
            (l1, l2) = (block[6], block[7]);
            output += 16;
            count -= 16;
        }
        self.audio_save(s, state & !7, output - 8, 8);
    }

    pub(super) fn filter(&mut self, s: &mut State, v: &Filter, w0: u32, w1: u32) -> u64 {
        let flags = w0 >> 16 & 0xff;
        let at = v.rdram.address(s, w1);
        if flags >= 2 {
            s.filter_count = w0 & 0xffff;
            s.filter_taps = std::array::from_fn(|i| self.rdram_half((at & !7) + 2 * i));
            return v.setup.cycles(0, false);
        }
        self.filter_run(s, flags == 1, (w0 & 0xffff) as usize, at);
        v.cost.cycles(s.filter_count as usize, flags == 1)
    }

    /// The filter's run: each sample of the buffer the sum of the last 8 input ones times the
    /// coefficients (the setup's averaged with the state's, rounded up), 8 at a time; the last 8
    /// input samples and the coefficients kept.
    fn filter_run(&mut self, s: &mut State, init: bool, buffer: usize, state: usize) {
        let state = state & !7;
        let old: [i16; 16] = std::array::from_fn(|i| if init { 0 } else { self.rdram_half(state + 2 * i) });
        let taps: [i16; 8] =
            std::array::from_fn(|i| ((i32::from(s.filter_taps[i]) + i32::from(old[8 + i]) + 1) >> 1) as i16);
        s.filter_taps = taps;
        // 8 samples a round, at least one round.
        let rounds = (s.filter_count as usize).div_ceil(16).max(1);
        let dmem = self.dmem_mut();
        let previous: [i16; 8] = old[..8].try_into().unwrap();
        let last = if inside(buffer, 16 * rounds) {
            filter::<Inside>(dmem, taps, previous, buffer, rounds)
        } else {
            filter::<Wrapping>(dmem, taps, previous, buffer, rounds)
        };
        // A VADDC leaves no carries.
        s.vco = 0;
        let mut bytes = [0; 32];
        put_samples(&mut bytes, &last);
        put_samples(&mut bytes[16..], &taps);
        self.audio_store(s, state, &bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{alike_wherever, halves, put16, run, staged, BASE, MULTIPLYING, SCRATCH, STATE};
    use super::*;

    #[test]
    fn polef_scales_by_the_gain_in_16_bits_and_starts_from_the_scratch() {
        // Rows of 0x1000 and 0x2000; a gain of 0x4000 shifted up 2 in 16 bits is 0, so the second
        // row becomes 0; starting, the two outputs before are what the scratch held after its
        // first 4 bytes (0x100 and 0x200).
        let mut n64 =
            staged(MULTIPLYING, &[(0x0b00_0020, 0x4000), (0x0800_0000, 0x0040_0010), (0x0e01_4000, STATE as u32)]);
        for i in 0..8 {
            put16(&mut n64, 0x4000 + 2 * i, 0x1000);
            put16(&mut n64, 0x4010 + 2 * i, 0x2000);
            n64.set_dmem_sample(BASE + 2 * i, 0x100);
        }
        n64.set_dmem_sample(SCRATCH, 0x7777);
        n64.set_dmem_sample(SCRATCH + 4, 0x100);
        n64.set_dmem_sample(SCRATCH + 6, 0x200);
        n64.run_audio(usize::MAX);
        assert_eq!(n64.dmem_block(0x4d0), [0; 8], "the scaled row");
        assert_eq!(n64.dmem_sample(SCRATCH), 0, "the scratch's first bytes cleared");
        // 0x100 * 0x4000 + 0x1000 * 0x100 + 0x2000 * 0x200, shifted down 14.
        assert_eq!(n64.dmem_block(BASE + 0x40), [0x240; 8]);
        assert_eq!(halves(&n64, STATE)[..4], [0x240; 4]);
    }

    /// The filter's setup (64 bytes, coefficients from RDRAM) and run on 0x800, starting and going on.
    #[test]
    fn the_filter_runs_alike_wherever_its_buffer_is() {
        for flags in [0, 1] {
            alike_wherever(&[0x7c0, 0x7c2, 0x7c8, 0x7d0, 0x7f8], |n64, d| {
                run(n64, &[(0x8be6_d551_4159_5ba7, 0x0502_0040, 0x4000),
                    (0x8be6_d551_4159_5ba7, 0x0500_0800 | flags << 16 | d as u32, 0x5000)]);
            });
        }
    }

    #[test]
    fn the_filters_sums_in_32_bits_are_those_in_64() {
        let mut seed = 7u32;
        let mut next = || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) as i16
        };
        let corners = [-0x8000, -0x7fff, -0x4000, -0x100, -1, 0, 1, 0xff, 0x100, 0x7fff];
        for round in 0..20_000 {
            let mut pick = || if round % 3 == 0 { corners[next() as usize % corners.len()] } else { next() };
            let taps: [i16; 8] = std::array::from_fn(|_| pick());
            let window: [i16; 16] = match round {
                0 => [-0x8000; 16],
                1 => [0x7fff; 16],
                _ => std::array::from_fn(|_| pick()),
            };
            let mut dmem = [0; 0x1000];
            write_block(&mut dmem, 0x100, window[8..].try_into().unwrap());
            filter::<Inside>(&mut dmem, taps, window[..8].try_into().unwrap(), 0x100, 1);
            let wanted: [i16; 8] = std::array::from_fn(|j| {
                let sum: i64 = (0..8).map(|k| 2 * i64::from(taps[k]) * i64::from(window[8 + j - k])).sum();
                clamp(((0x8000 + sum) >> 16) as i32)
            });
            assert_eq!(super::super::read_block(&dmem, 0x100), wanted, "{taps:?} {window:?}");
        }
    }
}
