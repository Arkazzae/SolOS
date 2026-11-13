//! Resampling: RESAMPLE's four-tap filter by the pitch, and DOWNSAMPLE_HALF.

use super::routines::{Cost, Rdram};
use super::{align, clamp, put_samples, read_samples, sample, set_sample, write_block, State};
use crate::N64;

/// RESAMPLE: from 4 samples before the input on, each output sample from four input ones by the
/// filter's phase, the input moving on by the pitch (16.16), 8 at a time (each block read before it
/// is written, and one at least); the last 4 input samples and the phase kept.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Resample {
    pub words: ResampleWords,
    pub rdram: Rdram,
    /// Where its code reads the filter in the data: four taps for each of 64 phases.
    pub filter: u16,
    /// Whether flag 2 is a loop (the input's 16 bytes kept about where it goes on put back before the
    /// input); otherwise flags 2 and 4 pick modes this leaves to the RSP (fewer or doubled samples
    /// before the input).
    pub loops: bool,
    /// Whether it keeps, beside the last 4 samples and the phase (10 bytes), the offset of the input's
    /// 16 bytes about where it goes on and, 16 bytes on, those 16 bytes.
    pub keeps_input: bool,
    /// Whether it puts its state together in the scratch (s7) and saves it from there: the scratch's
    /// first 10 bytes cleared when starting, the rest of the 32 as they were.
    pub stages: bool,
    /// Whether its first VSUB takes the carries a task before left in VCO.
    pub carries: bool,
    /// Whether its state's address is taken at 8 bytes.
    pub aligned: bool,
    pub cost: Cost,
}

/// How RESAMPLE reads its command.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum ResampleWords {
    /// Flags in w0's second byte, the pitch in its low half (half of it, 16.16), its state's address in
    /// w1; SETBUFF's input, output and count.
    SetBuff,
    /// Its state's address in w0; flags in w1's top 2 bits, then the pitch (16 bits), the input (12
    /// bits, from `base`) and which of the `outputs` (from `base`) it writes, `count` bytes.
    Packed { base: u16, outputs: [u16; 4], count: u16 },
}

impl Resample {
    pub(super) fn accepts(&self, w0: u32) -> bool {
        self.loops || w0 >> 16 & 6 == 0
    }
}

/// DOWNSAMPLE_HALF: every other sample of w1's high half (from `base`) into its low half, w0's low half
/// in samples rounded up to 8 (at least `least`), a sample at a time.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Downsample {
    pub base: u16,
    pub least: u16,
    pub cost: Cost,
}

impl N64 {
    pub(super) fn resample(&mut self, s: &mut State, v: &Resample, w0: u32, w1: u32) -> u64 {
        let (flags, pitch, state) = match v.words {
            ResampleWords::SetBuff => (w0 >> 16 & 0xff, (w0 & 0xffff) << 1, v.rdram.address(s, w1)),
            ResampleWords::Packed { base, outputs, count } => {
                let base = usize::from(base);
                let output = base + usize::from(outputs[(w1 & 3) as usize]);
                (s.input, s.output, s.count) = (base + (w1 >> 2 & 0xfff) as usize, output, usize::from(count));
                (w1 >> 30, (w1 >> 14 & 0xffff) << 1, v.rdram.address(s, w0))
            }
        };
        let state = if v.aligned { state & !7 } else { state };
        self.resample_blocks(s, v, flags & 1 != 0, v.loops && flags & 2 != 0, pitch, state);
        v.cost.cycles(s.count, flags & 1 != 0)
    }

    fn resample_blocks(&mut self, s: &mut State, v: &Resample, init: bool, looped: bool, pitch: u32, state: usize) {
        let scratch = if v.stages { s.scratch } else { None };
        // Staging, it loads the state into the scratch, or starting clears its first 10 bytes there
        // (the rest of the scratch, a loop's place among it, is what it was).
        let saved: [i16; 16] = match scratch {
            Some(scratch) => {
                if init {
                    self.dmem_clear(scratch, 10);
                } else {
                    self.dma_in(scratch, state, 32);
                }
                std::array::from_fn(|i| self.dmem_sample(scratch + 2 * i))
            }
            None => std::array::from_fn(|i| self.rdram_half(state + 2 * i)),
        };
        let mut start = s.input;
        if looped {
            for i in 0..8 {
                self.set_dmem_sample(s.input - 16 + 2 * i, saved[8 + i]);
            }
            // The offset as the microcode takes it (LH), below 16 as it keeps it.
            start = start.wrapping_sub(saved[5] as isize as usize) & 0xfff;
        }
        let position = (start.wrapping_sub(8) & 0xfff) / 2;
        for k in 0..4 {
            let v = if init { 0 } else { saved[k] };
            self.set_dmem_sample(2 * (position + k), v);
        }
        let accu = if init { 0 } else { saved[4] as u16 as u32 };
        let mut output = s.output / 2;
        let mut left = i32::from(s.count as u16 as i16);
        // Where the next output is from, in samples (16.16).
        let mut at = position << 16 | accu as usize;
        // A first VSUB taking the carries a task before left in VCO: those lanes of the first block are
        // half a step back, and the blocks after go on from its last.
        let mut carry = if v.carries { std::mem::take(&mut s.vco) } else { 0 };
        let dmem = self.dmem_mut();
        loop {
            let mut block = [0i16; 8];
            for (n, v) in block.iter_mut().enumerate() {
                let lane = at - (usize::from(carry) >> n & 1) * (pitch as usize >> 1);
                let (position, phase) = (lane >> 16, (lane & 0xfc00) >> 8);
                let taps = &s.filter[phase..phase + 4];
                let x: [i16; 4] = read_samples(dmem, 2 * position);
                // Each product rounded on its own (as VMULF does).
                let sum: i32 = (0..4).map(|k| (i32::from(x[k]) * i32::from(taps[k]) + 0x4000) >> 15).sum();
                *v = clamp(sum);
                at = if n == 7 { lane } else { at } + pitch as usize;
            }
            carry = 0;
            write_block(dmem, 2 * output, block);
            output += 8;
            left -= 16;
            if left <= 0 {
                break;
            }
        }
        let (position, accu) = (at >> 16, at & 0xffff);
        let mut bytes = [0; 12];
        put_samples(&mut bytes, &read_samples::<4>(self.dmem(), 2 * position));
        bytes[8..10].copy_from_slice(&(accu as u16).to_be_bytes());
        // The 16 bytes about where the input goes on, from a multiple of 16 past its start.
        let next = 2 * (position + 4);
        let skip = next.wrapping_sub(s.input) & 15;
        if v.keeps_input {
            bytes[10..].copy_from_slice(&(if skip == 0 { 0 } else { 16 - skip as u16 }).to_be_bytes());
        }
        if let Some(scratch) = scratch {
            // Put together in the scratch (bytes 12 to 15 as they were) and all 32 saved.
            let block = self.dmem_block(next - skip);
            let dmem = self.dmem_mut();
            for (i, &b) in bytes.iter().enumerate() {
                dmem[(scratch + i) & 0xfff] = b;
            }
            write_block(dmem, scratch + 16, block);
            self.audio_save(s, state & !7, scratch, 32);
            return;
        }
        self.audio_store(s, state, &bytes[..if v.keeps_input { 12 } else { 10 }]);
        if v.keeps_input {
            let mut bytes = [0; 16];
            put_samples(&mut bytes, &self.dmem_block(next - skip));
            self.audio_store(s, state + 16, &bytes);
        }
    }

    pub(super) fn downsample(&mut self, v: &Downsample, w0: u32, w1: u32) -> u64 {
        let base = usize::from(v.base);
        let count = align((w0 & 0xffff) as usize, 8).max(usize::from(v.least));
        let (from, to) = (base + (w1 >> 16) as usize, base + (w1 & 0xffff) as usize);
        let dmem = self.dmem_mut();
        for k in 0..count {
            set_sample(dmem, to + 2 * k, sample(dmem, from + 4 * k));
        }
        v.cost.cycles((w0 & 0xffff) as usize, false)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{halves, put16, staged, BASE, DATA, MULTIPLYING, STATE};

    #[test]
    fn resample_reads_a_block_before_writing_it() {
        // The output's second half is where the input's 4 samples from before go; at no pitch each
        // output is half the first of them.
        let mut n64 = staged(MULTIPLYING, &[(0x0800_0040, 0x0030_0010), (0x0500_0000, STATE as u32)]);
        put16(&mut n64, DATA + 0xc0, 0x4000);
        put16(&mut n64, STATE, 0x2000);
        n64.run_audio(usize::MAX);
        let outputs: Vec<i16> = (0..8).map(|i| n64.dmem_sample(BASE + 0x30 + 2 * i)).collect();
        assert_eq!(outputs, [0x1000; 8]);
        assert_eq!(halves(&n64, STATE), [0x1000, 0x1000, 0x1000, 0x1000, 0, 0, 0, 0]);
    }
}
