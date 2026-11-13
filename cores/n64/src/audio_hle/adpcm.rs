//! ADPCM: decoding frames of 4-bit (or 2-bit) residuals with a codebook of predictors, and LOADADPCM,
//! which loads the codebook.

use super::routines::{Cost, Rdram};
use super::{align, clamp, put_samples, read_block, write_block, State};
use crate::N64;

/// ADPCM: frames of a header (a scale and a book) and 16 residuals, decoded 16 samples at a time after
/// the last 16 of the frame before (none starting, flag 1; the loop's, flag 2; or the state's), which
/// are kept in the state at the end.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Adpcm {
    pub words: AdpcmWords,
    pub rdram: Rdram,
    /// Where its code reads the codebook in DMEM: 16 books of 32 bytes, those past the ones LOADADPCM
    /// loaded over the buffers that follow.
    pub book: u16,
    /// Whether flag 4 asks for frames of 2-bit residuals (a header and 4 bytes).
    pub two_bit: bool,
    /// Whether its state's and the loop's addresses are taken at 8 bytes.
    pub aligned: bool,
    pub cost: Cost,
}

/// How ADPCM reads its command.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum AdpcmWords {
    /// Flags in w0's second byte, its state's address in w1; SETBUFF's input, output and count.
    SetBuff,
    /// Its state's address in w0; flags in w1's top 4 bits, then the count (12 bits), the input (4
    /// bits) and the output (12 bits), both from `base`.
    Packed { base: u16 },
}

/// LOADADPCM: w0's low half in bytes from RDRAM at w1 into where its code keeps the codebook, by DMA.
/// Unless `empty`, a count of 0 is left to the RSP (its DMA of 0 bytes moves 4 KB rows).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct LoadAdpcm {
    pub book: u16,
    pub rdram: Rdram,
    pub empty: bool,
    pub cost: Cost,
}

/// ADPCM's prediction of 8 samples: each residual (shifted up 11) plus the codebook's first row
/// times the sample two before the 8 and its second row times the one before, and the second row's
/// entries before a sample's times the residuals before it (the sums wrapping in 32 bits), shifted
/// back down and clamped. The last sum lane by lane: each residual times the second row moved on.
#[inline(always)]
pub(super) fn predict(src: &[i16], book1: &[i16; 8], book2: &[i16; 8], l1: i16, l2: i16) -> [i16; 8] {
    let mut accu: [i32; 8] = std::array::from_fn(|i| {
        (i32::from(src[i]) << 11) + i32::from(book1[i]) * i32::from(l1) + i32::from(book2[i]) * i32::from(l2)
    });
    for m in 0..7 {
        let column: [i16; 8] = std::array::from_fn(|i| if i > m { book2[i - 1 - m] } else { 0 });
        for i in 0..8 {
            accu[i] += i32::from(column[i]) * i32::from(src[m]);
        }
    }
    accu.map(|a| clamp(a >> 11))
}

impl N64 {
    pub(super) fn adpcm(&mut self, s: &mut State, v: &Adpcm, w0: u32, w1: u32) -> u64 {
        let (flags, state) = match v.words {
            AdpcmWords::SetBuff => (w0 >> 16 & 0xff, v.rdram.address(s, w1)),
            AdpcmWords::Packed { base } => {
                let base = usize::from(base);
                (s.input, s.output, s.count) =
                    (base + (w1 >> 12 & 15) as usize, base + (w1 & 0xfff) as usize, (w1 >> 16 & 0xfff) as usize);
                (w1 >> 28, v.rdram.address(s, w0))
            }
        };
        self.adpcm_frames(s, v, flags, state);
        v.cost.cycles(s.count, flags & 1 != 0)
    }

    fn adpcm_frames(&mut self, s: &mut State, v: &Adpcm, flags: u32, state: usize) {
        let (init, looped, short) = (flags & 1 != 0, flags & 2 != 0, v.two_bit && flags & 4 != 0);
        let at = |address: usize| if v.aligned { address & !7 } else { address };
        let mut last = [0i16; 16];
        if !init {
            let from = at(if looped { s.loop_at as usize } else { state });
            for (i, v) in last.iter_mut().enumerate() {
                *v = self.rdram_half(from + 2 * i);
            }
        }
        let (mut input, mut output) = (s.input, s.output);
        let dmem = self.dmem_mut();
        write_block(dmem, output, last[..8].try_into().unwrap());
        write_block(dmem, output + 16, last[8..].try_into().unwrap());
        output += 32;
        let base = if short { 14 } else { 12 };
        for _ in 0..align(s.count, 32) / 32 {
            let code = dmem[input & 0xfff];
            input += 1;
            let shift = if u32::from(code >> 4) < base { base - u32::from(code >> 4) } else { 0 };
            let book = usize::from(v.book) + usize::from(code & 15) * 32;
            let mut frame = [0i16; 16];
            if short {
                for i in 0..4 {
                    let byte = i16::from(dmem[input & 0xfff]);
                    input += 1;
                    for k in 0..4 {
                        frame[4 * i + k] = ((byte << (8 + 2 * k)) & !0x3fff) >> shift;
                    }
                }
            } else {
                for i in 0..8 {
                    let byte = i16::from(dmem[input & 0xfff]);
                    input += 1;
                    frame[2 * i] = ((byte << 8) & !0xfff) >> shift;
                    frame[2 * i + 1] = ((byte << 12) & !0xfff) >> shift;
                }
            }
            let (book1, book2) = (read_block(dmem, book), read_block(dmem, book + 16));
            let first = predict(&frame[..8], &book1, &book2, last[14], last[15]);
            last[..8].copy_from_slice(&first);
            let second = predict(&frame[8..], &book1, &book2, last[6], last[7]);
            last[8..].copy_from_slice(&second);
            write_block(dmem, output, last[..8].try_into().unwrap());
            write_block(dmem, output + 16, last[8..].try_into().unwrap());
            output += 32;
        }
        let mut bytes = [0; 32];
        put_samples(&mut bytes, &last);
        self.audio_store(s, at(state), &bytes);
    }

    pub(super) fn load_adpcm(&mut self, s: &mut State, v: &LoadAdpcm, w0: u32, w1: u32) -> u64 {
        let count = (w0 & 0xffff) as usize;
        self.dma_in(usize::from(v.book), v.rdram.address(s, w1), count);
        v.cost.cycles(count, false)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{staged, BASE, MULTIPLYING, STATE};
    use super::*;

    #[test]
    fn adpcm_books_past_the_codebook_are_the_buffers_after_it() {
        // Four books loaded at 0x4c0; the frame asks for book 14, 0x1c0 bytes on, which is in the
        // buffers (BASE + 0xc0), where the microcode reads it.
        let mut n64 =
            staged(MULTIPLYING, &[(0x0b00_0080, 0x4000), (0x0800_0000, 0x0040_0020), (0x0101_0000, STATE as u32)]);
        let book: [i16; 16] = std::array::from_fn(|i| 0x100 * (i as i16 + 1));
        for (i, &v) in book.iter().enumerate() {
            n64.set_dmem_sample(BASE + 0xc0 + 2 * i, v);
        }
        // Book 14, shift 12: every residual is 1.
        n64.rsp.memory[BASE] = 0x0e;
        n64.rsp.memory[BASE + 1..BASE + 9].fill(0x11);
        n64.run_audio(usize::MAX);
        let (book1, book2): (&[i16; 8], &[i16; 8]) = (book[..8].try_into().unwrap(), book[8..].try_into().unwrap());
        let first = predict(&[1; 8], book1, book2, 0, 0);
        let second = predict(&[1; 8], book1, book2, first[6], first[7]);
        let out: Vec<i16> = (0..16).map(|i| n64.dmem_sample(BASE + 0x60 + 2 * i)).collect();
        assert_eq!(out, [first, second].concat());
        assert_ne!(first, predict(&[1; 8], &[0; 8], &[0; 8], 0, 0), "the book matters");
    }
}
