//! S2DEX, the 2D microcode (GBI2, versions 2.05 to 2.08), run as the HLE runs the 3D ones: its commands
//! turned straight into the RDP's, the same words its own code sends. A task may have it as its own microcode
//! (Resident Evil 2, Mario Party) or load it from F3DEX2 for a while (Majora's Mask, Ocarina of Time).
//!
//! S2DEX keeps everything in DMEM: below 0x180 what it shares with F3DEX2 (scissor, other modes, the saved
//! G_RDPHALF_0 and G_RDPHALF_1, segments, the list's stack), kept here in `Gfx`; from 0x180 on its data (tables
//! and defaults) and what its commands leave there (the object matrix at 0x1e0, the status words at 0x2b0, the
//! render mode and the tables it picks), kept here as the microcode has them, in `S2dex::dmem`.
//!
//! The drawing commands (objects.rs, copy.rs, scaled.rs) follow the microcode's own steps: its fixed point
//! runs through the vector unit (`crate::vu`, as the RSP's), whose accumulator, reciprocal unit and lanes carry
//! from one step to the next, so that the RDP's commands come out word for word as the microcode's. Its output
//! gathers in a buffer as the microcode's does, and goes to the RDP where the microcode would hand it over.

use super::{Gfx, Ucode, Work};
use crate::N64;
use crate::vu::Vu;

mod copy;
mod objects;
#[cfg(test)]
mod oracle;
mod scaled;

/// What S2DEX keeps from DMEM 0x180 on, its vector unit, and the G_RDPHALF_0 command it saved (DMEM 0xd0).
#[derive(Clone)]
pub(super) struct S2dex {
    /// DMEM as the microcode has it (4 KB; below 0x180 unused here).
    dmem: Vec<u8>,
    /// The vector unit: v0 zero, v1 ones, the data's constants in v28-v31, and what the commands leave.
    vu: Vu,
    /// The output the microcode gathers for the RDP, and how much of it there is (its s7).
    out: Vec<u8>,
    s7: u32,
    /// Ocarina of Time's build, which follows each group of commands with a pipe sync; a build whose output the
    /// RDP reads from DMEM, which gives it only what it had handed over when the task ends.
    safe: bool,
    xbus: bool,
    /// G_RDPHALF_0's words: a texture rectangle, or G_SELECT_DL's first half.
    rect: [u32; 2],
    /// The words of an RDP command not yet whole: the RDP reads the microcode's output as a stream.
    pending: Vec<u64>,
    /// RDP commands sent so far, for the time the microcode takes.
    sent: u64,
}

impl Default for S2dex {
    fn default() -> S2dex {
        S2dex { dmem: Vec::new(), vu: Vu::new(), out: Vec::new(), s7: 0, safe: false, xbus: false, rect: [0; 2],
            pending: Vec::new(), sent: 0 }
    }
}

// The vector unit's element fields, as listings write them: all lanes, pairs ([0q]), quarters ([0h]) or one
// lane ([0]).
pub(super) const ALL: u32 = 0;
pub(super) const Q0: u32 = 2;
pub(super) const Q1: u32 = 3;
pub(super) const H0: u32 = 4;
pub(super) const H1: u32 = 5;
pub(super) const H2: u32 = 6;
pub(super) const H3: u32 = 7;
pub(super) const E0: u32 = 8;
pub(super) const E1: u32 = 9;
pub(super) const E2: u32 = 10;
pub(super) const E3: u32 = 11;
pub(super) const E4: u32 = 12;
pub(super) const E5: u32 = 13;
pub(super) const E6: u32 = 14;
pub(super) const E7: u32 = 15;

/// The vector unit's computational instructions, each `(vd, vs, vt, element)` as the listing has them (for
/// the single-lane ones, vs's field is vd's lane).
macro_rules! vector_ops {
    ($($name:ident = $funct:expr,)*) => {
        #[allow(dead_code)]
        impl S2dex {
            $(
                pub(super) fn $name(&mut self, vd: u32, vs: u32, vt: u32, e: u32) {
                    self.vu.execute(0x4a00_0000 | e << 21 | vt << 16 | vs << 11 | vd << 6 | $funct);
                }
            )*
        }
    };
}

vector_ops! {
    vmulf = 0x00, vmudl = 0x04, vmudm = 0x05, vmudn = 0x06, vmudh = 0x07, vmacf = 0x08, vmadl = 0x0c,
    vmadm = 0x0d, vmadn = 0x0e, vmadh = 0x0f, vadd = 0x10, vsub = 0x11, vabs = 0x13, vaddc = 0x14, vsubc = 0x15,
    vsar = 0x1d, vlt = 0x20, veq = 0x21, vne = 0x22, vge = 0x23, vcl = 0x24, vch = 0x25, vcr = 0x26, vmrg = 0x27,
    vand = 0x28, vnand = 0x29, vor = 0x2a, vnor = 0x2b, vxor = 0x2c, vnxor = 0x2d, vrcp = 0x30, vrcpl = 0x31,
    vrcph = 0x32, vmov = 0x33,
}

/// S2DEX's builds known, by the name and version in their data.
pub(super) fn recognise(data: &[u8]) -> Option<Ucode> {
    const TITLE: &[u8] = b"RSP Gfx";
    let at = data.windows(TITLE.len()).position(|w| w == TITLE)? + TITLE.len();
    let text = &data[at..(at + 40).min(data.len())];
    // " ucode S2DEX" or "[Safe] S2DEX", then the version: fifo or xbus 2.0x.
    let (safe, rest) = if let Some(rest) = text.strip_prefix(b" ucode S2DEX ") {
        (false, rest)
    } else {
        (true, text.strip_prefix(b"[Safe] S2DEX ")?)
    };
    let rest = rest.trim_ascii_start();
    let xbus = rest.starts_with(b"xbus 2.0");
    (rest.starts_with(b"fifo 2.0") || xbus).then_some(Ucode::S2dex { safe, xbus })
}

impl S2dex {
    fn byte(&self, at: u32) -> u8 {
        self.dmem[at as usize & 0xfff]
    }

    fn half(&self, at: u32) -> u16 {
        u16::from_be_bytes([self.byte(at), self.byte(at + 1)])
    }

    fn word(&self, at: u32) -> u32 {
        u32::from_be_bytes([self.byte(at), self.byte(at + 1), self.byte(at + 2), self.byte(at + 3)])
    }

    fn set_byte(&mut self, at: u32, value: u8) {
        self.dmem[at as usize & 0xfff] = value;
    }

    fn set_half(&mut self, at: u32, value: u16) {
        let [a, b] = value.to_be_bytes();
        self.set_byte(at, a);
        self.set_byte(at + 1, b);
    }

    fn set_word(&mut self, at: u32, value: u32) {
        for (i, b) in value.to_be_bytes().into_iter().enumerate() {
            self.set_byte(at + i as u32, b);
        }
    }

    /// A vector load from DMEM (LBV, LSV, LLV, LDV: `size` bytes into vt's bytes from the element on).
    fn load(&mut self, size: u32, vt: u32, e: u32, at: u32) {
        for i in 0..size {
            if e + i < 16 {
                let byte = self.byte(at + i);
                self.vu.set_byte(vt as usize, (e + i) as usize, byte);
            }
        }
    }

    pub(super) fn lsv(&mut self, vt: u32, e: u32, at: u32) {
        self.load(2, vt, e, at);
    }

    pub(super) fn llv(&mut self, vt: u32, e: u32, at: u32) {
        self.load(4, vt, e, at);
    }

    pub(super) fn ldv(&mut self, vt: u32, e: u32, at: u32) {
        self.load(8, vt, e, at);
    }

    /// LQV: up to the end of the 16-byte line.
    pub(super) fn lqv(&mut self, vt: u32, e: u32, at: u32) {
        self.load(16 - (at & 15), vt, e, at);
    }

    /// A vector store to DMEM (SSV, SLV, SDV: `size` bytes of vt from the element on, round the register).
    fn store(&mut self, size: u32, vt: u32, e: u32, at: u32) {
        for i in 0..size {
            let byte = self.vu.byte(vt as usize, ((e + i) & 15) as usize);
            self.set_byte(at + i, byte);
        }
    }

    pub(super) fn ssv(&mut self, vt: u32, e: u32, at: u32) {
        self.store(2, vt, e, at);
    }

    pub(super) fn slv(&mut self, vt: u32, e: u32, at: u32) {
        self.store(4, vt, e, at);
    }

    pub(super) fn sdv(&mut self, vt: u32, e: u32, at: u32) {
        self.store(8, vt, e, at);
    }

    pub(super) fn sqv(&mut self, vt: u32, e: u32, at: u32) {
        self.store(16 - (at & 15), vt, e, at);
    }

    /// MFC2: a lane (from the byte `e`) sign-extended; MTC2 writes one.
    pub(super) fn mfc2(&self, vs: u32, e: u32) -> u32 {
        let (a, b) = (self.vu.byte(vs as usize, e as usize & 15), self.vu.byte(vs as usize, (e as usize + 1) & 15));
        i16::from_be_bytes([a, b]) as i32 as u32
    }

    pub(super) fn mtc2(&mut self, vs: u32, e: u32, value: u32) {
        let [a, b] = (value as u16).to_be_bytes();
        self.vu.set_byte(vs as usize, e as usize & 15, a);
        if e < 15 {
            self.vu.set_byte(vs as usize, e as usize + 1, b);
        }
    }

    /// CFC2 (sign-extended) and CTC2: VCO (0), VCC (1), VCE (2).
    pub(super) fn cfc2(&self, reg: u32) -> u32 {
        self.vu.control(reg as usize)
    }

    pub(super) fn ctc2(&mut self, reg: u32, value: u32) {
        self.vu.set_control(reg as usize, value);
    }

    /// Stores to the output (the microcode's at s7 + `at`): a word, half, byte, or vector bytes.
    fn out_byte(&mut self, at: u32, value: u8) {
        let at = self.s7.wrapping_add(at) as usize;
        if self.out.len() < at + 1 {
            self.out.resize(at + 64, 0);
        }
        self.out[at] = value;
    }

    pub(super) fn out_sw(&mut self, at: u32, value: u32) {
        for (i, b) in value.to_be_bytes().into_iter().enumerate() {
            self.out_byte(at + i as u32, b);
        }
    }

    pub(super) fn out_sh(&mut self, at: u32, value: u32) {
        let [a, b] = (value as u16).to_be_bytes();
        self.out_byte(at, a);
        self.out_byte(at + 1, b);
    }

    pub(super) fn out_sb(&mut self, at: u32, value: u32) {
        self.out_byte(at, value as u8);
    }

    /// SBV, SSV, SLV, SDV to the output.
    pub(super) fn out_store(&mut self, size: u32, vt: u32, e: u32, at: u32) {
        for i in 0..size {
            let byte = self.vu.byte(vt as usize, ((e + i) & 15) as usize);
            self.out_byte(at + i, byte);
        }
    }

    /// The output's next command (s7 on by 8 bytes, or `n`).
    pub(super) fn advance(&mut self, n: u32) {
        self.s7 += n;
    }

    /// The scissor's edges as G_SETSCISSOR's words have them, 10.2 (DMEM 0x204: left, right, top, bottom).
    fn set_scissor(&mut self, scissor: [u32; 2]) {
        let [w0, w1] = scissor;
        self.set_half(0x204, (w0 >> 12 & 0xfff) as u16);
        self.set_half(0x206, (w1 >> 12 & 0xfff) as u16);
        self.set_half(0x208, (w0 & 0xfff) as u16);
        self.set_half(0x20a, (w1 & 0xfff) as u16);
    }
}

impl N64 {
    /// S2DEX starts: its DMEM from its data in RDRAM (`size` bytes at `data`), all of it at a task's start,
    /// from 0x180 on when another microcode loaded it (what lies below stays, in `gfx`).
    pub(super) fn s2dex_enter(&mut self, gfx: &mut Gfx, ucode: Ucode, data: u32, size: u32, task_start: bool) {
        gfx.ucode = ucode;
        let (safe, xbus) = match ucode {
            Ucode::S2dex { safe, xbus } => (safe, xbus),
            _ => (false, false),
        };
        let vu = std::mem::replace(&mut gfx.s2.vu, Vu::new());
        let mut s2 = S2dex { dmem: vec![0; 0x1000], vu, out: Vec::new(), s7: 0, safe, xbus, rect: [0; 2],
            pending: Vec::new(), sent: 0 };
        let from = if task_start { 0 } else { 0x180 };
        for at in from..size.min(0x1000) {
            s2.dmem[at as usize] = self.rdram[(data + at) as usize & 0x7f_ffff];
        }
        if task_start {
            gfx.scissor = [s2.word(0xc0), s2.word(0xc4)];
            gfx.other = [s2.word(0xc8), s2.word(0xcc)];
            gfx.half[0] = s2.word(0xd8);
            s2.rect = [s2.word(0xd0), s2.word(0xd4)];
        } else {
            // F3DEX2's other modes as kept here, without the command's byte, which S2DEX keeps (and may change).
            gfx.other[0] = 0xef00_0000 | gfx.other[0] & 0x00ff_ffff;
            if let Some(rect) = gfx.texrect {
                s2.rect = [(rect >> 32) as u32, rect as u32];
            }
        }
        s2.set_scissor(gfx.scissor);
        // As its start does: v0 zero, v1 ones, the constants from 0x180.
        s2.vxor(0, 0, 0, ALL);
        for (i, at) in [(31, 0x180), (30, 0x190), (29, 0x1a0), (28, 0x1b0)] {
            s2.lqv(i, 0, at);
        }
        s2.vsub(1, 0, 31, E3);
        gfx.s2 = s2;
    }

    /// One command of S2DEX's display list; false at the end of the task.
    pub(super) fn s2dex_command(&mut self, gfx: &mut Gfx, w0: u32, w1: u32) -> bool {
        let sent = gfx.s2.sent;
        let going = self.s2dex_run(gfx, w0, w1);
        // The microcode's RSP cycles beyond a command's own, by what it sent to the RDP (fitted to its own over
        // random lists, within 3-8% for the objects, 7% for scaled backgrounds and 21% for copied ones).
        let (each, base) = match w0 >> 24 {
            0x09 => (15, 270),
            0x0a => (7, 160),
            0x01 | 0x07 | 0x08 | 0xda => (44, 80),
            0x02 | 0x06 => (70, 80),
            0x05 => (34, 40),
            0xe2..=0xff => (0, 40),
            _ => (0, 0),
        };
        gfx.work.s2dex += base + each * (gfx.s2.sent - sent);
        going
    }

    fn s2dex_run(&mut self, gfx: &mut Gfx, w0: u32, w1: u32) -> bool {
        match w0 >> 24 {
            0x01 => self.obj_rectangle(gfx, w0, w1),
            0x02 => self.obj_sprite(gfx, w1),
            0x04 => self.select_dl(gfx, w0, w1),
            0x05..=0x08 => self.obj_load_txtr(gfx, w0, w1),
            0x09 => self.bg_1cyc(gfx, w1),
            0x0a => self.bg_copy(gfx, w1),
            0x0b => obj_render_mode(&mut gfx.s2, w0, w1),
            0xda => self.obj_rectangle(gfx, w0, w1),
            0xdb => {
                // G_MOVEWORD: w1 to where the table at 0x386 says for the index, plus the offset.
                let at = u32::from(gfx.s2.half(0x386 + (w0 >> 16 & 0xff))).wrapping_add(w0) & 0xfff;
                set_word(gfx, at, w1);
            }
            0xdc => {
                // G_OBJ_MOVEMEM: the object matrix (or another table entry), by DMA.
                let to = u32::from(gfx.s2.half(0x388u32.wrapping_add(w0) & 0xfff));
                let from = gfx.address(w1);
                self.s2dex_dma(gfx, to, from, w0 >> 16 & 0xff);
            }
            0xdd => {
                self.s2dex_end(&mut gfx.s2);
                return self.load_ucode(gfx, w0);
            }
            0xde => self.s2dex_list(gfx, w0, w1),
            0xdf => match gfx.stack.pop() {
                Some(pc) => gfx.pc = pc,
                None => {
                    self.s2dex_end(&mut gfx.s2);
                    #[cfg(test)]
                    oracle::LAST_DMEM.with(|d| *d.borrow_mut() = gfx.s2.dmem.clone());
                    #[cfg(test)]
                    oracle::LAST_VU.with(|d| *d.borrow_mut() = gfx.s2.vu.regs);
                    return false;
                }
            },
            0xe1 => gfx.half[0] = w1,
            // The other modes' fields, w1 or'd in unmasked.
            op @ (0xe2 | 0xe3) => {
                let mask = (0x8000_0000u32 as i32 >> (w0 & 31)) as u32 >> (w0 >> 8 & 31);
                let mode = &mut gfx.other[usize::from(op == 0xe2)];
                *mode = *mode & !mask | w1;
                self.s2dex_other_modes(gfx);
            }
            0xe4 | 0xe5 => gfx.s2.rect = [w0, w1],
            0xed => {
                gfx.scissor = [w0, w1];
                gfx.s2.set_scissor(gfx.scissor);
                self.s2dex_send(gfx, &[u64::from(w0) << 32 | u64::from(w1)]);
            }
            0xef => {
                gfx.other = [w0, w1];
                self.s2dex_other_modes(gfx);
            }
            // G_RDPHALF_2: the scissor again, then the saved texture rectangle with its second half.
            0xf1 => {
                let [r0, r1] = gfx.s2.rect;
                let scissor = u64::from(gfx.scissor[0]) << 32 | u64::from(gfx.scissor[1]);
                let rect = u64::from(r0) << 32 | u64::from(r1);
                self.s2dex_send(gfx, &[scissor, rect, u64::from(gfx.half[0]) << 32 | u64::from(w1)]);
            }
            // Syncs (w1 is what the output buffer held: anything).
            0xe6..=0xe9 => self.s2dex_send(gfx, &[u64::from(w0) << 32]),
            op @ 0xea..=0xff => {
                let w1 = if op >= 0xfd { gfx.address(w1) } else { w1 };
                self.s2dex_send(gfx, &[u64::from(w0) << 32 | u64::from(w1)]);
            }
            // G_NOOP sends an RDP no-op; the rest do nothing.
            _ => {}
        }
        true
    }

    /// RDP commands, then (Ocarina of Time's build) a pipe sync, as the microcode's output does after each
    /// command or group of them.
    fn s2dex_send(&mut self, gfx: &mut Gfx, words: &[u64]) {
        let s2 = &mut gfx.s2;
        for &word in words {
            s2.out_sw(0, (word >> 32) as u32);
            s2.out_sw(4, word as u32);
            s2.advance(8);
        }
        self.s2dex_emit_from(s2);
    }

    /// The microcode's routine that hands what it gathered to the RDP (when there is enough of it, which makes
    /// no difference here): Ocarina of Time's build puts a pipe sync after it first.
    pub(super) fn s2dex_emit(&mut self, gfx: &mut Gfx) {
        self.s2dex_emit_from(&mut gfx.s2);
    }

    pub(super) fn s2dex_emit_from(&mut self, s2: &mut S2dex) {
        if s2.safe {
            s2.out_sw(0, 0xe700_0000);
            s2.out_sw(4, 0);
            s2.advance(8);
        }
        self.s2dex_drain(s2);
    }

    /// What the microcode gathered since it last handed its output over, at its end or before it loads another
    /// microcode: to the RDP (but for the builds whose output stays in DMEM, which leave it).
    fn s2dex_end(&mut self, s2: &mut S2dex) {
        if s2.xbus {
            s2.out.fill(0);
            s2.s7 = 0;
        } else {
            self.s2dex_drain(s2);
        }
    }

    /// What the microcode gathered, to the RDP.
    pub(super) fn s2dex_drain(&mut self, s2: &mut S2dex) {
        if s2.s7 as usize > s2.out.len() {
            s2.out.resize(s2.s7 as usize, 0);
        }
        let words: Vec<u64> =
            s2.out[..s2.s7 as usize].chunks(8).map(|c| u64::from_be_bytes(c.try_into().unwrap())).collect();
        s2.out.fill(0);
        s2.s7 = 0;
        self.s2dex_out(s2, &words);
    }

    /// Words of the microcode's output, which the RDP takes as a stream: each command once it is whole.
    fn s2dex_out(&mut self, s2: &mut S2dex, words: &[u64]) {
        for &word in words {
            s2.pending.push(word);
            if s2.pending.len() == command_length(s2.pending[0]) {
                self.send(&s2.pending);
                s2.pending.clear();
                s2.sent += 1;
            }
        }
    }

    fn s2dex_other_modes(&mut self, gfx: &mut Gfx) {
        let w = u64::from(gfx.other[0]) << 32 | u64::from(gfx.other[1]);
        self.s2dex_send(gfx, &[w]);
    }

    /// G_DL (and G_SELECT_DL's call): bit 16 of w0 makes it a branch.
    fn s2dex_list(&mut self, gfx: &mut Gfx, w0: u32, address: u32) {
        if w0 & 0x1_0000 == 0 {
            if gfx.stack.len() >= 18 {
                return;
            }
            gfx.stack.push(gfx.pc);
        }
        gfx.pc = gfx.address(address);
    }

    /// G_SELECT_DL: the list at the address in G_RDPHALF_0's low half and w0's (high), unless the status word
    /// at G_RDPHALF_0's second byte already holds its flag (G_RDPHALF_0's w1) under the mask (w1); the word
    /// takes the flag under the mask first.
    fn select_dl(&mut self, gfx: &mut Gfx, w0: u32, w1: u32) {
        let s2 = &mut gfx.s2;
        let at = 0x2b0 + (s2.rect[0] >> 16 & 0xff);
        let (status, flag) = (s2.word(at), s2.rect[1]);
        s2.rect[0] = (w0 & 0xffff) << 16 | s2.rect[0] & 0xffff;
        if status & w1 == flag {
            return;
        }
        s2.set_word(at, status & !w1 | flag & w1);
        let address = s2.rect[0];
        self.s2dex_list(gfx, w0, address);
    }

    /// A DMA into DMEM (`length` + 1 bytes, in whole doublewords from aligned addresses), as the RSP's: to S2DEX's
    /// DMEM, or below 0x180 to what it shares.
    fn s2dex_dma(&mut self, gfx: &mut Gfx, to: u32, from: u32, length: u32) {
        let (to, from) = (to & 0xff8, from & 0x00ff_fff8);
        for i in (0..(length & 0xff8) + 8).step_by(4) {
            let at = (from + i) as usize & 0x7f_fffc;
            let value = u32::from_be_bytes(self.rdram[at..at + 4].try_into().unwrap());
            set_word(gfx, (to + i) & 0xfff, value);
        }
    }
}

impl Gfx {
    /// S2DEX's DMEM below 0x180 that its drawing reads (scissor, other modes, G_RDPHALF_0 and _1), from where
    /// they are kept.
    pub(super) fn s2dex_shared(&mut self) {
        let words = [self.scissor[0], self.scissor[1], self.other[0], self.other[1], self.s2.rect[0], self.s2.rect[1],
            self.half[0]];
        for (i, w) in words.into_iter().enumerate() {
            self.s2.set_word(0xc0 + 4 * i as u32, w);
        }
    }
}

impl Work {
    /// S2DEX's RSP cycles: its commands' (fetching and dispatching each), and what each did.
    pub(super) fn s2dex_cycles(&self) -> u64 {
        30 * self.s2dex_commands + self.s2dex
    }
}

/// How many doublewords an RDP command takes, by its first.
fn command_length(word: u64) -> usize {
    match word >> 56 & 0x3f {
        0x08 => 4,
        0x09 => 6,
        0x0a | 0x0c => 12,
        0x0b | 0x0d => 14,
        0x0e => 20,
        0x0f => 22,
        0x24 | 0x25 => 2,
        _ => 1,
    }
}

/// A word of S2DEX's DMEM written (by G_MOVEWORD or a DMA): below 0x180 to what it shares with F3DEX2.
fn set_word(gfx: &mut Gfx, at: u32, value: u32) {
    match at {
        0xc0 | 0xc4 => {
            gfx.scissor[(at as usize - 0xc0) / 4] = value;
            gfx.s2.set_scissor(gfx.scissor);
        }
        0xc8 => gfx.other[0] = value,
        0xcc => gfx.other[1] = value,
        0xd0 | 0xd4 => gfx.s2.rect[(at as usize - 0xd0) / 4] = value,
        0xd8 => gfx.half[0] = value,
        0xf8..=0x134 if at.is_multiple_of(4) => gfx.segments[(at as usize - 0xf8) / 4] = value,
        0x180.. => gfx.s2.set_word(at, value),
        _ => {}
    }
}

/// G_OBJ_RENDERMODE: the mode's byte (0x268), and the tables it picks: of texture coordinates (0x244, by its
/// bilinear bit; 0x246 by w0's), the corrections of the edges (0x1f8, by the shrink and widen bits) and of the
/// texture (0x1fc, by the bilinear and shrink bits).
fn obj_render_mode(s2: &mut S2dex, w0: u32, w1: u32) {
    s2.set_byte(0x268, w1 as u8);
    s2.set_half(0x244, (w1 & 8) as u16 + 0x248);
    s2.set_half(0x246, (w0 & 8) as u16 + 0x258);
    let texture = s2.word(0x234 + ((w1 & 0x18) >> 1));
    let edges = s2.word(0x214 + ((w1 & 0x70) >> 2));
    s2.set_word(0x1fc, texture);
    s2.set_word(0x1f8, edges);
}

#[cfg(test)]
mod tests {
    use super::super::{HALTED, Hle, Task};
    use super::*;

    const F3DZEX: u32 = 0x1000;
    const S2DEX: u32 = 0x2000;
    const LIST: u32 = 0x3000;
    const SUB: u32 = 0x3800;

    fn put32(n64: &mut N64, at: u32, value: u32) {
        n64.rdram[at as usize..at as usize + 4].copy_from_slice(&value.to_be_bytes());
    }

    /// A machine with F3DZEX's and S2DEX's data (their version texts, and F3DZEX's scissor at 0xc0), and an F3DZEX
    /// task whose list is `commands`.
    fn machine(commands: &[(u32, u32)]) -> N64 {
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        let mut n64 = N64::new(&rom).unwrap();
        let mut put = |at: u32, text: &[u8]| n64.rdram[at as usize..at as usize + text.len()].copy_from_slice(text);
        put(F3DZEX + 0x138, b"RSP Gfx ucode F3DZEX.NoN  fifo 2.08I Yoshitaka Yasumoto/Kawasedo 1999.");
        put(S2DEX + 0x138, b"RSP Gfx ucode S2DEX       fifo 2.08  Yoshitaka Yasumoto 1999 Nintendo.");
        put32(&mut n64, F3DZEX + 0xc0, 0xed00_0000);
        put32(&mut n64, F3DZEX + 0xc4, 0x0050_03c0);
        // S2DEX's table of G_MOVEWORD's places: segments at 0xf8.
        put32(&mut n64, S2DEX + 0x38c, 0x00f8_02b0);
        for (offset, value) in [(0x00, 1), (0x04, 0), (0x18, F3DZEX), (0x1c, 0x800), (0x24, 0x400), (0x30, LIST)] {
            n64.rsp.write_memory(0xfc0 + offset, value, !0);
        }
        for (i, &(w0, w1)) in commands.iter().enumerate() {
            put32(&mut n64, LIST + 8 * i as u32, w0);
            put32(&mut n64, LIST + 8 * i as u32 + 4, w1);
        }
        n64
    }

    /// F3DZEX loads S2DEX, which keeps the scissor, other modes and segments and sends G_RDPHALF_2's texture
    /// rectangle after the scissor again, then loads F3DZEX back: the HLE runs it all.
    #[test]
    fn f3dzex_goes_on_through_s2dex_and_back() {
        let list = [
            (0xed00_2008, 0x0040_0300),
            (0xdb06_0018, SUB - 0x40),
            (0xe100_0000, S2DEX),
            (0xdd00_07ff, 0x0010_0000),
            // S2DEX: a texture rectangle, a call through segment 6, then F3DZEX again.
            (0xe400_0000 | 0x100 << 12 | 0x80, 0x0004_0004),
            (0xe100_0000, 0x0000_0000),
            (0xf100_0000, 0x0400_0400),
            (0xde00_0000, 0x0600_0040),
            (0xe100_0000, F3DZEX),
            (0xdd00_07ff, 0x0010_0000),
            (0xfa00_0000, 0x1234_5678),
            (0xdf00_0000, 0),
        ];
        let mut n64 = machine(&list);
        for (i, word) in [0xfb00_0000u32, 0x8040_20ff, 0xdf00_0000, 0].into_iter().enumerate() {
            put32(&mut n64, SUB + 4 * i as u32, word);
        }
        let mut copy = n64.clone();
        copy.hle = Hle { capturing: true, capture_only: true, ..Hle::default() };
        let task =
            Task { flags: 0, dram_stack_size: 0x400, ucode_data: F3DZEX, ucode_data_size: 0x800, data_ptr: LIST };
        let ucode = copy.identify(&task).expect("F3DZEX");
        copy.run_gfx(ucode, &task);
        assert_eq!(copy.hle.captured, vec![
            vec![0xed00_2008_0040_0300],
            vec![0xed00_2008_0040_0300],
            vec![0xe410_0080_0004_0004, 0x0000_0000_0400_0400],
            vec![0xfb00_0000_8040_20ff],
            vec![0xfa00_0000_1234_5678],
        ]);

        n64.set_gfx_hle(true);
        n64.rsp.status = HALTED;
        assert!(n64.gfx_task_started(), "the HLE runs it");
        assert_eq!(n64.hle.report.loading, 0);
    }

    /// A task that loads a microcode this does not know still goes to the RSP.
    #[test]
    fn s2dex_loading_an_unknown_microcode_goes_to_the_rsp() {
        let list = [(0xe100_0000, S2DEX), (0xdd00_07ff, 0x0010_0000), (0xe100_0000, 0x7000), (0xdd00_07ff, 0x0010_0000),
            (0xdf00_0000, 0)];
        let mut n64 = machine(&list);
        n64.set_gfx_hle(true);
        n64.rsp.status = HALTED;
        assert!(!n64.gfx_task_started());
        assert_eq!(n64.hle.report.loading, 1);
    }
}
