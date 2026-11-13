//! Buffers: SETBUFF, SEGMENT and SETLOOP, which set what later commands use; CLEARBUFF, DMEMMOVE and
//! DMEMMOVE2 within DMEM; LOADBUFF and SAVEBUFF between RDRAM and DMEM; and the DMAs, clears and copies
//! the commands are made of, wrapping at DMEM's end as the RSP does.

use super::routines::{Cost, Rdram};
use super::{align, State};
use crate::N64;

/// SETBUFF: the input, output (w0's low half, w1's high half, from `base`) and count (w1's low half, in
/// bytes) later commands work on. With `aux`, flag 8 in w0's second byte sets the auxiliary buffers
/// instead: the dry right, wet left and wet right.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct SetBuff {
    pub base: u16,
    pub aux: bool,
    pub cost: Cost,
}

/// CLEARBUFF: w1's low half in bytes, rounded up to 16 (at least `least`), from w0's low half (from
/// `base`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct ClearBuff {
    pub base: u16,
    pub least: u16,
    pub cost: Cost,
}

/// DMEMMOVE: w1's low half in bytes, rounded up to 16 (at least `least`), from w0's low half to w1's
/// high half (from `base`): all read before any is written, or `chunk` bytes at a time.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct DmemMove {
    pub base: u16,
    pub chunk: Option<u16>,
    pub least: u16,
    pub cost: Cost,
}

/// DMEMMOVE2: w1's low half in bytes, rounded up to 32, as many times over as w0's second byte says,
/// 32 bytes at a time from w0's low half to w1's high half (from `base`), so that a block repeats where
/// the output follows it; with `once`, at least 32 bytes once. With `aligned`, it runs here only on
/// 16-byte boundaries (its loads and stores are of 16 bytes).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct DmemMove2 {
    pub base: u16,
    pub once: bool,
    pub aligned: bool,
    pub cost: Cost,
}

impl DmemMove2 {
    pub(super) fn accepts(&self, w0: u32, w1: u32) -> bool {
        !self.aligned || (w0 | w1 >> 16) & 15 == 0
    }
}

/// LOADBUFF (RDRAM at w1 into DMEM) and SAVEBUFF (DMEM into RDRAM at w1). Unless `empty`, a count of 0
/// is left to the RSP (its DMA of 0 bytes moves 4 KB rows).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Transfer {
    pub words: TransferWords,
    pub rdram: Rdram,
    pub empty: bool,
    pub cost: Cost,
}

/// How LOADBUFF and SAVEBUFF read their command.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum TransferWords {
    /// SETBUFF's input (LOADBUFF) or output (SAVEBUFF) at 4 bytes, and its count rounded up to 8 bytes;
    /// RDRAM at 8 bytes; nothing for a count of 0.
    SetBuff,
    /// DMEM at w0's low half (from `base`), w0's second byte in 16-byte units: byte for byte, or with
    /// `dma` both addresses taken at 8 bytes and the count rounded up to 8.
    Units { base: u16, dma: bool },
    /// DMEM at w0's low 12 bits (from `base`), taken at 8 bytes, the count in its next 12 bits rounded up
    /// to 8; RDRAM at 8 bytes.
    Bytes { base: u16 },
}

impl Transfer {
    pub(super) fn accepts(&self, w0: u32) -> bool {
        self.empty || w0 >> 16 & 0xff != 0
    }
}

/// SETLOOP: the loop's RDRAM address (w1) for ADPCM. `keeps` is where its code keeps it in DMEM if at a
/// fixed place, over whatever the data has there (a table of commands' entries it covers lead
/// elsewhere once it ran).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct SetLoop {
    pub rdram: Rdram,
    pub keeps: Option<u16>,
    pub cost: Cost,
}

impl N64 {
    pub(super) fn set_buff(&mut self, s: &mut State, v: &SetBuff, w0: u32, w1: u32) -> u64 {
        let base = usize::from(v.base);
        let (a, b, c) = (base + (w0 & 0xffff) as usize, base + (w1 >> 16) as usize, base + (w1 & 0xffff) as usize);
        if v.aux && w0 >> 16 & 8 != 0 {
            (s.dry_right, s.wet_left, s.wet_right) = (a, b, c);
        } else {
            (s.input, s.output, s.count) = (a, b, (w1 & 0xffff) as usize);
        }
        v.cost.cycles(0, false)
    }

    pub(super) fn set_loop(&mut self, s: &mut State, v: &SetLoop, w1: u32) -> u64 {
        s.loop_at = v.rdram.address(s, w1) as u32;
        v.cost.cycles(0, false)
    }

    pub(super) fn clear_buff(&mut self, v: &ClearBuff, w0: u32, w1: u32) -> u64 {
        let count = (w1 & 0xffff) as usize;
        self.dmem_clear(usize::from(v.base) + (w0 & 0xffff) as usize, align(count, 16).max(usize::from(v.least)));
        v.cost.cycles(count, false)
    }

    pub(super) fn dmem_move_command(&mut self, v: &DmemMove, w0: u32, w1: u32) -> u64 {
        let base = usize::from(v.base);
        let (from, to, count) = (base + (w0 & 0xffff) as usize, base + (w1 >> 16) as usize, (w1 & 0xffff) as usize);
        let bytes = align(count, 16).max(usize::from(v.least));
        match v.chunk {
            Some(chunk) => self.dmem_copy(from, to, bytes, usize::from(chunk)),
            None => self.dmem_move(from, to, bytes),
        }
        v.cost.cycles(count, false)
    }

    pub(super) fn dmem_move2(&mut self, v: &DmemMove2, w0: u32, w1: u32) -> u64 {
        let base = usize::from(v.base);
        let (times, length) = ((w0 >> 16 & 0xff) as usize, align((w1 & 0xffff) as usize, 32));
        let count = if v.once { times.max(1) * length.max(32) } else { times * length };
        self.dmem_copy(base + (w0 & 0xffff) as usize, base + (w1 >> 16) as usize, count, 32);
        v.cost.cycles(count, false)
    }

    pub(super) fn load_buff(&mut self, s: &mut State, v: &Transfer, w0: u32, w1: u32) -> u64 {
        let from = v.rdram.address(s, w1);
        let count = match v.words {
            TransferWords::SetBuff => {
                if s.count != 0 {
                    self.dmem_load(s.input & !3, from & !7, align(s.count, 8));
                }
                s.count
            }
            TransferWords::Units { base, dma } => {
                let (at, units) = (usize::from(base) + (w0 & 0xffff) as usize, (w0 >> 12 & 0xff0) as usize);
                if dma {
                    self.dma_in(at, from, units);
                } else {
                    self.dmem_load(at, from, units);
                }
                units
            }
            TransferWords::Bytes { base } => {
                let (count, at) = ((w0 >> 12 & 0xfff) as usize, usize::from(base) + (w0 & 0xfff) as usize & !7);
                let from = from & !7;
                self.dmem_load(at, from, align(count, 8).min(self.rdram.len().saturating_sub(from)));
                count
            }
        };
        v.cost.cycles(count, false)
    }

    pub(super) fn save_buff(&mut self, s: &mut State, v: &Transfer, w0: u32, w1: u32) -> u64 {
        let to = v.rdram.address(s, w1);
        let count = match v.words {
            TransferWords::SetBuff => {
                if s.count != 0 {
                    self.audio_save(s, to & !7, s.output & !3, align(s.count, 8));
                }
                s.count
            }
            TransferWords::Units { base, dma } => {
                let (at, units) = (usize::from(base) + (w0 & 0xffff) as usize, (w0 >> 12 & 0xff0) as usize);
                if dma {
                    self.audio_save(s, to & !7, at & !7, align(units, 8));
                } else {
                    self.audio_save(s, to, at, units);
                }
                units
            }
            TransferWords::Bytes { base } => {
                let (count, at) = ((w0 >> 12 & 0xfff) as usize, usize::from(base) + (w0 & 0xfff) as usize & !7);
                self.audio_save(s, to & !7, at, align(count, 8));
                count
            }
        };
        v.cost.cycles(count, false)
    }

    /// Writes RDRAM, as the machine does (`wrote_rdram`), noting it.
    pub(super) fn audio_store(&mut self, s: &mut State, at: usize, bytes: &[u8]) {
        let at = at & 0x7f_ffff;
        let len = bytes.len().min(self.rdram.len() - at);
        self.wrote_rdram(at, len);
        self.rdram[at..at + len].copy_from_slice(&bytes[..len]);
        s.written.push((at, len));
    }

    /// Writes `count` bytes of DMEM from `from` (wrapping at its end) to RDRAM, as `audio_store`.
    pub(super) fn audio_save(&mut self, s: &mut State, to: usize, from: usize, count: usize) {
        let at = to & 0x7f_ffff;
        let len = count.min(self.rdram.len() - at);
        self.wrote_rdram(at, len);
        let from = from & 0xfff;
        if from + len <= 0x1000 {
            self.rdram[at..at + len].copy_from_slice(&self.rsp.memory[from..from + len]);
        } else {
            for i in 0..len {
                self.rdram[at + i] = self.rsp.memory[(from + i) & 0xfff];
            }
        }
        s.written.push((at, len));
    }

    /// Copies `count` bytes of RDRAM from `from` into DMEM at `to`, a byte at a time (both wrapping
    /// at their ends).
    pub(super) fn dmem_load(&mut self, to: usize, from: usize, count: usize) {
        let (to, from) = (to & 0xfff, from & 0x7f_ffff);
        if to + count <= 0x1000 && from + count <= self.rdram.len() {
            self.rsp.memory[to..to + count].copy_from_slice(&self.rdram[from..from + count]);
        } else {
            for i in 0..count {
                self.rsp.memory[(to + i) & 0xfff] = self.rdram[(from + i) & 0x7f_ffff];
            }
        }
    }

    /// The RSP's DMA of `count` bytes from RDRAM into DMEM: both addresses taken at 8 bytes, the
    /// length rounded up to 8.
    pub(super) fn dma_in(&mut self, at: usize, from: usize, count: usize) {
        let (at, from) = (at & 0xff8, from & 0x7f_fff8);
        self.dmem_load(at, from, align(count, 8).min(self.rdram.len().saturating_sub(from)));
    }

    /// Clears `count` bytes of DMEM from `at` (wrapping at its end).
    pub(super) fn dmem_clear(&mut self, at: usize, count: usize) {
        let at = at & 0xfff;
        if at + count <= 0x1000 {
            self.rsp.memory[at..at + count].fill(0);
        } else {
            for i in 0..count {
                self.rsp.memory[(at + i) & 0xfff] = 0;
            }
        }
    }

    /// Copies `count` bytes of DMEM (wrapping at its end), all read before any is written.
    pub(super) fn dmem_move(&mut self, from: usize, to: usize, count: usize) {
        let (from, to) = (from & 0xfff, to & 0xfff);
        if from + count <= 0x1000 && to + count <= 0x1000 {
            self.rsp.memory.copy_within(from..from + count, to);
        } else {
            let before = *self.dmem();
            let dmem = self.dmem_mut();
            for i in 0..count {
                dmem[(to + i) & 0xfff] = before[(from + i) & 0xfff];
            }
        }
    }

    /// Copies `count` bytes, `chunk` at a time, each read before it is written.
    pub(super) fn dmem_copy(&mut self, from: usize, to: usize, count: usize, chunk: usize) {
        for at in (0..count).step_by(chunk) {
            self.dmem_move(from + at, to + at, chunk);
        }
    }
}
