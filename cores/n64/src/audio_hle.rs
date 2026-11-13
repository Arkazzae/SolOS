//! High-level emulation of the RSP's sound microcodes: a task's command list is read here and its work
//! done on the RSP's DMEM and RDRAM straight away, as the microcode would do it, instead of running the
//! microcode. Nothing here goes by game or by family of microcodes: a task's microcode is known by its
//! own code (recognise.rs: its dispatcher's fingerprint, its table of commands, what its start code
//! sets), each command by the routine its table entry leads to, looked up by its fingerprint in one
//! library of routines (routines.rs) that says what each does and how its code reads the command. The
//! commands' work is in files by what it is: adpcm.rs, resample.rs, envelope.rs, mixing.rs, filters.rs
//! and buffers.rs. A task with a command whose routine is not known still runs on the RSP. What this
//! writes to DMEM and RDRAM is what the microcode writes, byte for byte, but for scratch buffers in DMEM
//! some microcodes stage the commands' state in (and a wet mix one throws away) and 4 bytes of a
//! RESAMPLE state never read.
//!
//! The comparison mode runs the microcode as usual and this on a copy of the machine at the task's
//! start, then compares what each wrote to RDRAM (and the VCO and VCC left, where they go on to the next
//! task). At the first task that differs it finds the first command after which DMEM differs, running
//! the microcode again on copies with the command list cut short.

use crate::N64;
use recognise::{Code, Entry, Microcode};
// The video HLE knows its microcodes by the same fingerprints of their code.
pub(crate) use recognise::fingerprint;

mod adpcm;
mod buffers;
mod envelope;
mod filters;
mod mixing;
mod recognise;
mod resample;
mod routines;

/// Host preferences and comparison state, not part of the machine's state.
#[derive(Default)]
pub(crate) struct AudioHle {
    pub enabled: bool,
    pub compare: bool,
    /// While comparing: the machine at the running task's start, what this wrote to RDRAM from it, and
    /// the VCO and VCC it left (where they go on to the next task).
    expected: Option<(Box<N64>, Vec<(usize, Vec<u8>)>, Option<(u16, u16)>)>,
    /// Microcodes found, by the address of their data.
    known: Vec<(u32, Option<Microcode>)>,
    pub report: AudioReport,
}

/// A copy of the machine keeps the preferences only.
impl Clone for AudioHle {
    fn clone(&self) -> AudioHle {
        AudioHle { enabled: self.enabled, compare: self.compare, ..AudioHle::default() }
    }
}

/// What the comparison found.
#[derive(Default, Clone, Debug)]
pub struct AudioReport {
    pub tasks: u64,
    /// Tasks whose writes to RDRAM were the same, byte for byte.
    pub same: u64,
    pub notes: Vec<String>,
    /// Sound microcodes this does not know, which ran on the RSP: where their data lies in RDRAM and
    /// its size.
    pub unknown: Vec<(u32, u32)>,
    /// Tasks of a known microcode left to the RSP, for commands this does not run.
    pub declined: u64,
}

/// A task's state through its commands.
struct State {
    /// SEGMENT's table, for routines that take segmented RDRAM addresses.
    segments: [u32; 16],
    /// SETBUFF: the input, output and size (bytes), and the auxiliary buffers (dry right, wet left and
    /// right).
    input: usize,
    output: usize,
    count: usize,
    dry_right: usize,
    wet_left: usize,
    wet_right: usize,
    /// SETVOL: volumes, targets and rates (left, right), dry and wet gain.
    volume: [i16; 2],
    target: [i16; 2],
    rate: [i32; 2],
    dry: i16,
    wet: i16,
    /// SETLOOP: the loop's RDRAM address.
    loop_at: u32,
    /// The scratch in DMEM the start code points s7 at: routines that stage their state put it
    /// together there, and what a command leaves there the next finds.
    scratch: Option<usize>,
    /// The resampling filter (four taps for each of 64 phases) and ENVMIXER's eighths (1/8 to 8/8, the
    /// last as 0xffff), from the data where the routines read them.
    filter: [i16; 256],
    eighths: [u16; 8],
    /// What ENVSETUP1 and ENVSETUP2 keep for an ENVMIXER that reads it back.
    envelope: envelope::Envelope,
    /// The volumes ENVSETUP1 and ENVSETUP2 leave in a vector register for an ENVMIXER right after them
    /// (left, left a block on, right, right a block on, reverb, reverb a block on), and their steps in
    /// scalar registers (left, right, reverb).
    volume_lanes: [u16; 8],
    volume_steps: [u16; 3],
    /// The count and coefficients a filter's setup leaves for its run.
    filter_count: u32,
    filter_taps: [i16; 8],
    /// VCO's carries the task before left (which a first VSUB takes), and VCC as the last signed compare
    /// left it.
    vco: u16,
    vcc: Option<u16>,
    /// What the task wrote to RDRAM (for comparisons).
    written: Vec<(usize, usize)>,
}

impl State {
    /// A task's state before its commands.
    fn new(scratch: Option<usize>, filter: [i16; 256], eighths: [u16; 8], vco: u16) -> State {
        State {
            segments: [0; 16],
            input: 0,
            output: 0,
            count: 0,
            dry_right: 0,
            wet_left: 0,
            wet_right: 0,
            volume: [0; 2],
            target: [0; 2],
            rate: [0; 2],
            dry: 0,
            wet: 0,
            loop_at: 0,
            scratch,
            filter,
            eighths,
            envelope: envelope::Envelope::default(),
            volume_lanes: [0; 8],
            volume_steps: [0; 3],
            filter_count: 0,
            filter_taps: [0; 8],
            vco,
            vcc: None,
            written: Vec::new(),
        }
    }
}


/// One lane of the vector unit's accumulator (48 bits), for ENVMIXER's
/// sums made as the microcode makes them.
#[derive(Clone, Copy)]
struct Acc(i64);

impl Acc {
    fn add(self, v: i64) -> Acc {
        Acc((self.0 + v) << 16 >> 16)
    }

    /// The high and middle slices clamped to 16 bits (VMADH, VMADM, VMACF).
    fn high(self) -> i16 {
        (self.0 >> 16).clamp(-0x8000, 0x7fff) as i16
    }

    /// The low slice, or 0 or 0xffff where the high and middle ones are out
    /// of 16 bits (VMADN).
    fn low(self) -> u16 {
        let rest = self.0 >> 16;
        if rest < -0x8000 {
            0
        } else if rest > 0x7fff {
            0xffff
        } else {
            self.0 as u16
        }
    }
}

/// A sample of the output times 0x7fff (VMULF) plus one of the input times
/// the gain (VMACF): mixing as the microcode does it, rounded once. The
/// accumulator's out * 0xfffe + 0x8000 + 2 * x * gain, shifted down by 16,
/// is out and the rest shifted down by 15, which fits 32 bits.
fn mix(out: i16, x: i16, gain: i16) -> i16 {
    let (out, x, gain) = (i32::from(out), i32::from(x), i32::from(gain));
    clamp(out + (x * gain - out + 0x4000 >> 15))
}

/// The sample at a DMEM address.
#[inline(always)]
fn sample(dmem: &[u8; 0x1000], at: usize) -> i16 {
    let at = at & 0xffe;
    i16::from_be_bytes([dmem[at], dmem[at + 1]])
}

#[inline(always)]
fn set_sample(dmem: &mut [u8; 0x1000], at: usize, v: i16) {
    let at = at & 0xffe;
    [dmem[at], dmem[at + 1]] = v.to_be_bytes();
}

/// N samples from DMEM (wrapping at its end, as the RSP does): one load where they do not wrap.
#[inline(always)]
fn read_samples<const N: usize>(dmem: &[u8; 0x1000], at: usize) -> [i16; N] {
    let at = at & 0xffe;
    match dmem.get(at..at + 2 * N) {
        Some(bytes) => std::array::from_fn(|n| i16::from_be_bytes([bytes[2 * n], bytes[2 * n + 1]])),
        None => std::array::from_fn(|n| sample(dmem, at + 2 * n)),
    }
}

/// 16 big-endian bytes as 8 samples. The bytes of each pair are swapped within 32-bit words, by
/// masks: the compiler turns that into vector instructions, where it leaves swaps of 16 bits scalar.
#[inline(always)]
fn from_be(bytes: &[u8; 16]) -> [i16; 8] {
    let words: [u32; 4] = std::array::from_fn(|m| u32::from_le_bytes(bytes[4 * m..4 * m + 4].try_into().unwrap()));
    let words = words.map(|w| (w & 0x00ff_00ff) << 8 | w >> 8 & 0x00ff_00ff);
    std::array::from_fn(|n| (words[n / 2] >> (16 * (n & 1))) as i16)
}

/// 8 samples as 16 big-endian bytes, as `from_be`.
#[inline(always)]
fn to_be(block: [i16; 8]) -> [u8; 16] {
    let mut bytes = [0; 16];
    for m in 0..4 {
        let w = u32::from(block[2 * m] as u16) | u32::from(block[2 * m + 1] as u16) << 16;
        bytes[4 * m..4 * m + 4].copy_from_slice(&((w & 0x00ff_00ff) << 8 | w >> 8 & 0x00ff_00ff).to_le_bytes());
    }
    bytes
}

/// Whether `len` bytes of DMEM from `at` (as blocks of samples take it, at an even address) stay
/// before its end.
fn inside(at: usize, len: usize) -> bool {
    (at & 0xffe) + len <= 0x1000
}

/// How a command's loop reaches DMEM's blocks of 8 samples.
pub(super) trait Blocks {
    fn read(dmem: &[u8; 0x1000], at: usize) -> [i16; 8];
    fn write(dmem: &mut [u8; 0x1000], at: usize, block: [i16; 8]);
}

/// Blocks a command has checked stay before DMEM's end (`inside`): one access each, without a
/// branch, so that a loop's body is straight code the compiler turns into vector instructions.
pub(super) struct Inside;

/// Any blocks, wrapping at DMEM's end as the RSP does.
pub(super) struct Wrapping;

impl Blocks for Inside {
    #[inline(always)]
    fn read(dmem: &[u8; 0x1000], at: usize) -> [i16; 8] {
        #[cfg(test)]
        assert!(inside(at, 16), "a block at {at:x} past DMEM's end through Inside");
        // Within DMEM without a branch (a block the command checked is already).
        let at = (at & 0xffe).min(0xff0);
        from_be(dmem[at..at + 16].try_into().unwrap())
    }

    #[inline(always)]
    fn write(dmem: &mut [u8; 0x1000], at: usize, block: [i16; 8]) {
        #[cfg(test)]
        assert!(inside(at, 16), "a block at {at:x} past DMEM's end through Inside");
        let at = (at & 0xffe).min(0xff0);
        dmem[at..at + 16].copy_from_slice(&to_be(block));
    }
}

impl Blocks for Wrapping {
    #[inline(always)]
    fn read(dmem: &[u8; 0x1000], at: usize) -> [i16; 8] {
        let at = at & 0xffe;
        from_be(&std::array::from_fn(|i| dmem[(at + i) & 0xfff]))
    }

    #[inline(always)]
    fn write(dmem: &mut [u8; 0x1000], at: usize, block: [i16; 8]) {
        let at = at & 0xffe;
        for (i, b) in to_be(block).into_iter().enumerate() {
            dmem[(at + i) & 0xfff] = b;
        }
    }
}

/// 8 samples from DMEM (wrapping at its end, as the RSP does).
#[inline(always)]
fn read_block(dmem: &[u8; 0x1000], at: usize) -> [i16; 8] {
    if inside(at, 16) { Inside::read(dmem, at) } else { Wrapping::read(dmem, at) }
}

#[inline(always)]
fn write_block(dmem: &mut [u8; 0x1000], at: usize, block: [i16; 8]) {
    if inside(at, 16) { Inside::write(dmem, at, block) } else { Wrapping::write(dmem, at, block) }
}

/// Samples as big-endian bytes, into the start of `bytes`.
fn put_samples(bytes: &mut [u8], samples: &[i16]) {
    for (b, v) in bytes.chunks_exact_mut(2).zip(samples) {
        b.copy_from_slice(&v.to_be_bytes());
    }
}

fn clamp(v: i32) -> i16 {
    v.clamp(-0x8000, 0x7fff) as i16
}

/// Rounded up to a multiple of `n` (a power of two).
fn align(v: usize, n: usize) -> usize {
    (v + n - 1) & !(n - 1)
}

impl N64 {
    /// The CPU started the RSP on a sound task: runs it here if this knows its microcode and every
    /// command in its list (true), or sets up the comparison (false: the RSP runs it).
    pub(crate) fn audio_task_started(&mut self) -> bool {
        if !self.audio.enabled && !self.audio.compare {
            return false;
        }
        let word = |o: u32| self.rsp.read_memory(0xfc0 + o);
        // A task the RSP yielded goes on there.
        if word(0x04) & 1 != 0 {
            return false;
        }
        let Some(microcode) = self.microcode(word(0x18) & 0x7f_ffff, word(0x1c)) else { return false };
        let (list, size) = (self.rsp.read_memory(0xfc0 + 0x30) & 0x7f_ffff, self.rsp.read_memory(0xfc0 + 0x34));
        if let Some(reason) = microcode.decline_reason(&self.rdram, list as usize, size as usize) {
            if self.audio.compare {
                self.audio.report.notes.push(format!("task declined: {reason}"));
            }
            self.audio.report.declined += 1;
            return false;
        }
        if self.audio.enabled {
            let (rsp_cycles, _) = self.run_audio(usize::MAX);
            self.hle_task_busy(rsp_cycles);
            return true;
        }
        let start = Box::new(self.clone());
        let mut copy = self.clone();
        let (_, written) = copy.run_audio(usize::MAX);
        let written = written.iter().map(|&(at, len)| (at, copy.rdram[at..at + len].to_vec())).collect();
        // The VCO and VCC the next task finds, where they go on.
        let flags = microcode.dispatcher.flags
            .then(|| (copy.rsp.vu.control(0) as u16, copy.rsp.vu.control(1) as u16));
        self.audio.expected = Some((start, written, flags));
        false
    }

    /// After the RSP stopped: compares what a sound task wrote (and left in
    /// VCO and VCC, where this keeps them).
    pub(crate) fn audio_task_ran(&mut self) {
        let Some((start, written, flags)) = self.audio.expected.take() else { return };
        let report = &mut self.audio.report;
        report.tasks += 1;
        let left = (self.rsp.vu.control(0) as u16, self.rsp.vu.control(1) as u16);
        let differs = written.iter()
            .find(|(at, bytes)| self.rdram[*at..*at + bytes.len()] != bytes[..])
            .map(|(at, _)| format!("RDRAM at {at:x} differs"))
            .or_else(|| flags.filter(|&ours| ours != left).map(|(vco, vcc)| {
                format!("VCO and VCC {vco:04x} {vcc:04x}, not {:04x} {:04x}", left.0, left.1)
            }));
        let Some(what) = differs else {
            report.same += 1;
            return;
        };
        if report.notes.len() < 4 {
            let note = format!("task {}: {what}; {}", report.tasks, start.first_difference());
            self.audio.report.notes.push(note);
        }
    }

    /// The first command after which DMEM, or RDRAM where this wrote it,
    /// differs from the microcode's, from the machine at a task's start: the
    /// microcode runs lists cut short on copies, and this on others; DMEM is
    /// compared where this changed it from the task's data, loaded (the
    /// microcode keeps its own state in the rest).
    fn first_difference(&self) -> String {
        let microcode = |commands: usize| {
            let mut copy = self.clone();
            copy.rsp.write_memory(0xfc0 + 0x34, 8 * commands as u32, !0);
            for _ in 0..100_000 {
                if copy.rsp.halted() {
                    break;
                }
                copy.rsp_slice();
            }
            copy
        };
        let commands = self.rsp.read_memory(0xfc0 + 0x34) as usize / 8;
        let list = (self.rsp.read_memory(0xfc0 + 0x30) & 0x7f_ffff) as usize;
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        // DMEM as the boot microcode leaves it, with the task's data loaded.
        let mut loaded = self.clone();
        let (data, size) = (self.rsp.read_memory(0xfc0 + 0x18) & 0x7f_ffff, self.rsp.read_memory(0xfc0 + 0x1c));
        loaded.dma_in(0, data as usize, size as usize);
        for n in 1..=commands {
            let expected = microcode(n);
            let mut copy = self.clone();
            let (_, written) = copy.run_audio(n);
            let (actual, wanted) = (&copy.rsp.memory[..0xfc0], &expected.rsp.memory[..0xfc0]);
            let changed = |i: usize| actual[i] != loaded.rsp.memory[i];
            let mut diffs: Vec<String> = (0..0xfc0).step_by(2)
                .filter(|&i| (changed(i) || changed(i + 1)) && actual[i..i + 2] != wanted[i..i + 2])
                .take(6)
                .map(|i| {
                    format!("{i:x}: {:02x}{:02x} vs {:02x}{:02x}", actual[i], actual[i + 1], wanted[i], wanted[i + 1])
                })
                .collect();
            diffs.extend(written.iter().filter(|&&(at, len)| copy.rdram[at..at + len] != expected.rdram[at..at + len])
                .take(2)
                .map(|&(at, len)| {
                    let len = len.min(16);
                    let (ours, theirs) = (hex(&copy.rdram[at..at + len]), hex(&expected.rdram[at..at + len]));
                    format!("RDRAM {at:x}: {ours} vs {theirs}")
                }));
            if !diffs.is_empty() {
                let at = list + 8 * (n - 1);
                let word = |at: usize| u32::from_be_bytes(self.rdram[at..at + 4].try_into().unwrap());
                return format!("command {} ({:08x} {:08x}) differs first: {}", n - 1, word(at), word(at + 4),
                    diffs.join(", "));
            }
        }
        "DMEM and RDRAM the same after every command".to_string()
    }

    /// The microcode with this data, as its code and data describe it, if this knows it.
    fn microcode(&mut self, data: u32, size: u32) -> Option<Microcode> {
        if let Some(&(_, known)) = self.audio.known.iter().find(|(at, _)| *at == data) {
            return known;
        }
        let known = Microcode::of(&self.rdram, data as usize, size, Code::of(self));
        self.audio.known.push((data, known));
        if known.is_none() && self.audio.compare {
            self.audio.report.unknown.push((data, size));
        }
        known
    }

    /// Runs the task's command list (at most `limit` commands): about how many cycles the microcode's
    /// RSP would take over it, and what it wrote to RDRAM.
    fn run_audio(&mut self, limit: usize) -> (u64, Vec<(usize, usize)>) {
        let word = |o: u32| self.rsp.read_memory(0xfc0 + o);
        let (data, list) = ((word(0x18) & 0x7f_ffff) as usize, (word(0x30) & 0x7f_ffff) as usize);
        let size = word(0x34) as usize;
        let data_size = word(0x1c) as usize;
        let Some(microcode) = self.microcode(data as u32, data_size as u32) else { return (0, Vec::new()) };
        // The boot microcode loads the task's data into DMEM, as it would for the microcode.
        self.dma_in(0, data, data_size);
        // The constants the routines read from the data.
        let half = |at: Option<u16>, i: usize| at.map_or(0, |at| {
            let at = data + usize::from(at) + 2 * i;
            u16::from_be_bytes([self.rdram[at], self.rdram[at + 1]])
        });
        let filter = std::array::from_fn(|i| half(microcode.filter, i) as i16);
        let eighths = std::array::from_fn(|i| half(microcode.eighths, i));
        let mut s = State::new(microcode.scratch, filter, eighths, self.rsp.vu.control(0) as u16);
        let dispatcher = microcode.dispatcher;
        let mut cycles = u64::from(dispatcher.task);
        // The next command before which the dispatcher fetches more of the list, and how often it does.
        let (fetch, every, mut next_fetch) = dispatcher.fetch
            .map_or((0, 0, usize::MAX), |f| (u64::from(f.cost), usize::from(f.every), usize::from(f.at)));
        for n in 0..(size / 8).min(limit) {
            let at = list + 8 * n;
            let w0 = u32::from_be_bytes(self.rdram[at..at + 4].try_into().unwrap());
            let w1 = u32::from_be_bytes(self.rdram[at + 4..at + 8].try_into().unwrap());
            if n == next_fetch {
                cycles += fetch;
                next_fetch += every;
            }
            cycles += match microcode.entry(w0) {
                Some(Entry::Idle) => u64::from(dispatcher.command) + u64::from(dispatcher.idle.unwrap_or(0)),
                Some(Entry::Runs(routine)) => u64::from(dispatcher.command) + self.command(&mut s, routine, w0, w1),
                None => 0,
            };
        }
        if dispatcher.flags {
            self.rsp.vu.set_control(0, u32::from(s.vco));
            if let Some(vcc) = s.vcc {
                self.rsp.vu.set_control(1, u32::from(vcc));
            }
        }
        (cycles, s.written)
    }

    /// DMEM as an array, which addresses masked to it need no checks in.
    #[inline(always)]
    fn dmem(&self) -> &[u8; 0x1000] {
        self.rsp.memory[..0x1000].try_into().unwrap()
    }

    #[inline(always)]
    fn dmem_mut(&mut self) -> &mut [u8; 0x1000] {
        (&mut self.rsp.memory[..0x1000]).try_into().unwrap()
    }

    #[inline(always)]
    fn dmem_sample(&self, at: usize) -> i16 {
        sample(self.dmem(), at)
    }

    /// 8 samples from DMEM (wrapping at its end, as the RSP does).
    #[inline(always)]
    fn dmem_block(&self, at: usize) -> [i16; 8] {
        read_block(self.dmem(), at)
    }

    #[inline(always)]
    fn set_dmem_sample(&mut self, at: usize, v: i16) {
        set_sample(self.dmem_mut(), at, v);
    }

    fn rdram_half(&self, at: usize) -> i16 {
        let at = at & 0x7f_fffe;
        i16::from_be_bytes([self.rdram[at], self.rdram[at + 1]])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const DATA: usize = 0x1000;
    pub(super) const LIST: usize = 0x2000;
    pub(super) const STATE: usize = 0x3000;

    /// The routines of a table of 16 commands, by number (from 1), with their buffers from 0x5c0 and
    /// their states put together in the scratch; ENVMIXER ramping by adding (`MULTIPLYING` the one that
    /// multiplies). And their dispatcher.
    pub(super) const STAGED: [u64; 16] = [0, 0x60e4_de64_ddf1_8ba4, 0x18e2_527b_7659_0feb, 0x75cc_048b_4cae_acef,
        0x95cd_9e0f_aa20_528e, 0xf312_972f_bfc1_9e38, 0xfbed_beee_1ca3_618c, 0x8617_ba0b_6d0b_16ef,
        0x469e_058d_eadb_6f51, 0x6db6_0897_3b02_1dc7, 0x7066_4bf3_9395_5147, 0x2665_d735_bbb2_60d7,
        0x3138_8b79_dd3f_19b5, 0x7c09_81be_cc05_61d4, 0x8a4c_0324_742d_11b4, 0x4a24_a33a_18fa_01d7];
    pub(super) const MULTIPLYING: u64 = 0x1603_647c_1e77_0951;
    pub(super) const STAGED_DISPATCHER: u64 = 0x53d4_d6d8_59b6_1aa0;
    /// The scratch the start code of `STAGED`'s microcode sets.
    pub(super) const SCRATCH: usize = 0xf90;
    /// Their buffers' base.
    pub(super) const BASE: usize = 0x5c0;

    pub(super) fn put16(n64: &mut N64, at: usize, value: u16) {
        n64.rdram[at..at + 2].copy_from_slice(&value.to_be_bytes());
    }

    pub(super) fn halves(n64: &N64, at: usize) -> [u16; 8] {
        std::array::from_fn(|i| u16::from_be_bytes([n64.rdram[at + 2 * i], n64.rdram[at + 2 * i + 1]]))
    }

    /// A routine of the library by its fingerprint.
    pub(super) fn routine(print: u64) -> &'static routines::Routine {
        routines::known(print).unwrap()
    }

    /// A machine with a sound task in DMEM: `microcode` known by its data at DATA (`size` bytes, with
    /// `data`'s halves at their offsets), and `commands` at LIST.
    pub(super) fn machine(microcode: Microcode, size: u32, data: &[(usize, &[u16])], commands: &[(u32, u32)]) -> N64 {
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        let mut n64 = N64::new(&rom).unwrap();
        for &(at, values) in data {
            for (i, &v) in values.iter().enumerate() {
                put16(&mut n64, DATA + at + 2 * i, v);
            }
        }
        n64.audio.known.push((DATA as u32, Some(microcode)));
        for (offset, value) in [(0x04, 0), (0x18, 0x8000_0000 | DATA as u32), (0x1c, size),
            (0x30, 0x8000_0000 | LIST as u32), (0x34, 8 * commands.len() as u32)] {
            n64.rsp.write_memory(0xfc0 + offset, value, !0);
        }
        for (i, &(w0, w1)) in commands.iter().enumerate() {
            n64.rdram[LIST + 8 * i..LIST + 8 * i + 4].copy_from_slice(&w0.to_be_bytes());
            n64.rdram[LIST + 8 * i + 4..LIST + 8 * i + 8].copy_from_slice(&w1.to_be_bytes());
        }
        n64
    }

    /// A machine with `STAGED`'s microcode (its ENVMIXER the routine `envmixer`), its data's first
    /// words, masks and ENVMIXER's eighths, and a sound task of `commands`.
    pub(super) fn staged(envmixer: u64, commands: &[(u32, u32)]) -> N64 {
        let mut routines: Vec<(usize, u64)> = (1..16).map(|op| (op, STAGED[op])).collect();
        routines[2].1 = envmixer;
        let microcode = Microcode::with(STAGED_DISPATCHER, Some(SCRATCH), &routines);
        let eighths = [0x2000, 0x4000, 0x6000, 0x8000, 0xa000, 0xc000, 0xe000, 0xffff];
        machine(microcode, 0x2c0, &[(0, &[0, 1]), (0x30, &[0xf000, 0x0f00]), (0xb0, &eighths)], commands)
    }

    #[test]
    fn mixing_in_32_bits_is_the_accumulators() {
        let mut values = vec![-0x8000, -0x7fff, -0x4001, -0x4000, -1, 0, 1, 0x3fff, 0x4000, 0x7ffe, 0x7fff];
        let mut seed = 1u32;
        values.extend((0..60).map(|_| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) as i16
        }));
        for &out in &values {
            for &x in &values {
                for &g in &values {
                    let acc = Acc(i64::from(out) * 0xfffe + 0x8000 + i64::from(x) * i64::from(g) * 2).high();
                    assert_eq!(mix(out, x, g), acc, "{out} {x} {g}");
                }
            }
        }
    }

    #[test]
    fn other_microcodes_stay_on_the_rsp() {
        // A microcode's data with the first words and constants of a known one, but code this does not
        // know (none: no dispatcher this knows).
        let mut n64 = staged(MULTIPLYING, &[]);
        n64.audio.known.clear();
        for i in 0..16 {
            put16(&mut n64, DATA + 0x10 + 2 * i, 0x1100 + 0x40 * i as u16);
        }
        assert!(n64.microcode(DATA as u32, 0x2c0).is_none());
        assert!(matches!(n64.audio.known[..], [(at, None)] if at == DATA as u32),
            "and not known by its address once looked at");
        assert!(n64.microcode(DATA as u32, 0x2c0).is_none());
    }

    /// A task's state before its commands: no buffers, volumes or scratch; a resampling filter and
    /// ENVMIXER's eighths.
    pub(super) fn blank_state() -> State {
        let filter = std::array::from_fn(|i| (i as i16).wrapping_mul(0x0123));
        State::new(None, filter, [0x2000, 0x4000, 0x6000, 0x8000, 0xa000, 0xc000, 0xe000, 0xffff], 0)
    }

    /// A machine with DMEM and RDRAM's first 64 KB of noise, DMEM's moved on by `d` bytes (what is
    /// at i with d = 0 is at i + d, wrapping at DMEM's end).
    pub(super) fn noisy(d: usize) -> N64 {
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        let mut n64 = N64::new(&rom).unwrap();
        let mut seed = 0x1234_5678u32;
        let mut next = || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) as u8
        };
        for i in 0..0x1000 {
            n64.rsp.memory[(i + d) & 0xfff] = next();
        }
        for i in 0..0x10000 {
            n64.rdram[i] = next();
        }
        n64
    }

    /// Runs commands on DMEM of noise moved on by each of `shifts`, their buffers moved on as much
    /// (`run` makes and runs them for a shift), and checks that DMEM, moved back, and RDRAM are as
    /// they are where nothing moved: the same whether a command's buffers reach DMEM's end (each
    /// block through `Inside`, which checks it stays before the end) or wrap there.
    pub(super) fn alike_wherever(shifts: &[usize], run: impl Fn(&mut N64, usize)) {
        let after = |d: usize| {
            let mut n64 = noisy(d);
            run(&mut n64, d);
            let dmem: Vec<u8> = (0..0x1000).map(|i| n64.rsp.memory[(i + d) & 0xfff]).collect();
            (dmem, n64.rdram[..0x10000].to_vec())
        };
        let (dmem, rdram) = after(0);
        for &d in shifts {
            let (moved, written) = after(d);
            assert!(moved == dmem, "DMEM differs with the buffers moved on by {d:x}");
            assert!(written == rdram, "RDRAM differs with the buffers moved on by {d:x}");
        }
    }

    /// Runs commands of these routines (fingerprints) on a blank state.
    pub(super) fn run(n64: &mut N64, commands: &[(u64, u32, u32)]) {
        let mut s = blank_state();
        for &(print, w0, w1) in commands {
            n64.command(&mut s, routine(print), w0, w1);
        }
    }

    #[test]
    fn blocks_are_their_samples_wherever_they_are() {
        let n64 = noisy(0);
        let dmem = n64.dmem();
        let block = [0x0102, -0x0304, 0x0506, -0x0708, 0x090a, -0x0b0c, 0x0d0e, -0x0f10];
        for at in (0..0x1010).chain([0x2ff0, 0x3fff, 0x10_0ffa]) {
            let samples: [i16; 8] = std::array::from_fn(|n| sample(dmem, at + 2 * n));
            let mut written = *dmem;
            for (n, &v) in block.iter().enumerate() {
                set_sample(&mut written, at + 2 * n, v);
            }
            let mut ours = (*dmem, *dmem);
            assert_eq!((read_block(dmem, at), Wrapping::read(dmem, at)), (samples, samples), "{at:x}");
            write_block(&mut ours.0, at, block);
            Wrapping::write(&mut ours.1, at, block);
            assert!(ours.0 == written && ours.1 == written, "{at:x}");
            if inside(at, 16) {
                assert_eq!(Inside::read(dmem, at), samples, "{at:x}");
                let mut ours = *dmem;
                Inside::write(&mut ours, at, block);
                assert!(ours == written, "{at:x}");
            }
        }
    }
}
