//! High-level emulation of Resident Evil 2's video microcodes, which make
//! the pictures of its films (the intro, the cutscenes) and of its company
//! logos. They come as graphics tasks (OSTask type 1) with 0x80 bytes of
//! data, their parameters and constants: one turns a frame's YUV 4:2:0
//! planes into 32-bit RGBA in the frame buffer (mupen64plus's rsp-hle has
//! it as decode_video_frame), one averages two 32-bit frame buffers byte by
//! byte (fill_video_double_buffer), and one scales a 24-bit picture 320
//! pixels wide to 16-bit RGBA5551, bilinear, diffusing the error of each
//! group of eight pixels into the next (resize_bilinear).
//!
//! A microcode is told apart by a fingerprint of its code (audio_hle's), and
//! the boot code at IMEM 0 by its own; a task of any other runs on the RSP,
//! as does one the boot code would not start at once (yielded, waiting for
//! the RDP, or asked to yield). What runs here is the microcode instruction
//! for instruction, as straight-line Rust: each vector instruction is the
//! same word given to the RSP's vector unit, its vector loads, stores and
//! DMA go through the RSP's own paths, and its scalar registers are kept
//! alongside; only the waits for DMA and the pipeline's timing are left out.
//! The YUV and blend loops' groups go by the bytes instead, with the vector
//! unit's arithmetic, while VCO was clear at the start and the addresses are
//! aligned (always, in the game), the last group on the vector unit. So
//! DMEM, IMEM, RDRAM, the scalar registers, the vector unit (VCO too, which
//! the YUV microcode reads first) and the DMA registers come out as the
//! microcode leaves them. The RSP then looks busy as long as the
//! microcode takes on the RSP (a cost model of its loops, exact under
//! rsp.rs's timing), and its last SP_STATUS write (the signals) and its
//! BREAK land when that time is up.
//!
//! The comparison mode lets the RSP run each task, in place, and compares
//! what this wrote to RDRAM, DMEM and the SP status with the microcode's
//! when it halts; and runs the microcode alone on one copy of the machine
//! at the task's start and this on another, comparing all of RDRAM, DMEM,
//! IMEM, the RSP's registers, vector unit, DMA registers, semaphore and
//! status, and the RSP's cycles with the cost model.

use crate::N64;
use crate::audio_hle::fingerprint;
use savestate::State;

// SP_STATUS: halted, broke, and signal 0 (libultra's yield request).
const HALTED: u32 = 1 << 0;
const BROKE: u32 = 1 << 1;
const SIGNAL_0: u32 = 1 << 7;

// The scalar registers used, by number.
const AT: usize = 1;
const V0: usize = 2;
const V1: usize = 3;
const A0: usize = 4;
const A1: usize = 5;
const A2: usize = 6;
const A3: usize = 7;
const T0: usize = 8;
const T1: usize = 9;
const T2: usize = 10;
const T3: usize = 11;
const T4: usize = 12;
const T5: usize = 13;
const T6: usize = 14;
const T7: usize = 15;
const S0: usize = 16;
const S1: usize = 17;
const S2: usize = 18;
const S3: usize = 19;
const S4: usize = 20;
const S5: usize = 21;
const S6: usize = 22;
const S7: usize = 23;
const T8: usize = 24;
const T9: usize = 25;
const K0: usize = 26;
const K1: usize = 27;
const GP: usize = 28;
const SP: usize = 29;
const S8: usize = 30;
const RA: usize = 31;

// Vector instructions' functions.
const VMUDL: u32 = 0x04;
const VMUDM: u32 = 0x05;
const VMUDN: u32 = 0x06;
const VMUDH: u32 = 0x07;
const VMADM: u32 = 0x0d;
const VMADN: u32 = 0x0e;
const VADD: u32 = 0x10;
const VSUB: u32 = 0x11;
const VABS: u32 = 0x13;
const VADDC: u32 = 0x14;
const VLT: u32 = 0x20;
const VGE: u32 = 0x23;
const VMRG: u32 = 0x27;
const VAND: u32 = 0x28;
const VOR: u32 = 0x2a;
const VXOR: u32 = 0x2c;

// Vector loads' and stores' kinds (bits 11-15 of LWC2 and SWC2).
const BV: u32 = 0;
const SV: u32 = 1;
const QV: u32 = 4;
const RV: u32 = 5;
const UV: u32 = 7;
const HV: u32 = 8;
const FV: u32 = 9;

/// A vector instruction: vd = vs op vt, vt's lanes picked by the element
/// field (0 all, 8-15 one lane for all).
const fn vu(func: u32, vd: u32, vs: u32, vt: u32, e: u32) -> u32 {
    0x4a00_0000 | e << 21 | vt << 16 | vs << 11 | vd << 6 | func
}

/// LWC2 of a kind: the register, its element and the offset in units of
/// the access (the base register's value goes with it).
const fn lwc2(kind: u32, vt: u32, e: u32, offset: i32) -> u32 {
    0xc800_0000 | vt << 16 | kind << 11 | e << 7 | offset as u32 & 0x7f
}

/// SWC2, as LWC2.
const fn swc2(kind: u32, vt: u32, e: u32, offset: i32) -> u32 {
    0xe800_0000 | vt << 16 | kind << 11 | e << 7 | offset as u32 & 0x7f
}

/// The microcodes known here.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Microcode {
    /// YUV 4:2:0 to 32-bit RGBA, two rows at a time.
    Yuv,
    /// The average of two 32-bit frame buffers.
    Blend,
    /// A 24-bit picture scaled to RGBA5551.
    Resize,
}

/// The microcodes by the fingerprint of their code from IMEM 0x80.
const MICROCODES: [(u64, Microcode); 3] = [
    (0x80a8_baa8_f83f_a7fd, Microcode::Yuv),
    (0x48d2_1167_42ec_eed8, Microcode::Blend),
    (0x356e_18fc_7692_0614, Microcode::Resize),
];

/// libultra's boot microcode (rspboot) at IMEM 0, by its fingerprint: it
/// loads the task's data to DMEM 0 and 0xf80 bytes of its text to IMEM 0x80.
const BOOT: u64 = 0x6c19_f8bf_e025_5f3e;

/// Host preferences and comparison state, not part of the machine's state.
#[derive(Default)]
pub(crate) struct VideoHle {
    pub compare: bool,
    /// The microcode of a task this ran, whose last writes (SP_STATUS's,
    /// the semaphore's) and BREAK (its PC) come when it looks done: a save
    /// state from before then has the RSP at the boot code.
    running: Option<Microcode>,
    /// RDRAM written (each DMA's address and length word), while kept.
    written: Option<Vec<(u32, u32)>>,
    /// While comparing: what this made of the task the RSP is running.
    expected: Option<Box<Expected>>,
    /// Boot codes and texts (IMEM 0-0xd0, then 0x80-0x1000) whose
    /// microcodes were looked up, and what they are.
    known: Vec<(Box<[u8]>, Option<Microcode>)>,
    pub report: VideoReport,
}

/// A copy of the machine keeps the preferences only.
impl Clone for VideoHle {
    fn clone(&self) -> VideoHle {
        VideoHle { compare: self.compare, ..VideoHle::default() }
    }
}

/// What this made of a task: what it wrote to RDRAM, DMEM, and the SP status at the end.
struct Expected {
    microcode: Microcode,
    written: Vec<(usize, Vec<u8>)>,
    dmem: Vec<u8>,
    status: u32,
}

/// What the comparison found, by microcode (YUV, blend, resize).
#[derive(Default, Clone, Debug)]
pub struct VideoReport {
    pub tasks: [u64; 3],
    /// Tasks whose RDRAM written, DMEM and SP status were the microcode's when the RSP halted.
    pub same: [u64; 3],
    /// Tasks whose whole machine was the microcode's, run alone from the task's start.
    pub same_alone: [u64; 3],
    /// The RSP's cycles over the tasks run alone, and the cost model's.
    pub cycles: [u64; 3],
    pub estimated: [u64; 3],
    /// The most the cost model was off on a task, in either direction.
    pub worst: [i64; 3],
    pub notes: Vec<String>,
    /// Graphics tasks of other microcodes: their text's address and fingerprint.
    pub unknown: Vec<(u32, u64)>,
    /// Tasks of a known microcode left to the RSP (yielded, waiting for the RDP, asked to yield).
    pub declined: u64,
}

/// The microcode's last SP_STATUS write: signal 2 (task done) set, or for
/// the blend also signal 1 (yielded) set and signal 0 (yield) cleared.
fn last_status(microcode: Microcode) -> u32 {
    if microcode == Microcode::Blend { 0x5200 } else { 0x4000 }
}

/// Where the microcode's BREAK is, in IMEM.
fn break_at(microcode: Microcode) -> u32 {
    match microcode {
        Microcode::Yuv => 0x380,
        Microcode::Blend => 0x148,
        Microcode::Resize => 0x5b8,
    }
}

impl N64 {
    /// The CPU started the RSP on a graphics task: runs it here if its
    /// microcode is one of these and the graphics HLE is on (true), or sets
    /// up the comparison (false: the RSP runs it).
    pub(crate) fn video_task_started(&mut self) -> bool {
        if !self.hle.enabled && !self.video.compare {
            return false;
        }
        let Some(microcode) = self.video_microcode() else { return false };
        let flags = self.rsp.read_memory(0xfc4);
        // The boot code would go on with a yielded task, wait for the RDP, or yield at once.
        if flags & 3 != 0 || self.rsp.status & SIGNAL_0 != 0 {
            self.video.report.declined += 1;
            return false;
        }
        if !self.video.compare {
            let cycles = self.run_video(microcode);
            self.video.running = Some(microcode);
            self.hle_task_busy(cycles);
            return true;
        }
        let mut ours = self.clone();
        ours.video.written = Some(Vec::new());
        let estimated = ours.run_video(microcode);
        ours.video_task_finished_now(microcode);
        let mut alone = self.clone();
        let clock = alone.rsp.clock;
        for _ in 0..1 << 20 {
            if alone.rsp.halted() {
                break;
            }
            alone.rsp_slice();
        }
        let cycles = alone.rsp.clock - clock;
        let k = microcode as usize;
        let report = &mut self.video.report;
        report.cycles[k] += cycles;
        report.estimated[k] += estimated;
        let off = estimated as i64 - cycles as i64;
        if off.abs() > report.worst[k].abs() {
            report.worst[k] = off;
        }
        if let Some(what) = machine_difference(&alone, &ours) {
            let n = report.tasks.iter().sum::<u64>();
            if report.notes.len() < 8 {
                report.notes.push(format!("task {n} ({microcode:?}) alone: {what}"));
            }
        } else {
            report.same_alone[k] += 1;
        }
        let written = ours.video.written.take().unwrap_or_default().iter()
            .flat_map(|&(dram, length)| rows(dram, length))
            .map(|(at, len)| (at, ours.rdram[at..at + len].to_vec()))
            .collect();
        self.video.expected = Some(Box::new(Expected { microcode, written, dmem: ours.rsp.memory[..0x1000].to_vec(),
            status: ours.rsp.status }));
        false
    }

    /// The RSP halted on a graphics task: if this is comparing it, compares
    /// what the microcode wrote with what this did.
    pub(crate) fn video_task_ran(&mut self) {
        let Some(expected) = self.video.expected.take() else { return };
        let k = expected.microcode as usize;
        let n = self.video.report.tasks.iter().sum::<u64>();
        self.video.report.tasks[k] += 1;
        let differs = expected.written.iter()
            .find(|(at, bytes)| self.rdram[*at..*at + bytes.len()] != bytes[..])
            .map(|(at, bytes)| {
                let i = (0..bytes.len()).find(|&i| self.rdram[at + i] != bytes[i]).unwrap_or(0);
                format!("RDRAM at {:x}: {:02x}, not {:02x}", at + i, bytes[i], self.rdram[at + i])
            })
            .or_else(|| (0..0x1000).find(|&i| self.rsp.memory[i] != expected.dmem[i]).map(|i| {
                format!("DMEM at {i:x}: {:02x}, not {:02x}", expected.dmem[i], self.rsp.memory[i])
            }))
            .or_else(|| (self.rsp.status != expected.status)
                .then(|| format!("SP_STATUS {:x}, not {:x}", expected.status, self.rsp.status)));
        match differs {
            None => self.video.report.same[k] += 1,
            Some(what) if self.video.report.notes.len() < 8 => {
                self.video.report.notes.push(format!("task {n} ({:?}): {what}", expected.microcode));
            }
            Some(_) => {}
        }
    }

    /// A task this ran looks done (the RSP's event, before it halts with the
    /// BREAK): the microcode's last writes, the semaphore's and SP_STATUS's,
    /// and the PC past the BREAK.
    pub(crate) fn video_task_finished(&mut self) {
        let Some(microcode) = self.video.running.take() else { return };
        self.rsp.semaphore = false;
        self.set_sp_register(4, last_status(microcode));
        let at = break_at(microcode);
        self.rsp.pc = at + 4;
        self.rsp.next_pc = at + 8;
    }

    /// The end of a task at once, on a copy: the last writes and the BREAK.
    fn video_task_finished_now(&mut self, microcode: Microcode) {
        self.video.running = Some(microcode);
        self.video_task_finished();
        self.rsp.status |= HALTED | BROKE;
    }

    /// The microcode of the task in DMEM, if one of these after the boot code
    /// this knows: by the code's bytes, as found before, or its fingerprints.
    fn video_microcode(&mut self) -> Option<Microcode> {
        let [boot, text] = self.video_code()?;
        let same = |bytes: &[u8]| bytes[..0xd0] == *boot && bytes[0xd0..] == *text;
        if let Some((_, known)) = self.video.known.iter().find(|(bytes, _)| same(bytes)) {
            return *known;
        }
        let known = self.video_fingerprints();
        if self.video.known.len() == 16 {
            self.video.known.remove(0);
        }
        let bytes = self.video_code()?.concat().into_boxed_slice();
        self.video.known.push((bytes, known));
        known
    }

    /// The boot code in IMEM, and the task's text the boot code would load.
    fn video_code(&self) -> Option<[&[u8]; 2]> {
        let text = (self.rsp.read_memory(0xfd0) & 0x7f_ffff) as usize;
        Some([&self.rsp.memory[0x1000..0x10d0], self.rdram.get(text..text + 0xf80)?])
    }

    fn video_fingerprints(&mut self) -> Option<Microcode> {
        let imem = |pc: u32| {
            let at = pc as usize;
            (0x1000..0x10d0).contains(&at).then(|| u32::from_be_bytes(self.rsp.memory[at..at + 4].try_into().unwrap()))
        };
        let boot = fingerprint(&imem, 0x1000, 0);
        let text = (self.rsp.read_memory(0xfd0) & 0x7f_ffff) as usize;
        let rdram = &self.rdram;
        let word = |pc: u32| {
            let at = text + (pc as usize).checked_sub(0x1080).filter(|&o| o + 4 <= 0xf80)?;
            Some(u32::from_be_bytes(rdram.get(at..at + 4)?.try_into().unwrap()))
        };
        // Another boot code: its fingerprint, at no text.
        let (at, print) = if boot == BOOT { (text as u32, fingerprint(&word, 0x1080, 0)) } else { (u32::MAX, boot) };
        let known = MICROCODES.iter().find(|&&(p, _)| p == print && boot == BOOT).map(|&(_, m)| m);
        let unknown = &mut self.video.report.unknown;
        if known.is_none() && self.video.compare && unknown.len() < 8 && !unknown.contains(&(at, print)) {
            unknown.push((at, print));
        }
        known
    }

    /// Runs a task as the boot code and the microcode would: the cycles the
    /// RSP would take.
    fn run_video(&mut self, microcode: Microcode) -> u64 {
        let mut g = self.rsp.gpr;
        self.video_boot(&mut g);
        let cycles = match microcode {
            Microcode::Yuv => self.yuv(&mut g),
            Microcode::Blend => self.blend(&mut g),
            Microcode::Resize => self.resize(&mut g),
        };
        self.rsp.gpr = g;
        cycles
    }

    /// The boot code's work: the task's data to DMEM 0, 0xf80 bytes of its
    /// text to IMEM 0x80, checking for a yield after each (there is none),
    /// the semaphore freed, and the registers it leaves.
    fn video_boot(&mut self, g: &mut [u32; 32]) {
        g[AT] = 0xfc0;
        g[V0] = self.rsp.read_memory(0xfd8);
        g[V1] = self.rsp.read_memory(0xfdc).wrapping_sub(1);
        g[S8] = 0;
        self.set_sp_register(0, 0);
        self.set_sp_register(1, g[V0]);
        self.set_sp_register(2, g[V1]);
        g[A0] = 0;
        g[T0] = self.rsp.status & SIGNAL_0;
        g[V0] = self.rsp.read_memory(0xfd0);
        g[V1] = 0xf7f;
        g[A3] = 0x1080;
        self.set_sp_register(0, g[A3]);
        self.set_sp_register(1, g[V0]);
        self.set_sp_register(2, g[V1]);
        g[RA] = 0x034;
        self.rsp.semaphore = false;
    }

    /// The microcode's DMA routine: from DMEM (or IMEM) t2 and RDRAM t1, as
    /// the length word t0 has it, waiting for the DMA (none) and freeing the
    /// semaphore.
    fn video_dma(&mut self, g: &mut [u32; 32], to_rdram: bool) {
        g[T3] = 0;
        self.set_sp_register(0, g[T2]);
        self.set_sp_register(1, g[T1]);
        if to_rdram && let Some(written) = &mut self.video.written {
            written.push((self.rsp.dram_addr, g[T0]));
        }
        self.set_sp_register(if to_rdram { 3 } else { 2 }, g[T0]);
        self.rsp.semaphore = false;
    }

    #[inline(always)]
    fn vop(&mut self, word: u32) {
        self.rsp.vu.execute(word);
    }

    #[inline(always)]
    fn vload(&mut self, word: u32, base: u32) {
        self.rsp.load_vector(word, base);
    }

    #[inline(always)]
    fn vstore(&mut self, word: u32, base: u32) {
        self.rsp.store_vector(word, base);
    }

    /// LW and LH from DMEM (LH sign-extends).
    fn dmem_word(&self, at: u32) -> u32 {
        let byte = |i: u32| u32::from(self.rsp.memory[(at.wrapping_add(i) & 0xfff) as usize]);
        byte(0) << 24 | byte(1) << 16 | byte(2) << 8 | byte(3)
    }

    fn dmem_half(&self, at: u32) -> u32 {
        let byte = |i: u32| self.rsp.memory[(at.wrapping_add(i) & 0xfff) as usize];
        i16::from_be_bytes([byte(0), byte(1)]) as i32 as u32
    }

    /// SH and SW to DMEM.
    fn set_dmem_bytes(&mut self, at: u32, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            self.rsp.memory[(at.wrapping_add(i as u32) & 0xfff) as usize] = b;
        }
    }

    /// YUV 4:2:0 to RGBA: for each pass of `gp`, two rows of Y (t9 wide) and
    /// a row of each of Cb and Cr come in to DMEM 0, 0x200 and 0x280, and
    /// `k1` times sixteen pixels of two rows at a time become RGBA at 0x300
    /// on (four bytes a pixel, the fourth left as it was: zero, or on the
    /// first row what Cr's DMA left there), of which the DMA word `sp`
    /// takes the first rows to t8 (stepping by s8). With k1 at 2 the second
    /// time round works on what lies after the rows in DMEM and writes past
    /// DMEM's end, which wraps.
    fn yuv(&mut self, g: &mut [u32; 32]) -> u64 {
        for (i, r) in [S5, S6, S7, T8, T9, K0, K1, GP, SP, S8].into_iter().enumerate() {
            g[r] = self.dmem_word(4 * i as u32);
        }
        // v0: zero, less VCO's carries; v1 and v2: the constants.
        self.vop(vu(VSUB, 0, 0, 0, 0));
        self.vload(lwc2(QV, 1, 0, 4), 0);
        self.vload(lwc2(QV, 2, 0, 5), 0);
        // Takes the semaphore, which the boot code left free.
        g[T3] = 0;
        self.rsp.semaphore = true;
        g[S4] = g[T9] << 2;
        g[T0] = 0x300;
        g[T1] = 0xac0;
        loop {
            for i in 0..4 {
                self.vstore(swc2(QV, 0, 0, i), g[T0]);
            }
            let again = g[T0] != g[T1];
            g[T0] = g[T0].wrapping_add(64);
            if !again {
                break;
            }
        }
        let (mut passes, mut rows, mut groups) = (0, 0, 0);
        loop {
            g[S3] = g[K1];
            g[T4] = 0;
            g[S0] = 0x300;
            g[T6] = 0x200;
            g[T7] = 0x280;
            g[T5] = g[T4].wrapping_add(g[T9]);
            g[S1] = g[S0].wrapping_add(g[S4]);
            g[T2] = 0;
            g[T1] = g[S5];
            g[T0] = g[T9] << 1;
            g[RA] = 0x110;
            g[T0] = g[T0].wrapping_sub(1);
            self.video_dma(g, false);
            g[T2] = 0x200;
            g[T1] = g[S6];
            g[RA] = 0x120;
            g[T0] = g[T9].wrapping_sub(1);
            self.video_dma(g, false);
            g[T2] = 0x280;
            g[RA] = 0x12c;
            g[T1] = g[S7];
            self.video_dma(g, false);
            loop {
                g[S2] = g[T9];
                loop {
                    let last = (g[GP] as i32) <= 1 && (g[S3] as i32) <= 1 && (g[S2] as i32) <= 16;
                    if last || !self.yuv_bytes(g) {
                        self.yuv_group(g);
                    }
                    groups += 1;
                    let again = (g[S2] as i32) > 0;
                    g[S1] = g[S1].wrapping_add(64);
                    if !again {
                        break;
                    }
                }
                g[S3] = g[S3].wrapping_sub(1);
                g[T4] = g[T4].wrapping_add(g[T9]);
                g[T5] = g[T5].wrapping_add(g[T9]);
                g[S0] = g[S0].wrapping_add(g[S4]);
                rows += 1;
                let again = (g[S3] as i32) > 0;
                g[S1] = g[S1].wrapping_add(g[S4]);
                if !again {
                    break;
                }
            }
            g[T1] = g[T8];
            g[T2] = 0x300;
            g[RA] = 0x354;
            g[T0] = g[SP];
            self.video_dma(g, true);
            g[T8] = g[T8].wrapping_add(g[S8]);
            g[T0] = g[T9] << 1;
            g[S5] = g[S5].wrapping_add(g[T0]);
            g[T0] >>= 2;
            g[S7] = g[S7].wrapping_add(g[T0]);
            g[GP] = g[GP].wrapping_sub(1);
            passes += 1;
            let again = (g[GP] as i32) > 0;
            g[S6] = g[S6].wrapping_add(g[T0]);
            if !again {
                break;
            }
        }
        g[AT] = 0x4000;
        YUV_COST[0] + YUV_COST[1] * passes + YUV_COST[2] * rows + YUV_COST[3] * groups
    }

    /// yuv_group by the bytes, as the vector unit has it, while v0 is zero
    /// (VCO was clear) and the group's addresses are aligned as they are in
    /// the game (lines whole, no wrapping within one), with the first row's
    /// pixels clear of 0xf80-0xfff, which the second's are read back from
    /// after them; false (and nothing done) otherwise. The vector registers
    /// are left as they were: the task's last group runs on the vector unit.
    fn yuv_bytes(&mut self, g: &mut [u32; 32]) -> bool {
        let v = &self.rsp.vu.regs;
        let aligned = (g[T4] | g[T5] | g[T6] | g[T7]) & 7 == 0 && (g[S0] | g[S1]) & 15 == 0;
        if v[0] != [0; 8] || !aligned || g[S0] & 0xfff > 0xf80 - 64 {
            return false;
        }
        let (k, m, limit) = (v[1], v[2][0] as i16, v[2][3] as i16);
        let clamp = |x: i32| x.clamp(-32768, 32767);
        // VMUDM: signed times unsigned, the middle of the product.
        let mudm = |a: i32, k: u16| (a * i32::from(k)) >> 16;
        let fraction = |byte: u8| i32::from(byte) << 7;
        let mem = &mut self.rsp.memory;
        let at = |a: u32| a as usize & 0xfff;
        // R, G and B of the two rows (by the clamps to 0 and v2[3]).
        let mut out = [[0u8; 16]; 6];
        for c in 0..8 {
            let cr = clamp(fraction(mem[at(g[T7] + c)]) - i32::from(m));
            let cb = clamp(fraction(mem[at(g[T6] + c)]) - i32::from(m));
            let (r, g1, g2, b) = (mudm(cr, k[0]), mudm(cr, k[1]), mudm(cb, k[2]), mudm(cb, k[3]));
            for (row, base) in [(0, g[T4]), (3, g[T5])] {
                for p in [2 * c, 2 * c + 1] {
                    let y = mudm(fraction(mem[at(base + p)]), k[4]);
                    let colours = [clamp(y + r), clamp(clamp(y - g1) - g2), clamp(y + b)];
                    for (i, colour) in colours.into_iter().enumerate() {
                        let x = colour.max(0).min(i32::from(limit));
                        out[row + i][p as usize] = (x as u16 >> 7) as u8;
                    }
                }
            }
        }
        for (i, line) in out.iter().enumerate() {
            let scratch = 0xf80 + 16 * (i + i / 3);
            mem[scratch..scratch + 16].copy_from_slice(line);
        }
        for (row, base) in [(0, g[S0]), (3, g[S1])] {
            if row == 3 {
                for i in 0..64 {
                    mem[at(base + i)] = 0;
                }
            }
            for p in 0..16 {
                for i in 0..3 {
                    mem[at(base + 4 * p + i as u32)] = out[row + i][p as usize];
                }
            }
        }
        g[T7] = g[T7].wrapping_add(8);
        g[T6] = g[T6].wrapping_add(8);
        g[T4] = g[T4].wrapping_add(16);
        g[T5] = g[T5].wrapping_add(16);
        g[S2] = g[S2].wrapping_sub(16);
        g[T3] = 0xf80;
        g[T1] = g[S1].wrapping_add(1);
        g[T2] = g[S1].wrapping_add(2);
        g[S0] = g[S0].wrapping_add(64);
        true
    }

    /// Sixteen pixels of two rows: Y of the even pixels then of the odd
    /// ones, each turned to R, G and B (clamped to 0-255) with the group's
    /// eight Cb and Cr, parked at 0xf80 and 0xf81 a byte apart, read back as
    /// bytes and stored a pixel (four bytes) apart.
    fn yuv_group(&mut self, g: &mut [u32; 32]) {
        self.vload(lwc2(UV, 3, 0, 0), g[T7]);
        g[T7] = g[T7].wrapping_add(8);
        self.vload(lwc2(UV, 5, 0, 0), g[T6]);
        g[T6] = g[T6].wrapping_add(8);
        self.vop(vu(VSUB, 3, 3, 2, 8));
        self.vop(vu(VSUB, 5, 5, 2, 8));
        self.vop(vu(VADD, 4, 0, 3, 0));
        self.vop(vu(VADD, 6, 0, 5, 0));
        self.vload(lwc2(HV, 7, 0, 0), g[T4]);
        self.vload(lwc2(HV, 8, 0, 0), g[T5]);
        self.vop(vu(VMUDM, 3, 3, 1, 8));
        self.vop(vu(VMUDM, 4, 4, 1, 9));
        self.vop(vu(VMUDM, 5, 5, 1, 10));
        self.vop(vu(VMUDM, 6, 6, 1, 11));
        self.vop(vu(VMUDM, 7, 7, 1, 12));
        self.vop(vu(VMUDM, 8, 8, 1, 12));
        g[T4] = g[T4].wrapping_add(1);
        g[T5] = g[T5].wrapping_add(1);
        g[S2] = g[S2].wrapping_sub(16);
        self.yuv_colours();
        self.vload(lwc2(HV, 7, 0, 0), g[T4]);
        self.vload(lwc2(HV, 8, 0, 0), g[T5]);
        g[T3] = 0xf80;
        self.yuv_park(g[T3]);
        self.vop(vu(VMUDM, 7, 7, 1, 12));
        self.vop(vu(VMUDM, 8, 8, 1, 12));
        g[T3] = g[T3].wrapping_add(1);
        g[T4] = g[T4].wrapping_add(15);
        g[T5] = g[T5].wrapping_add(15);
        self.yuv_colours();
        self.yuv_park(g[T3]);
        g[T3] = 0xf80;
        for (i, r) in (9..15).enumerate() {
            self.vload(lwc2(UV, r, 0, i as i32), g[T3]);
        }
        g[T1] = g[S0].wrapping_add(1);
        g[T2] = g[S0].wrapping_add(2);
        self.yuv_store(g[S0], g[T1], g[T2]);
        for (i, r) in (9..15).enumerate() {
            self.vload(lwc2(UV, r, 0, 8 + i as i32), g[T3]);
        }
        g[T1] = g[S1].wrapping_add(1);
        g[T2] = g[S1].wrapping_add(2);
        g[S0] = g[S0].wrapping_add(64);
        for i in 0..4 {
            self.vstore(swc2(QV, 0, 0, i), g[S1]);
        }
        self.yuv_store(g[S1], g[T1], g[T2]);
    }

    /// v9-v14: the two rows' R, G and B from Y (v7, v8) and the chroma terms
    /// (v3-v6), each clamped to v0 and below v2[3].
    fn yuv_colours(&mut self) {
        self.vop(vu(VADD, 9, 7, 3, 0));
        self.vop(vu(VSUB, 11, 7, 4, 0));
        self.vop(vu(VSUB, 12, 8, 4, 0));
        self.vop(vu(VADD, 13, 7, 6, 0));
        self.vop(vu(VADD, 10, 8, 3, 0));
        self.vop(vu(VADD, 14, 8, 6, 0));
        self.vop(vu(VSUB, 11, 11, 5, 0));
        self.vop(vu(VSUB, 12, 12, 5, 0));
        for r in 9..15 {
            self.vop(vu(VGE, r, r, 0, 0));
            self.vop(vu(VLT, r, r, 2, 11));
        }
    }

    fn yuv_park(&mut self, at: u32) {
        for (r, offset) in [(9, 0), (10, 4), (11, 1), (12, 5), (13, 2), (14, 6)] {
            self.vstore(swc2(HV, r, 0, offset), at);
        }
    }

    fn yuv_store(&mut self, s: u32, t1: u32, t2: u32) {
        for (r, base, e, offset) in [(9, s, 0, 0), (11, t1, 0, 0), (13, t2, 0, 0), (10, s, 0, 2), (12, t1, 0, 2),
            (14, t2, 0, 2), (9, s, 8, 1), (11, t1, 8, 1), (13, t2, 8, 1), (10, s, 8, 3), (12, t1, 8, 3), (14, t2, 8, 3)]
        {
            self.vstore(swc2(FV, r, e, offset), base);
        }
    }

    /// The average of two frame buffers: `s0` times, t7 rows (s2's DMA
    /// word, its rows field from t7) from t4 come in to DMEM 0 and from t5
    /// to 0x800; each of t6 bytes becomes (a + b) >> 1 through the
    /// accumulator (with v3 gathering carries, which there are none of with
    /// VCO clear at the start), and goes back to t5; both step by s3.
    fn blend(&mut self, g: &mut [u32; 32]) -> u64 {
        for (i, r) in [T4, T5, T6, T7, S0, S1, S2, S3].into_iter().enumerate() {
            g[r] = self.dmem_word(4 * i as u32);
        }
        self.vload(lwc2(QV, 10, 0, 2), 0);
        self.vop(vu(VSUB, 0, 0, 0, 0));
        self.vop(vu(VSUB, 3, 3, 3, 0));
        g[S7] = g[T7].wrapping_sub(1) << 12;
        // With v0 zero (VCO was clear) and the constants 1 and 0x8000, a byte
        // is (a + b) >> 1: the sum, shifted by 7, never carries, so v3 stays
        // zero, and the +1 goes in the halving.
        let v = &self.rsp.vu.regs;
        let bytes = v[0] == [0; 8] && v[10][3] == 1 && v[10][4] == 0x8000;
        let (mut passes, mut groups) = (0, 0);
        loop {
            g[T0] = g[S2] | g[S7];
            g[T2] = 0;
            g[RA] = 0x0d0;
            g[T1] = g[T4];
            if (g[S7] as i32) >= 0 {
                self.video_dma(g, false);
            }
            g[T2] = 0x800;
            g[RA] = 0x0dc;
            g[T1] = g[T5];
            if (g[T0] as i32) >= 0 {
                self.video_dma(g, false);
            }
            g[S5] = 0;
            g[S6] = 0x800;
            g[S4] = g[T6];
            if bytes && (g[S4] as i32) > 8 {
                // All but the pass's last group by the bytes; that one leaves the registers.
                let n = (g[S4] - 1) / 8 * 8;
                let dmem = &mut self.rsp.memory;
                for i in 0..n {
                    let (a, b) = ((g[S5] + i) as usize & 0xfff, (g[S6] + i) as usize & 0xfff);
                    dmem[b] = ((u16::from(dmem[a]) + u16::from(dmem[b])) >> 1) as u8;
                }
                groups += u64::from(n / 8);
                g[S5] = g[S5].wrapping_add(n);
                g[S6] = g[S6].wrapping_add(n);
                g[S4] = g[S4].wrapping_sub(n);
            }
            loop {
                self.vload(lwc2(UV, 1, 0, 0), g[S5]);
                self.vload(lwc2(UV, 2, 0, 0), g[S6]);
                self.vop(vu(VADDC, 1, 1, 2, 0));
                self.vop(vu(VADD, 3, 3, 0, 0));
                self.vop(vu(VADDC, 1, 1, 10, 11));
                self.vop(vu(VADD, 3, 3, 0, 0));
                self.vop(vu(VMUDL, 1, 1, 10, 12));
                self.vop(vu(VMADM, 3, 3, 10, 12));
                self.vop(vu(VMADN, 1, 0, 0, 8));
                self.vstore(swc2(UV, 1, 0, 0), g[S6]);
                groups += 1;
                g[S5] = g[S5].wrapping_add(8);
                g[S4] = g[S4].wrapping_sub(8);
                let again = (g[S4] as i32) > 0;
                g[S6] = g[S6].wrapping_add(8);
                if !again {
                    break;
                }
            }
            g[T2] = 0x800;
            g[RA] = 0x12c;
            g[T1] = g[T5];
            if (g[T0] as i32) >= 0 {
                self.video_dma(g, true);
            }
            g[T4] = g[T4].wrapping_add(g[S3]);
            g[S0] = g[S0].wrapping_sub(1);
            passes += 1;
            let again = (g[S0] as i32) > 0;
            g[T5] = g[T5].wrapping_add(g[S3]);
            if !again {
                break;
            }
        }
        g[T3] = 0x5200;
        BLEND_COST[0] + BLEND_COST[1] * passes + BLEND_COST[2] * groups
    }

    /// The scaling: s6 rows of s5 pixels from 24-bit rows 320 wide at s7
    /// (from row gp >> 16, gp stepping by k0) to 16 bits at s3 (stepping by
    /// s2), x from sp stepping by k1: for each row two rows of the source
    /// come in to DMEM 0, and eight pixels at a time are weighed from their
    /// four neighbours (v27 and v28 the eight x's fractions and whole parts,
    /// v1 and v25 the row's weights), the error of each lane carried to the
    /// next through v22-v24 and DMEM 0xfe0, and packed to RGBA5551 at 0x780.
    fn resize(&mut self, g: &mut [u32; 32]) -> u64 {
        for (i, r) in [S7, S3, S5, S6, K1, K0, S2, T9, SP, GP].into_iter().enumerate() {
            g[r] = self.dmem_word(4 * i as u32);
        }
        self.vload(lwc2(QV, 29, 0, 4), 0);
        self.vload(lwc2(QV, 30, 0, 5), 0);
        self.vload(lwc2(QV, 31, 0, 6), 0);
        self.vop(vu(VXOR, 0, 0, 0, 0));
        self.vop(vu(VXOR, 22, 22, 22, 0));
        self.vop(vu(VXOR, 23, 23, 23, 0));
        g[S8] = 0xf80;
        self.vop(vu(VXOR, 24, 24, 24, 0));
        self.vstore(swc2(QV, 0, 0, -8), 0);
        self.set_dmem_bytes(0xffc, &[0; 4]);
        self.vstore(swc2(QV, 0, 0, -7), 0);
        self.set_dmem_bytes(0xfb0, &[0; 4]);
        self.vstore(swc2(QV, 0, 0, -6), 0);
        // The eight x's, their low halves at 0xfc0 and high at 0xfd0.
        const XS: [usize; 8] = [AT, V0, V1, A0, A1, A2, A3, T0];
        g[AT] = g[SP];
        for i in 1..8 {
            g[XS[i]] = g[XS[i - 1]].wrapping_add(g[K1]);
        }
        for (i, &r) in XS.iter().enumerate() {
            self.set_dmem_bytes(0xfc0 + 2 * i as u32, &(g[r] as u16).to_be_bytes());
        }
        for r in XS {
            g[r] >>= 16;
        }
        for (i, &r) in XS.iter().enumerate() {
            self.set_dmem_bytes(0xfd0 + 2 * i as u32, &(g[r] as u16).to_be_bytes());
        }
        let (mut rows, mut groups) = (0, 0);
        loop {
            self.vload(lwc2(QV, 27, 0, 4), g[S8]);
            g[T3] = g[GP] >> 16;
            self.vload(lwc2(QV, 28, 0, 5), g[S8]);
            g[S0] = g[T3] << 10;
            g[T3] <<= 6;
            g[S0] = g[S0].wrapping_sub(g[T3]).wrapping_add(g[S7]);
            g[AT] = g[GP] >> 1;
            self.set_dmem_bytes(g[S8], &(g[AT] as u16).to_be_bytes());
            g[T2] = 0;
            g[T1] = g[S0];
            self.vload(lwc2(SV, 1, 0, 0), g[S8]);
            g[RA] = 0x194;
            g[T0] = 0x77f;
            self.video_dma(g, false);
            self.vop(vu(VADD, 1, 0, 1, 8));
            self.vop(vu(VXOR, 25, 1, 29, 8));
            self.vop(vu(VAND, 1, 1, 29, 9));
            g[S4] = 0x780;
            g[T8] = g[S5];
            self.vop(vu(VAND, 25, 25, 29, 9));
            loop {
                self.resize_group(g);
                groups += 1;
                let again = (g[T8] as i32) > 0;
                g[S4] = g[S4].wrapping_add(16);
                if !again {
                    break;
                }
            }
            g[T1] = g[S3];
            g[T2] = 0x780;
            g[T0] = g[S5] << 1;
            g[RA] = 0x59c;
            g[T0] = g[T0].wrapping_sub(1);
            self.video_dma(g, true);
            g[S3] = g[S3].wrapping_add(g[S2]);
            g[S6] = g[S6].wrapping_sub(1);
            rows += 1;
            let again = (g[S6] as i32) > 0;
            g[GP] = g[GP].wrapping_add(g[K0]);
            if !again {
                break;
            }
        }
        g[AT] = 0x4000;
        RESIZE_COST[0] + RESIZE_COST[1] * rows + RESIZE_COST[2] * groups
    }

    /// Eight pixels of a row of the scaling.
    fn resize_group(&mut self, g: &mut [u32; 32]) {
        self.vop(vu(VMUDL, 26, 27, 30, 10));
        for r in [21, 20, 19, 18, 10, 11, 12, 13, 17, 16, 15, 14] {
            self.vop(vu(VXOR, r, r, r, 0));
        }
        self.vop(vu(VXOR, 5, 26, 29, 8));
        self.vop(vu(VAND, 26, 26, 29, 9));
        self.vop(vu(VAND, 5, 5, 29, 9));
        self.vop(vu(VMUDM, 7, 26, 25, 0));
        self.vop(vu(VMUDM, 3, 5, 25, 0));
        self.vop(vu(VMUDM, 8, 26, 1, 0));
        self.vop(vu(VMUDM, 9, 5, 1, 0));
        self.vop(vu(VMUDH, 4, 28, 29, 10));
        self.vstore(swc2(QV, 4, 0, 0), g[S8]);
        self.vop(vu(VADD, 4, 4, 31, 8));
        // The four neighbours' bytes, by the offsets parked at 0xf80, to the
        // low bytes of the lanes of v10/v14/v18 (x, this row), v13/v17/v21
        // (x, next row), v11/v15/v19 (x + 1, this row), v12/v16/v20 (x + 1,
        // next row).
        self.resize_pixels(g, T4, 0..8, [10, 14, 18]);
        self.vstore(swc2(QV, 4, 0, 0), g[S8]);
        self.vop(vu(VSUB, 4, 4, 31, 8));
        self.resize_pixels(g, T7, 0..4, [13, 17, 21]);
        self.vop(vu(VADD, 4, 4, 29, 10));
        self.resize_pixels(g, T7, 4..8, [13, 17, 21]);
        self.vstore(swc2(QV, 4, 0, 0), g[S8]);
        self.vop(vu(VADD, 4, 4, 31, 8));
        self.resize_pixels(g, T5, 0..8, [11, 15, 19]);
        self.vstore(swc2(QV, 4, 0, 0), g[S8]);
        self.resize_pixels(g, T6, 0..8, [12, 16, 20]);
        for r in [7, 3, 8, 9] {
            self.vop(vu(VMUDN, r, r, 29, 11));
        }
        for (d, t) in [(10, 3), (11, 7), (12, 8), (13, 9), (14, 3), (15, 7), (16, 8), (17, 9), (18, 3), (19, 7),
            (20, 8), (21, 9)]
        {
            self.vop(vu(VMUDM, d, d, t, 0));
        }
        for (d, t) in [(10, 11), (14, 15), (18, 19), (10, 12), (14, 16), (18, 20), (10, 13), (14, 17), (18, 21)] {
            self.vop(vu(VADDC, d, d, t, 0));
        }
        g[T8] = g[T8].wrapping_sub(8);
        self.vop(vu(VADD, 22, 22, 10, 0));
        g[AT] = self.dmem_half(0xfe0);
        self.vop(vu(VADD, 23, 23, 14, 0));
        g[V0] = self.dmem_half(0xfe2);
        self.vop(vu(VADD, 24, 24, 18, 0));
        g[V1] = self.dmem_half(0xfe4);
        self.vstore(swc2(SV, 22, 14, -16), 0);
        self.vstore(swc2(SV, 23, 14, -15), 0);
        self.vstore(swc2(SV, 24, 14, -14), 0);
        g[T3] = 2;
        // Each sum a lane on, the last lane before kept for the next group.
        for (i, (r, previous)) in [(22, AT), (23, V0), (24, V1)].into_iter().enumerate() {
            self.vstore(swc2(QV, r, 0, -8 + i as i32), g[T3]);
            self.vstore(swc2(RV, r, 0, -8 + i as i32), g[T3]);
            self.set_dmem_bytes(0xf80 + 16 * i as u32, &(g[previous] as u16).to_be_bytes());
        }
        for (i, r) in (22..25).enumerate() {
            self.vload(lwc2(QV, r, 0, -8 + i as i32), 0);
        }
        self.vop(vu(VSUB, 4, 10, 22, 0));
        self.vop(vu(VSUB, 5, 14, 23, 0));
        self.vop(vu(VSUB, 6, 18, 24, 0));
        self.vop(vu(VABS, 4, 4, 4, 0));
        self.vop(vu(VABS, 5, 5, 5, 0));
        self.vop(vu(VABS, 6, 6, 6, 0));
        self.vop(vu(VLT, 11, 4, 31, 9));
        g[AT] = self.rsp.vu.control(1);
        self.vop(vu(VLT, 15, 5, 31, 9));
        g[V0] = self.rsp.vu.control(1);
        self.vop(vu(VLT, 19, 6, 31, 9));
        g[V1] = self.rsp.vu.control(1);
        g[AT] = g[AT] & g[V0] & g[V1];
        self.rsp.vu.set_control(1, g[AT]);
        for r in 22..25 {
            self.vop(vu(VMRG, r, r, 29, 11));
        }
        for r in 22..25 {
            self.vop(vu(VAND, r, r, 30, 9));
        }
        for (d, t) in [(10, 22), (14, 23), (18, 24)] {
            self.vop(vu(VADD, d, d, t, 0));
        }
        for r in [10, 14, 18] {
            self.vop(vu(VLT, r, r, 30, 11));
        }
        for r in [10, 14, 18] {
            self.vop(vu(VAND, r, r, 29, 13));
        }
        self.vop(vu(VMUDH, 18, 18, 30, 8));
        self.vop(vu(VMUDH, 14, 14, 29, 15));
        self.vop(vu(VMUDL, 10, 10, 29, 14));
        self.vop(vu(VADDC, 27, 27, 30, 13));
        self.vop(vu(VADD, 28, 28, 30, 12));
        self.vop(vu(VADDC, 18, 18, 18, 0));
        self.vop(vu(VOR, 10, 10, 14, 0));
        self.vop(vu(VOR, 10, 10, 18, 0));
        self.vop(vu(VOR, 10, 10, 29, 12));
        self.vstore(swc2(QV, 10, 0, 0), g[S4]);
    }

    /// LH of the offsets parked at 0xf80 (`lanes`), each to `base`, and LBV
    /// of the pixel's three bytes there to the low byte of that lane of
    /// each of `regs`.
    fn resize_pixels(&mut self, g: &mut [u32; 32], base: usize, lanes: std::ops::Range<u32>, regs: [u32; 3]) {
        for lane in lanes {
            g[base] = self.dmem_half(g[S8] + 2 * lane);
            for (i, &r) in regs.iter().enumerate() {
                self.vload(lwc2(BV, r, 2 * lane + 1, i as i32), g[base]);
            }
        }
    }
}

/// The RSP's cycles (as rsp.rs times the microcode, from the boot code to
/// the BREAK, with DMA at once): the YUV microcode's to start, a pass, a
/// pair of rows and a group of sixteen pixels; measured with the parameters
/// varied, and exact for all of the intro's tasks.
const YUV_COST: [u64; 4] = [322, 112, 8, 116];
/// The blend's: to start, a pass, a group of eight bytes.
const BLEND_COST: [u64; 3] = [91, 96, 13];
/// The scaling's: to start, a row, a group of eight pixels.
const RESIZE_COST: [u64; 3] = [130, 68, 297];

/// The RDRAM a DMA to it wrote: each row's address and length.
fn rows(dram: u32, length: u32) -> impl Iterator<Item = (usize, usize)> {
    let len = (length & 0xff8) + 8;
    let count = (length >> 12 & 0xff) + 1;
    let skip = length >> 20 & 0xff8;
    (0..count).map(move |r| ((dram + r * (len + skip)) as usize & 0xff_fff8, len as usize))
        .filter(|&(at, len)| at + len <= crate::bus::RDRAM_SIZE)
}

/// How two machines differ in what an RSP task can change, if they do.
fn machine_difference(a: &N64, b: &N64) -> Option<String> {
    if let Some(i) = (0..a.rdram.len()).find(|&i| a.rdram[i] != b.rdram[i]) {
        return Some(format!("RDRAM at {i:x}: {:02x}, not {:02x}", b.rdram[i], a.rdram[i]));
    }
    if let Some(i) = (0..0x2000).find(|&i| a.rsp.memory[i] != b.rsp.memory[i]) {
        let what = if i < 0x1000 { "DMEM" } else { "IMEM" };
        return Some(format!("{what} at {:x}: {:02x}, not {:02x}", i & 0xfff, b.rsp.memory[i], a.rsp.memory[i]));
    }
    if let Some(r) = (0..32).find(|&r| a.rsp.gpr[r] != b.rsp.gpr[r]) {
        return Some(format!("register {r}: {:x}, not {:x}", b.rsp.gpr[r], a.rsp.gpr[r]));
    }
    let (mut left, mut right) = (Vec::new(), Vec::new());
    a.rsp.vu.save(&mut left);
    b.rsp.vu.save(&mut right);
    if let Some(i) = (0..left.len()).find(|&i| left[i] != right[i]) {
        return Some(format!("vector unit's state at byte {i}: {:02x}, not {:02x}", right[i], left[i]));
    }
    let registers = |n: &N64| [n.rsp.pc, n.rsp.mem_addr, n.rsp.dram_addr, n.rsp.length, u32::from(n.rsp.semaphore),
        n.rsp.status];
    let (x, y) = (registers(a), registers(b));
    let names = ["PC", "SP_MEM_ADDR", "SP_DRAM_ADDR", "SP length", "semaphore", "SP_STATUS"];
    (0..6).find(|&i| x[i] != y[i]).map(|i| format!("{} {:x}, not {:x}", names[i], y[i], x[i]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_are_the_microcodes_words() {
        // As in the microcodes, with their base registers' fields.
        let base = |word: u32, r: u32| word | r << 21;
        assert_eq!(vu(VSUB, 3, 3, 2, 8), 0x4b02_18d1);
        assert_eq!(vu(VLT, 9, 9, 2, 11), 0x4b62_4a60);
        assert_eq!(vu(VGE, 9, 9, 0, 0), 0x4a00_4a63);
        assert_eq!(vu(VMADN, 1, 0, 0, 8), 0x4b00_004e);
        assert_eq!(vu(VMRG, 22, 22, 29, 11), 0x4b7d_b5a7);
        assert_eq!(base(lwc2(UV, 3, 0, 0), 15), 0xc9e3_3800);
        assert_eq!(base(lwc2(HV, 7, 0, 0), 12), 0xc987_4000);
        assert_eq!(base(lwc2(BV, 10, 15, 0), 12), 0xc98a_0780);
        assert_eq!(base(swc2(FV, 9, 8, 1), 16), 0xea09_4c01);
        assert_eq!(base(swc2(SV, 22, 14, -16), 0), 0xe816_0f70);
        assert_eq!(base(swc2(RV, 22, 0, -8), 11), 0xe976_2878);
        assert_eq!(base(swc2(QV, 0, 0, -8), 0), 0xe800_2078);
    }

    #[test]
    fn yuv_groups_by_the_bytes_are_the_vector_units() {
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut random = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // The game's places (both passes of a DMEM's worth), others aligned, and the ends of DMEM.
        let places = [(0, 240, 0x200, 0x280, 0x300, 0x6c0), (480, 720, 0x278, 0x2f8, 0xa80, 0xe40),
            (0x48, 0x1f8, 0x3a0, 0x8, 0x200, 0xfc0), (0xff8, 0xfb0, 0xff8, 0xf80, 0xf40, 0xff0)];
        for round in 0..64 {
            let mut start = N64::new(&rom).unwrap();
            for b in start.rsp.memory[..0x1000].iter_mut() {
                *b = random() as u8;
            }
            start.rsp.vu.regs[0] = [0; 8];
            start.rsp.vu.regs[1] = std::array::from_fn(|_| random() as u16);
            start.rsp.vu.regs[2] = std::array::from_fn(|_| random() as u16);
            if round % 2 == 0 {
                // The game's constants.
                start.rsp.vu.regs[1][..5].copy_from_slice(&[0xb375, 0x5b69, 0x2c0d, 0xe2d1, 0x950b]);
                start.rsp.vu.regs[2][..4].copy_from_slice(&[0x4000, 0x7c00, 0x0001, 0x7f80]);
            }
            let (t4, t5, t6, t7, s0, s1) = places[round % places.len()];
            let mut g = [0u32; 32];
            (g[T4], g[T5], g[T6], g[T7], g[S0], g[S1], g[S2]) = (t4, t5, t6, t7, s0, s1, 240);
            let (mut fast, mut reference) = (start.clone(), start);
            let (mut a, mut b) = (g, g);
            assert!(fast.yuv_bytes(&mut a));
            reference.yuv_group(&mut b);
            assert_eq!(a, b, "registers, round {round}");
            let at = (0..0x1000).find(|&i| fast.rsp.memory[i] != reference.rsp.memory[i]);
            assert_eq!(at, None, "DMEM, round {round}");
        }
    }

    #[test]
    fn a_dmas_rows_are_its_length_apart_and_its_skip() {
        let rows: Vec<_> = rows(0x63e030, 0x0600_13bf).collect();
        assert_eq!(rows, [(0x63e030, 0x3c0), (0x63e030 + 0x420, 0x3c0)]);
    }
}
