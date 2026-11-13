//! The four DMA channels: copies of halfwords or words that run while the
//! CPU waits, at once, at the start of vertical or horizontal blank, or
//! (channels 1 and 2) when a sound FIFO runs low. Channel 0 has the
//! highest priority, and only channel 3 reaches the cartridge.

use crate::Gba;
use crate::bus::set_byte;
use savestate::state;

// When a channel runs (CNT_H bits 12-13).
pub const IMMEDIATE: u16 = 0;
pub const VBLANK: u16 = 1;
pub const HBLANK: u16 = 2;
pub const SPECIAL: u16 = 3;

#[derive(Clone, Copy, Default)]
pub struct Channel {
    /// The registers as written: source, destination, count and control.
    src: u32,
    dst: u32,
    count: u16,
    pub control: u16,
    /// Where it has got to, and the units left; kept between repeats.
    from: u32,
    to: u32,
}

state!(Channel { src, dst, count, control, from, to });

impl Channel {
    fn enabled(&self) -> bool {
        self.control & 0x8000 != 0
    }

    pub fn timing(&self) -> u16 {
        self.control >> 12 & 3
    }
}

/// The address bits each channel's source and destination have.
const SOURCE_MASKS: [u32; 4] = [0x07ff_ffff, 0x0fff_ffff, 0x0fff_ffff, 0x0fff_ffff];
const DEST_MASKS: [u32; 4] = [0x07ff_ffff, 0x07ff_ffff, 0x07ff_ffff, 0x0fff_ffff];

impl Gba {
    pub(crate) fn dma_read_register(&self, reg: u32) -> Option<u16> {
        let (n, offset) = ((reg as usize - 0xb0) / 12, (reg - 0xb0) % 12);
        match offset {
            8 => Some(0),
            10 => Some(self.dma[n].control),
            _ => None,
        }
    }

    pub(crate) fn dma_write_register(&mut self, reg: u32, value: u8) {
        let (n, offset) = ((reg as usize - 0xb0) / 12, (reg - 0xb0) % 12);
        let ch = &mut self.dma[n];
        match offset {
            0..=3 => {
                let shift = offset * 8;
                ch.src = ch.src & !(0xff << shift) | u32::from(value) << shift;
            }
            4..=7 => {
                let shift = (offset - 4) * 8;
                ch.dst = ch.dst & !(0xff << shift) | u32::from(value) << shift;
            }
            8 | 9 => set_byte(&mut ch.count, reg, value),
            _ => {
                let was = ch.enabled();
                set_byte(&mut ch.control, reg, value);
                // Only channel 3 has the cartridge's DMA request (bit 11).
                ch.control &= if n == 3 { 0xffe0 } else { 0xf7e0 };
                if !was && ch.enabled() {
                    ch.from = ch.src & SOURCE_MASKS[n];
                    ch.to = ch.dst & DEST_MASKS[n];
                    if ch.timing() == IMMEDIATE {
                        self.run_dma(n);
                    }
                }
            }
        }
    }

    /// Runs the channels waiting for a moment (blank), in priority order.
    pub(crate) fn trigger_dma(&mut self, timing: u16) {
        for n in 0..4 {
            if self.dma[n].enabled() && self.dma[n].timing() == timing {
                self.run_dma(n);
            }
        }
    }

    /// A sound FIFO wants samples: the channel (1 or 2) set to fill it.
    pub(crate) fn fifo_dma(&mut self, fifo: usize) {
        let addr = 0x0400_00a0 + 4 * fifo as u32;
        for n in 1..3 {
            let ch = self.dma[n];
            if ch.enabled() && ch.timing() == SPECIAL && ch.dst & !3 == addr {
                self.run_dma(n);
            }
        }
    }

    /// Video capture (channel 3, special): a line at a time, lines 2-161.
    pub(crate) fn video_capture_dma(&mut self, line: u16) {
        let ch = self.dma[3];
        if !ch.enabled() || ch.timing() != SPECIAL {
            return;
        }
        if line == 162 {
            self.dma[3].control &= !0x8000;
        } else if (2..162).contains(&line) {
            self.run_dma(3);
        }
    }

    /// Copies a channel's units: the first read and write take N cycles,
    /// the rest S, and two internal cycles more. A sound FIFO takes four
    /// words to the same address.
    fn run_dma(&mut self, n: usize) {
        let ch = self.dma[n];
        let fifo = (n == 1 || n == 2) && ch.timing() == SPECIAL;
        // A sound FIFO's DMA moves nothing the CPU can see.
        self.idle_loop.dma_ran |= !fifo;
        let words = fifo || ch.control & 0x400 != 0;
        let count = match (fifo, ch.count) {
            (true, _) => 4,
            (false, 0) if n == 3 => 0x10000,
            (false, 0) => 0x4000,
            (false, c) => u32::from(c) & if n == 3 { 0xffff } else { 0x3fff },
        };
        let size: u32 = if words { 4 } else { 2 };
        let step = |control: u16| match control & 3 {
            0 | 3 => size,
            1 => size.wrapping_neg(),
            _ => 0,
        };
        let mut src_step = step(ch.control >> 7);
        if ch.from >> 24 >= 0x08 && ch.from >> 24 < 0x0e {
            // The cartridge only counts up.
            src_step = size;
        }
        let dst_step = if fifo { 0 } else { step(ch.control >> 5) };
        if n == 3 && self.cart.eeprom_at(ch.to) {
            self.cart.eeprom_transfer(count);
        }
        let (mut from, mut to) = (ch.from, ch.to);
        self.cycles += 2;
        for i in 0..count {
            let seq = i > 0;
            if words {
                self.cycles += self.waits.word(from, seq) + self.waits.word(to, seq);
                let value = self.dma_load32(from);
                self.store32(to, value);
            } else {
                self.cycles += self.waits.half(from, seq) + self.waits.half(to, seq);
                let value = self.dma_load16(from);
                self.store16(to, value);
            }
            from = from.wrapping_add(src_step);
            to = to.wrapping_add(dst_step);
        }
        let ch = &mut self.dma[n];
        ch.from = from;
        ch.to = to;
        if ch.control & 0x4000 != 0 {
            self.request(crate::DMA_IRQ << n);
        }
        let ch = &mut self.dma[n];
        if ch.control & 0x200 != 0 && ch.timing() != IMMEDIATE {
            // Repeating: again at the next moment, the destination back at
            // the start if asked (control 3).
            if ch.control >> 5 & 3 == 3 {
                ch.to = ch.dst & DEST_MASKS[n];
            }
        } else {
            ch.control &= !0x8000;
        }
    }

    /// What DMA reads: below the board's work RAM (the BIOS) it gets what
    /// it last moved.
    fn dma_load32(&mut self, addr: u32) -> u32 {
        if addr < 0x0200_0000 {
            return self.dma_latch;
        }
        self.dma_latch = self.load32(addr);
        self.dma_latch
    }

    fn dma_load16(&mut self, addr: u32) -> u16 {
        if addr < 0x0200_0000 {
            return (self.dma_latch >> ((addr & 2) * 8)) as u16;
        }
        let value = self.load16(addr);
        self.dma_latch = u32::from(value) * 0x0001_0001;
        value
    }
}
