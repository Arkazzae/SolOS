//! High-level emulation of the RSP's graphics microcode: instead of
//! running the microcode on the RSP, a graphics task's display list is
//! read here and turned straight into the RDP's commands, which the RDP
//! (raster.rs, on its own thread) then draws as before. The microcode is
//! told apart by the version text in its data (F3D, the original GBI, and
//! Rare's variant of it, for now); a task of any other microcode still
//! runs on the RSP. The task is
//! done the moment it starts: the RSP stops with the task-done signal and
//! interrupts as the microcode's BREAK would.
//!
//! The arithmetic is floating point, after the GBI's definition of each
//! command (libultra's gbi.h), not the microcode's fixed point, so the
//! triangles come out close to, but not exactly as, the microcode's. The
//! comparison mode runs the microcode as usual and, for each graphics task,
//! this on a copy of the machine at the task's start, and compares the two
//! lists of RDP commands.

use crate::{N64, SP_INTR};

mod s2dex;

// SP_STATUS, as the RSP leaves it at a task's end: halted, broke, and
// signal 2 (libultra's task done).
const HALTED: u32 = 1 << 0;
const BROKE: u32 = 1 << 1;
const INTBREAK: u32 = 1 << 6;
const TASK_DONE: u32 = 1 << 9;


/// The microcodes known here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ucode {
    /// F3D (GBI0): "RSP SW Version: 2.0D" (Super Mario 64).
    F3d,
    /// Rare's F3D, "RSP SW Version: 2.0G" (GoldenEye 007): four triangles
    /// a command (0xb1), and texture coordinates the other way round.
    F3dRare,
    /// Perfect Dark's, Rare's F3D changed: vertices of 12 bytes (x, y, z, flags, a colour index,
    /// s, t) whose colour (or normal) is 4 bytes at that offset in a table command 0x07 copies
    /// into DMEM (G_VTXCOLORBASE), no flat shading, G_POPMTX and G_CULLDL doing nothing. Its data
    /// has no version text: it is known by its first 0x200 bytes.
    F3dPd,
    /// F3DEX2 (GBI2): "RSP Gfx ucode F3DEX       fifo 2.07" (Donkey Kong
    /// 64, Paper Mario) and F3DZEX (Ocarina of Time); `near` false for the
    /// .NoN ones, which do not clip at the near plane; `reject` for the .Rej ones (Resident Evil 2), which leave
    /// out a triangle partly outside the clipping box instead of clipping it.
    F3dex2 { near: bool, reject: bool },
    /// F3DEX 1.x (GBI1): F3D's opcodes with vertex indices doubled, 32
    /// vertices, two triangles a command (0xb1) and G_MODIFYVTX where F3D
    /// had G_RDPHALF_CONT: "RSP Gfx ucode F3DEX         0.95" (Mario Kart
    /// 64).
    F3dex { near: bool },
    /// S2DEX 2.0x (GBI2), the 2D microcode (s2dex.rs): "RSP Gfx ucode S2DEX       fifo 2.08" (Majora's Mask,
    /// Mario Party 2 and 3), 2.06 (Resident Evil 2, Mario Party), and Ocarina of Time's "RSP Gfx[Safe] S2DEX
    /// fifo 2.05", `safe`, which follows each group of commands it sends with a pipe sync; `xbus` for the builds
    /// whose output the RDP reads from DMEM.
    S2dex { safe: bool, xbus: bool },
}

/// Host preferences and comparison state, not part of the machine's state.
#[derive(Default)]
pub(crate) struct Hle {
    pub enabled: bool,
    pub compare: bool,
    /// RDP commands are gathered here (and with `capture_only` not run).
    pub capturing: bool,
    pub capture_only: bool,
    /// A task this ran is still being made to look busy.
    finishing: bool,
    /// This is running a task's display list now.
    drawing: bool,
    /// The RSP is handing the RDP commands now (writing DPC_END).
    pub from_rsp: bool,
    /// Commands made while the CPU has the RDP frozen: it runs them once
    /// thawed, as it would run what the microcode left in its FIFO.
    pub held: Vec<Vec<u64>>,
    pub captured: Vec<Vec<u64>>,
    /// While comparing: what this made of the task the RSP is running.
    expected: Option<Vec<Vec<u64>>>,
    /// Microcodes found, by the address of their data.
    known: Vec<(u32, Option<Ucode>)>,
    pub report: Report,
}

/// Following a display list for G_LOAD_UCODE (`loads_microcode`).
struct Scan {
    /// The microcode the commands are read for: F3DEX2's or S2DEX's opcodes (GBI2), or F3DEX 1.x's.
    ucode: Ucode,
    gbi2: bool,
    /// S2DEX's G_RDPHALF_0, whose w0 has the low half of G_SELECT_DL's address.
    half0: u32,
    segments: [u32; 16],
    half: u32,
    /// Lists followed, by their address, the segments and G_RDPHALF_1.
    seen: Vec<u64>,
    /// Commands left to read.
    budget: u32,
}

impl Scan {
    fn address(&self, addr: u32) -> u32 {
        let physical = self.segments[(addr >> 24 & 15) as usize].wrapping_add(addr & 0x00ff_ffff);
        if self.gbi2 { physical } else { physical & 0x00ff_ffff }
    }
}

/// A copy of the machine keeps the preferences only.
impl Clone for Hle {
    fn clone(&self) -> Hle {
        Hle { enabled: self.enabled, compare: self.compare, ..Hle::default() }
    }
}

/// What the comparison found.
#[derive(Default, Clone, Debug)]
pub struct Report {
    pub tasks: u64,
    pub commands: u64,
    /// Commands the same, word for word (setting state, loading, ...).
    pub same: u64,
    /// Triangles compared, and those within the tolerances.
    pub triangles: u64,
    pub close: u64,
    /// Tasks whose lists differ in length or in kinds of commands.
    pub misaligned: u64,
    /// The first differences, described.
    pub notes: Vec<String>,
    /// The first tasks with fewer than nine in ten triangles close.
    pub poor: Vec<String>,
    /// Tasks left to the RSP because they load a microcode this does not know.
    pub loading: u64,
}

/// libultra's OSTask, as osSpTaskLoad leaves it in DMEM at 0xFC0.
struct Task {
    flags: u32,
    dram_stack_size: u32,
    ucode_data: u32,
    ucode_data_size: u32,
    data_ptr: u32,
}

/// F3DEX2's kinds known here, by the name and version after "RSP Gfx ucode
/// ": F3DEX or F3DZEX, plain or .NoN, of version 2.
fn gbi2(data: &[u8]) -> Option<Ucode> {
    const TITLE: &[u8] = b"RSP Gfx ucode ";
    let at = data.windows(TITLE.len()).position(|w| w == TITLE)? + TITLE.len();
    let text = &data[at..(at + 40).min(data.len())];
    let name_end = text.iter().position(|&b| b == b' ')?;
    let (name, rest) = text.split_at(name_end);
    let rest = rest.trim_ascii_start();
    let two = rest.starts_with(b"fifo 2.") || rest.starts_with(b"xbus 2.");
    // F3DEX 1.x: a version from 0.9 to 1.x after the name.
    let one = rest.starts_with(b"0.9") || rest.starts_with(b"1.");
    match (name, two, one) {
        (b"F3DEX" | b"F3DZEX", true, _) => Some(Ucode::F3dex2 { near: true, reject: false }),
        (b"F3DEX.NoN" | b"F3DZEX.NoN", true, _) => Some(Ucode::F3dex2 { near: false, reject: false }),
        (b"F3DEX.Rej", true, _) => Some(Ucode::F3dex2 { near: true, reject: true }),
        (b"F3DEX", _, true) => Some(Ucode::F3dex { near: true }),
        (b"F3DEX.NoN", _, true) => Some(Ucode::F3dex { near: false }),
        _ => None,
    }
}

/// The FNV-1a hash of Perfect Dark's microcode's first 0x200 bytes of data (its overlays' table,
/// commands' entries and constants).
const PERFECT_DARK_DATA: u64 = 0x2c10_119b_8fac_62ff;

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

fn physical(addr: u32) -> u32 {
    addr & 0x00ff_ffff
}

impl N64 {
    /// The CPU started the RSP on a graphics task: runs it here if this
    /// knows its microcode (true), or sets up the comparison (false: the RSP
    /// runs it).
    pub(crate) fn gfx_task_started(&mut self) -> bool {
        if self.video_task_started() {
            return true;
        }
        if !self.hle.enabled && !self.hle.compare {
            return false;
        }
        let word = |o: u32| self.rsp.read_memory(0xfc0 + o);
        let task = Task { flags: word(0x04), dram_stack_size: word(0x24), ucode_data: physical(word(0x18)),
            ucode_data_size: word(0x1c), data_ptr: physical(word(0x30)) };
        // A task the RSP yielded goes on there.
        if task.flags & 1 != 0 {
            return false;
        }
        let Some(ucode) = self.identify(&task) else { return false };
        if self.hle.enabled && self.loads_microcode(ucode, task.data_ptr) {
            // The RSP runs it, going on with the other microcode where this could not.
            self.hle.report.loading += 1;
            return false;
        }
        if self.hle.enabled {
            // Drawn now; the RSP looks busy until about when the microcode
            // would have finished.
            self.hle.drawing = true;
            let rsp_cycles = self.run_gfx(ucode, &task);
            self.hle.drawing = false;
            self.hle_task_busy(rsp_cycles);
            return true;
        }
        self.finish_comparison();
        let mut copy = self.clone();
        copy.hle = Hle { capturing: true, capture_only: true, ..Hle::default() };
        copy.run_gfx(ucode, &task);
        self.hle.expected = Some(std::mem::take(&mut copy.hle.captured));
        self.hle.captured.clear();
        self.hle.capturing = true;
        false
    }

    /// A task this ran: the RSP looks busy until about when the microcode
    /// would have finished (its cycles at 2/3 of the CPU's clock).
    pub(crate) fn hle_task_busy(&mut self, rsp_cycles: u64) {
        self.hle.finishing = true;
        let at = self.cycles + rsp_cycles * 3 / 2;
        self.schedule(crate::RSP, at);
    }

    /// The RSP's event while a task this ran looks busy: it is done.
    pub(crate) fn gfx_task_finished(&mut self) -> bool {
        if !self.hle.finishing {
            return false;
        }
        self.hle.finishing = false;
        self.video_task_finished();
        let rsp = &mut self.rsp;
        rsp.status |= HALTED | BROKE | TASK_DONE;
        if rsp.status & INTBREAK != 0 {
            self.request(SP_INTR);
        }
        self.rsp_stopped();
        true
    }

    /// Whether a task's display list can reach a command that loads a microcode this does not follow
    /// (G_LOAD_UCODE: Majora's Mask and Ocarina of Time load S2DEX for backgrounds, then their own back; F3DEX
    /// 1.x's loads are not followed). Only what moves through the lists is followed: calls, branches and ends,
    /// segments, G_BRANCH_Z both ways (as a call), G_CULLDL as not culled, S2DEX's G_SELECT_DL as a call, and
    /// the loads this knows, whose microcode reads the commands after; each list once for a segment table.
    fn loads_microcode(&self, ucode: Ucode, start: u32) -> bool {
        let gbi2 = matches!(ucode, Ucode::F3dex2 { .. } | Ucode::S2dex { .. });
        if !gbi2 && !matches!(ucode, Ucode::F3dex { .. }) {
            return false;
        }
        let mut scan = Scan { ucode, gbi2, half0: 0, segments: [0; 16], half: 0, seen: Vec::new(), budget: 1 << 18 };
        self.scan_list(&mut scan, start, 0)
    }

    fn scan_list(&self, scan: &mut Scan, mut pc: u32, depth: usize) -> bool {
        let key = |scan: &Scan, pc: u32| {
            let s2dex = u64::from(matches!(scan.ucode, Ucode::S2dex { .. }));
            let start = u64::from(pc) << 32 | u64::from(scan.half) ^ s2dex;
            scan.segments.iter().fold(start, |h, &s| (h ^ u64::from(s)).wrapping_mul(0x100_0000_01b3))
        };
        let k = key(scan, pc);
        if scan.seen.contains(&k) {
            return false;
        }
        scan.seen.push(k);
        let (call, end, load, branch_z, half, move_word, limit) =
            if scan.gbi2 { (0xde, 0xdf, 0xdd, 0x04, 0xe1, 0xdb, 18) } else { (0x06, 0xb8, 0xaf, 0xb0, 0xb4, 0xbc, 10) };
        while scan.budget > 0 {
            scan.budget -= 1;
            let at = pc as usize & 0x7f_fff8;
            let w0 = u32::from_be_bytes(self.rdram[at..at + 4].try_into().unwrap());
            let w1 = u32::from_be_bytes(self.rdram[at + 4..at + 8].try_into().unwrap());
            pc = pc.wrapping_add(8);
            let op = w0 >> 24;
            let s2dex = matches!(scan.ucode, Ucode::S2dex { .. });
            if op == load {
                let data = physical(scan.half);
                let known = self.hle.known.iter().find(|(at, _)| *at == data).map_or_else(
                    || self.recognise(data, (w0 & 0xfff) + 1), |&(_, ucode)| ucode);
                match known {
                    Some(ucode @ (Ucode::S2dex { .. } | Ucode::F3dex2 { .. })) if scan.gbi2 => scan.ucode = ucode,
                    _ => return true,
                }
            } else if s2dex && op == 0xe4 {
                scan.half0 = w0;
            } else if s2dex && op == 0x04 {
                // G_SELECT_DL: a call (or a branch) when the status says so, followed as a call.
                let target = scan.address(scan.half0 & 0xffff | w0 << 16);
                if depth < limit && self.scan_list(scan, target, depth + 1) {
                    return true;
                }
            } else if op == end {
                return false;
            } else if op == call {
                let target = scan.address(w1);
                if w0 >> 16 & 0xff != 0 {
                    // A branch: this list goes on there.
                    let k = key(scan, target);
                    if scan.seen.contains(&k) {
                        return false;
                    }
                    scan.seen.push(k);
                    pc = target;
                } else if depth < limit && self.scan_list(scan, target, depth + 1) {
                    return true;
                }
            } else if op == branch_z && !s2dex {
                let target = scan.address(scan.half);
                if depth < limit && self.scan_list(scan, target, depth + 1) {
                    return true;
                }
            } else if op == half {
                scan.half = w1;
            } else if op == move_word {
                let (index, offset) =
                    if scan.gbi2 { (w0 >> 16 & 0xff, w0 & 0xffff) } else { (w0 & 0xff, w0 >> 8 & 0xffff) };
                if index == 6 {
                    scan.segments[(offset >> 2 & 15) as usize] = w1;
                }
            }
        }
        false
    }

    /// The RDP reached a SYNC_FULL: its interrupt comes once everything before it is done. Sent by
    /// a task the RSP (or this) runs, it also waits for the RSP to stop, after the RSP's own: the
    /// RDP finishes what the microcode sent it after the microcode itself. Resident Evil 2's
    /// scheduler takes a DP interrupt before the SP one as a second task finished, and stops its
    /// frames. The wait is in the RDP's timing, so a state saved before the RSP stops keeps it.
    pub(crate) fn full_sync(&mut self) {
        let waits = self.hle.drawing || self.hle.from_rsp;
        self.rdp_full_sync(waits);
    }

    /// The RSP stopped (BREAK, or the CPU halted it): a SYNC_FULL's interrupt that waited for it.
    pub(crate) fn rsp_stopped(&mut self) {
        self.rdp_rsp_stopped();
    }

    /// Compares the last graphics task's commands with what this made of
    /// it: when the next starts, as some games send the RDP the commands
    /// the microcode left in RDRAM only after it has finished.
    pub(crate) fn finish_comparison(&mut self) {
        let Some(expected) = self.hle.expected.take() else { return };
        let actual = std::mem::take(&mut self.hle.captured);
        self.hle.capturing = false;
        compare(&mut self.hle.report, &expected, &actual);
    }

    /// The microcode of a task, by the version text in its data.
    fn identify(&mut self, task: &Task) -> Option<Ucode> {
        self.identify_data(task.ucode_data, task.ucode_data_size)
    }

    /// A microcode by its data in RDRAM (at most 4 KB of it), remembered by the data's address.
    fn identify_data(&mut self, at: u32, size: u32) -> Option<Ucode> {
        if let Some(&(_, ucode)) = self.hle.known.iter().find(|(data, _)| *data == at) {
            return ucode;
        }
        let ucode = self.recognise(at, size);
        self.hle.known.push((at, ucode));
        ucode
    }

    fn recognise(&self, at: u32, size: u32) -> Option<Ucode> {
        let start = at as usize;
        let end = (start + size.min(0x1000) as usize).min(self.rdram.len());
        let data = &self.rdram[start.min(end)..end];
        let has = |text: &[u8]| data.windows(text.len()).any(|w| w == text);
        if has(b"RSP SW Version: 2.0D") {
            Some(Ucode::F3d)
        } else if has(b"RSP SW Version: 2.0G") {
            Some(Ucode::F3dRare)
        } else if data.len() >= 0x200 && fnv(&data[..0x200]) == PERFECT_DARK_DATA {
            Some(Ucode::F3dPd)
        } else {
            gbi2(data).or_else(|| s2dex::recognise(data))
        }
    }

    /// G_LOAD_UCODE: the text at w1, the data at G_RDPHALF_1's address, of which DMEM takes what lies from
    /// 0x180 on ((w0 & 0xfff) + 1 bytes in all): what lies below (matrices, viewport, scissor, other modes,
    /// segments, the list's stack) stays. The new microcode goes on with the next command. False when this does
    /// not know it (the task's scan sent such tasks to the RSP).
    fn load_ucode(&mut self, gfx: &mut Gfx, w0: u32) -> bool {
        let (data, size) = (physical(gfx.half[0]), (w0 & 0xfff) + 1);
        match self.identify_data(data, size) {
            Some(ucode @ Ucode::S2dex { .. }) => self.s2dex_enter(gfx, ucode, data, size, false),
            Some(ucode @ Ucode::F3dex2 { .. }) => gfx.reenter(ucode),
            _ => return false,
        }
        true
    }

    /// Hands the RDP a command from the display list; the drawing thread
    /// gets them as they gather, as it does from DPC_END.
    fn send(&mut self, words: &[u64]) {
        if self.rdp_frozen() && !self.hle.capture_only {
            self.hle.held.push(words.to_vec());
            return;
        }
        self.rdp_command(words);
        self.rdp_thread.flush_some();
    }

    /// Runs a task's display list: about how many cycles the microcode's
    /// RSP would take over it.
    fn run_gfx(&mut self, ucode: Ucode, task: &Task) -> u64 {
        let mut gfx = Gfx::new(ucode);
        gfx.pc = task.data_ptr;
        self.load_defaults(&mut gfx, task);
        if let Ucode::S2dex { .. } = ucode {
            self.s2dex_enter(&mut gfx, ucode, task.ucode_data, task.ucode_data_size, true);
        } else if let Ucode::F3dex2 { .. } = ucode {
            // The scissor as the data has it (DMEM 0xc0), which S2DEX reads when loaded.
            let word = |at: u32| self.rdram_word(task.ucode_data + at);
            gfx.scissor = [word(0xc0), word(0xc4)];
        }
        // The modelview stack is kept in RDRAM, a matrix of 64 bytes each.
        gfx.stack_depth = (task.dram_stack_size as usize / 64).clamp(1, 64);
        // A display list that never ends stops after this many commands.
        for _ in 0..1 << 20 {
            let at = gfx.pc as usize & 0x7f_fff8;
            let w0 = u32::from_be_bytes(self.rdram[at..at + 4].try_into().unwrap());
            let w1 = u32::from_be_bytes(self.rdram[at + 4..at + 8].try_into().unwrap());
            gfx.pc = gfx.pc.wrapping_add(8);
            if let Ucode::S2dex { .. } = gfx.ucode {
                gfx.work.s2dex_commands += 1;
            } else {
                gfx.work.commands += 1;
            }
            if !self.gfx_command(&mut gfx, w0, w1) {
                break;
            }
        }
        self.rdp_thread.flush();
        gfx.work.rsp_cycles(gfx.model.unwrap_or(ucode))
    }

    /// A word of RDRAM at a physical address.
    fn rdram_word(&self, at: u32) -> u32 {
        let at = at as usize & 0x7f_fffc;
        u32::from_be_bytes(self.rdram[at..at + 4].try_into().unwrap())
    }

    /// One command of F3D's display list; false at the end of the task.
    fn gfx_command(&mut self, gfx: &mut Gfx, w0: u32, w1: u32) -> bool {
        if let Ucode::F3dex2 { .. } = gfx.ucode {
            return self.gbi2_command(gfx, w0, w1);
        }
        if let Ucode::S2dex { .. } = gfx.ucode {
            return self.s2dex_command(gfx, w0, w1);
        }
        match w0 >> 24 {
            0x01 => {
                let params = w0 >> 16 & 0xff;
                let addr = gfx.address(w1);
                if gfx.ucode == Ucode::F3dPd {
                    let m = self.read_fixed(addr);
                    gfx.pd_matrix(m, params & 1 != 0, params & 2 != 0);
                }
                let m = self.read_matrix(addr);
                gfx.matrix(m, params & 1 != 0, params & 2 != 0, params & 4 != 0);
            }
            0x03 => {
                let addr = gfx.address(w1);
                self.move_memory(gfx, w0 >> 16 & 0xff, addr);
            }
            0x04 => {
                let (first, n) = if gfx.ex() { ((w0 >> 16 & 0xff) / 2, w0 >> 10 & 0x3f) } else { (w0 >> 16 & 15, (w0 >> 20 & 15) + 1) };
                // Perfect Dark's 12-byte vertices: the DMA drops the address's low three bits, and
                // the microcode steps 4 bytes on when the list's address has bit 2.
                let addr = if gfx.ucode == Ucode::F3dPd { gfx.address(w1) & !7 | w1 & 4 } else { gfx.address(w1) };
                self.load_vertices(gfx, addr, first as usize, n as usize);
            }
            0x07 if gfx.ucode == Ucode::F3dPd => self.color_table(gfx, w0, w1),
            // Perfect Dark's G_POPMTX and G_CULLDL do nothing.
            0xbd | 0xbe if gfx.ucode == Ucode::F3dPd => {}
            0x06 => {
                if w0 >> 16 & 0xff == 0 {
                    if gfx.stack.len() >= 10 {
                        return true;
                    }
                    gfx.stack.push(gfx.pc);
                }
                gfx.pc = gfx.address(w1);
            }
            0xb2 if gfx.ex() => gfx.modify_vertex((w0 & 0xffff) as usize / 2, w0 >> 16 & 0xff, w1),
            0xb4 => gfx.half[0] = w1,
            // F3DEX: to the list in G_RDPHALF_1 when the vertex is nearer.
            0xb0 if gfx.ex() => self.branch_z(gfx, w0, w1),
            // Another microcode from here on: not followed.
            0xaf if gfx.ex() => return false,
            0xb6 => gfx.geometry &= !w1,
            0xb7 => gfx.geometry |= w1,
            0xb8 => match gfx.stack.pop() {
                Some(pc) => gfx.pc = pc,
                None => return false,
            },
            op @ (0xb9 | 0xba) => {
                let (shift, len) = (w0 >> 8 & 0xff, w0 & 0xff);
                let mask = (((1u64 << len) - 1) << shift) as u32;
                let mode = &mut gfx.other[usize::from(op == 0xb9)];
                *mode = *mode & !mask | w1 & mask;
                self.send(&[0xef00_0000u64 << 32 | u64::from(gfx.other[0] & 0x00ff_ffff) << 32
                    | u64::from(gfx.other[1])]);
            }
            0xbb => {
                gfx.texture = Texture {
                    on: w0 & 0xff != 0,
                    tile: w0 >> 8 & 7,
                    level: w0 >> 11 & 7,
                    scale: [(w1 >> 16) as f32 / 65536.0, (w1 & 0xffff) as f32 / 65536.0],
                };
            }
            0xbc => gfx.move_word(w0 & 0xff, w0 >> 8 & 0xffff, w1),
            0xbd => {
                if gfx.modelview.len() > 1 {
                    gfx.modelview.pop();
                    gfx.combined = None;
                }
            }
            0xbe => {
                let step = if gfx.ex() { 2 } else { 40 };
                if !gfx.visible((w0 & 0xffff) / step, (w1 & 0xffff) / step) {
                    match gfx.stack.pop() {
                        Some(pc) => gfx.pc = pc,
                        None => return false,
                    }
                }
            }
            0xbf if gfx.ex() => {
                let index = |shift: u32| (w1 >> shift & 0xff) as usize / 2;
                self.triangle(gfx, [index(16), index(8), index(0)], 0);
            }
            0xbf => {
                let index = |shift: u32| (w1 >> shift & 0xff) as usize / 10;
                self.triangle(gfx, [index(16), index(8), index(0)], (w1 >> 24) as usize);
            }
            0xb1 if gfx.ex() => {
                let index = |w: u32, shift: u32| (w >> shift & 0xff) as usize / 2;
                self.triangle(gfx, [index(w0, 16), index(w0, 8), index(w0, 0)], 0);
                self.triangle(gfx, [index(w1, 16), index(w1, 8), index(w1, 0)], 0);
            }
            // G_LINE3D as a quadrangle: two triangles, 1 2 3 and 0 1 3 (the
            // split most of the microcode's take; it splits some others
            // along the other diagonal, which only a bent quadrangle shows).
            0xb5 if gfx.ex() => {
                let v = [24, 16, 8, 0].map(|shift| (w1 >> shift & 0xff) as usize / 2);
                self.triangle(gfx, [v[1], v[2], v[3]], 0);
                self.triangle(gfx, [v[0], v[1], v[3]], 0);
            }
            // Rare's: up to four triangles, four bits a vertex, the first
            // of each in w0 and the other two in w1 (drawn from the second,
            // as the microcode's edges show).
            0xb1 if gfx.rare() => {
                for k in 0..4 {
                    let v = [(w1 >> (8 * k) & 15) as usize, (w1 >> (8 * k + 4) & 15) as usize, (w0 >> (4 * k) & 15) as usize];
                    if v[0] != v[1] || v[1] != v[2] {
                        self.triangle(gfx, v, 0);
                    }
                }
            }
            // Texture Rectangle goes on in the next two commands' second
            // words (s and t, then dsdx and dtdy), which the microcode takes
            // as they come, whatever their opcodes: F3D's lists have
            // G_RDPHALF_2 and G_RDPHALF_CONT there, F3DEX's G_RDPHALF_1 and
            // G_RDPHALF_2, and Mario Kart 64's F3DEX 0.95 lists F3D's pair,
            // whose 0xb2 is G_MODIFYVTX to F3DEX.
            0xe4 | 0xe5 => {
                let next = |n64: &N64, n: u32| {
                    let at = gfx.pc.wrapping_add(8 * n + 4) as usize & 0x7f_fffc;
                    u32::from_be_bytes(n64.rdram[at..at + 4].try_into().unwrap())
                };
                let (st, slopes) = (next(self, 0), next(self, 1));
                gfx.pc = gfx.pc.wrapping_add(16);
                gfx.work.commands += 2;
                self.send(&[u64::from(w0) << 32 | u64::from(w1), u64::from(st) << 32 | u64::from(slopes)]);
            }
            op @ 0xc0..=0xff => self.rdp_passthrough(gfx, op, w0, w1),
            _ => {}
        }
        true
    }

    /// One command of F3DEX2's display list (GBI2: its own opcodes, vertex
    /// indices doubled, 24 bytes between lights); false at the end of the
    /// task.
    fn gbi2_command(&mut self, gfx: &mut Gfx, w0: u32, w1: u32) -> bool {
        let index = |w: u32, shift: u32| (w >> shift & 0xff) as usize / 2;
        match w0 >> 24 {
            0x01 => {
                let n = (w0 >> 12 & 0xff) as usize;
                let end = (w0 >> 1 & 0x7f) as usize;
                let addr = gfx.address(w1);
                self.load_vertices(gfx, addr, end.saturating_sub(n), n);
            }
            0x02 => gfx.modify_vertex((w0 & 0xffff) as usize / 2, w0 >> 16 & 0xff, w1),
            0x03 => {
                if !gfx.visible((w0 & 0xffff) / 2, (w1 & 0xffff) / 2) {
                    match gfx.stack.pop() {
                        Some(pc) => gfx.pc = pc,
                        None => return false,
                    }
                }
            }
            0x04 => self.branch_z(gfx, w0, w1),
            0x05 => self.triangle(gfx, [index(w0, 16), index(w0, 8), index(w0, 0)], 0),
            // Two triangles (G_TRI2, and G_QUAD as two), the second
            // first, as the microcode's commands come (the .Rej ones' the first first).
            0x06 | 0x07 => {
                let first = [index(w0, 16), index(w0, 8), index(w0, 0)];
                let second = [index(w1, 16), index(w1, 8), index(w1, 0)];
                if let Ucode::F3dex2 { reject: true, .. } = gfx.ucode {
                    self.triangle(gfx, first, 0);
                    self.triangle(gfx, second, 0);
                } else {
                    self.triangle(gfx, second, 0);
                    self.triangle(gfx, first, 0);
                }
            }
            0xd7 => {
                gfx.texture = Texture {
                    on: w0 >> 1 & 0x7f != 0,
                    tile: w0 >> 8 & 7,
                    level: w0 >> 11 & 7,
                    scale: [(w1 >> 16) as f32 / 65536.0, (w1 & 0xffff) as f32 / 65536.0],
                };
            }
            0xd8 => {
                for _ in 0..w1 / 64 {
                    if gfx.modelview.len() > 1 {
                        gfx.modelview.pop();
                    }
                }
                gfx.combined = None;
            }
            0xd9 => {
                gfx.geometry2 = gfx.geometry2 & w0 & 0x00ff_ffff | w1;
                gfx.geometry = geometry_of_gbi2(gfx.geometry2);
            }
            // The push bit is inverted.
            0xda => {
                let params = (w0 & 0xff) ^ 1;
                let m = self.read_matrix(gfx.address(w1));
                gfx.matrix(m, params & 4 != 0, params & 2 != 0, params & 1 != 0);
            }
            0xdb => gfx.move_word2(w0 >> 16 & 0xff, w0 & 0xffff, w1),
            0xdc => {
                let addr = gfx.address(w1);
                self.move_memory2(gfx, w0 & 0xff, (w0 >> 8 & 0xff) * 8, addr);
            }
            0xde => {
                if w0 >> 16 & 0xff == 0 {
                    if gfx.stack.len() >= 18 {
                        return true;
                    }
                    gfx.stack.push(gfx.pc);
                }
                gfx.pc = gfx.address(w1);
            }
            0xdf => match gfx.stack.pop() {
                Some(pc) => gfx.pc = pc,
                None => return false,
            },
            0xdd => return self.load_ucode(gfx, w0),
            0xe1 => gfx.half[0] = w1,
            // Texture Rectangle goes on in G_RDPHALF_1 (s, t) and
            // G_RDPHALF_2 (dsdx, dtdy).
            0xf1 => {
                gfx.half[1] = w1;
                if let Some(first) = gfx.texrect.take() {
                    self.send(&[first, u64::from(gfx.half[0]) << 32 | u64::from(gfx.half[1])]);
                }
            }
            op @ (0xe2 | 0xe3) => {
                let len = (w0 & 0xff) + 1;
                let shift = 32u32.saturating_sub((w0 >> 8 & 0xff) + len);
                let mask = (((1u64 << len) - 1) << shift) as u32;
                let mode = &mut gfx.other[usize::from(op == 0xe2)];
                *mode = *mode & !mask | w1 & mask;
                self.send(&[0xef00_0000u64 << 32 | u64::from(gfx.other[0] & 0x00ff_ffff) << 32
                    | u64::from(gfx.other[1])]);
            }
            0xe4 | 0xe5 => gfx.texrect = Some(u64::from(w0) << 32 | u64::from(w1)),
            op @ 0xe6..=0xff => self.rdp_passthrough(gfx, op, w0, w1),
            _ => {}
        }
        true
    }

    /// An RDP command as it is, with segmented image addresses made
    /// physical; the microcode keeps the other modes it sets, as its own
    /// commands change them a field at a time.
    fn rdp_passthrough(&mut self, gfx: &mut Gfx, op: u32, w0: u32, w1: u32) {
        let w1 = if matches!(op, 0xfd..=0xff) { gfx.address(w1) } else { w1 };
        if op == 0xef {
            gfx.other = [w0 & 0x00ff_ffff, w1];
        } else if op == 0xed {
            gfx.scissor = [w0, w1];
        }
        self.send(&[u64::from(w0) << 32 | u64::from(w1)]);
    }

    /// G_BRANCH_Z: to the list in G_RDPHALF_1 when the vertex is nearer
    /// than w1 (on screen, 16.16 of the viewport's depth).
    fn branch_z(&mut self, gfx: &mut Gfx, w0: u32, w1: u32) {
        let v = &gfx.vertices[((w0 & 0xfff) as usize / 2).min(gfx.vertices.len() - 1)];
        if (f64::from(v.screen.z) / 32.0 * 65536.0) as i64 <= i64::from(w1 as i32) {
            gfx.pc = gfx.address(gfx.half[0]);
        }
    }

    /// F3DEX2's G_MOVEMEM: the viewport, and lights (24 bytes apart: the
    /// two lookat ones, then the lights).
    fn move_memory2(&mut self, gfx: &mut Gfx, index: u32, offset: u32, addr: u32) {
        match index {
            8 => self.move_memory(gfx, 0x80, addr),
            // G_MV_MATRIX: the combined matrix, used as it is while G_MW_FORCEMTX says it is up to
            // date (Episode I: Racer).
            14 if offset == 0 => {
                gfx.mvp = self.read_matrix(addr);
                if gfx.combined.is_some() {
                    gfx.combined = Some(gfx.mvp);
                }
            }
            10 => match offset / 24 {
                0 => self.move_memory(gfx, 0x84, addr),
                1 => self.move_memory(gfx, 0x82, addr),
                n => self.move_memory(gfx, 0x86 + 2 * (n - 2).min(7), addr),
            },
            _ => {}
        }
    }

    /// F3D's and F3DEX 1.x's state as their data starts it, where G_MOVEMEM
    /// finds it (a table of DMEM addresses at 0x2f0, Perfect Dark's at 0x230:
    /// the viewport, the two lookat lights, then the lights). Games may draw
    /// lit without setting lights: the default ambient light is red.
    fn load_defaults(&self, gfx: &mut Gfx, task: &Task) {
        if matches!(gfx.ucode, Ucode::F3dex2 { .. }) || task.ucode_data_size < 0x320 {
            return;
        }
        let data = task.ucode_data as usize;
        let byte = |at: usize| self.rdram[(data + at) & 0x7f_ffff];
        let table = if gfx.ucode == Ucode::F3dPd { 0x230 } else { 0x2f0 };
        let entry = |k: usize| usize::from(u16::from_be_bytes([byte(table + 2 * k), byte(table + 1 + 2 * k)]));
        let size = task.ucode_data_size as usize;
        if gfx.ucode == Ucode::F3dPd {
            // The colour table's DMEM as the data leaves it.
            for (i, color) in gfx.colors.iter_mut().enumerate() {
                *color = if PD_COLORS + i < size { byte(PD_COLORS + i) } else { 0 };
            }
        }
        let read = |at: usize| (at + 16 <= size).then(|| {
            let direction = [8, 9, 10].map(|i| f32::from(byte(at + i) as i8));
            Light { color: [0, 1, 2].map(|i| f32::from(byte(at + i))), dir: normalize(direction) }
        });
        if let (Some(y), Some(x)) = (read(entry(1)), read(entry(2))) {
            gfx.lookat = [x.dir, y.dir];
        }
        for (n, light) in gfx.lights.iter_mut().enumerate() {
            if let Some(default) = read(entry(3 + n)) {
                *light = default;
            }
        }
    }

    /// A Mtx as it is, in s15.16.
    fn read_fixed(&self, addr: u32) -> Fixed {
        let at = addr as usize & 0x7f_fff8;
        let half = |i: usize| u16::from_be_bytes([self.rdram[at + 2 * i], self.rdram[at + 2 * i + 1]]);
        let fixed = |i: usize| (u32::from(half(i)) << 16 | u32::from(half(16 + i))) as i32;
        std::array::from_fn(|r| std::array::from_fn(|c| fixed(r * 4 + c)))
    }

    /// A Mtx: sixteen 16-bit integer parts, then sixteen fractions.
    fn read_matrix(&self, addr: u32) -> Matrix {
        let at = addr as usize & 0x7f_fff8;
        let half = |i: usize| u16::from_be_bytes([self.rdram[at + 2 * i], self.rdram[at + 2 * i + 1]]);
        std::array::from_fn(|r| {
            std::array::from_fn(|c| {
                let i = r * 4 + c;
                (i32::from(half(i)) << 16 | i32::from(half(16 + i))) as f32 / 65536.0
            })
        })
    }

    fn move_memory(&mut self, gfx: &mut Gfx, index: u32, addr: u32) {
        let at = addr as usize & 0x7f_fff8;
        let byte = |i: usize| self.rdram[(at + i) & 0x7f_ffff];
        let half = |i: usize| i16::from_be_bytes([byte(i), byte(i + 1)]);
        match index {
            0x80 => {
                // x and y in quarter pixels, z as is.
                gfx.viewport = std::array::from_fn(|i| f32::from(half(2 * i)) / if i % 4 < 2 { 4.0 } else { 1.0 });
            }
            0x82 | 0x84 => {
                let n = usize::from(index == 0x82);
                gfx.lookat[n] = normalize([0, 1, 2].map(|i| f32::from(byte(8 + i) as i8)));
            }
            0x86..=0x94 => {
                let n = ((index - 0x86) / 2) as usize;
                // Perfect Dark's microcode has room for two: the others' address is the first's.
                let n = if gfx.ucode == Ucode::F3dPd && n > 1 { 0 } else { n };
                let gbi2 = matches!(gfx.ucode, Ucode::F3dex2 { .. });
                let direction = [0, 1, 2].map(|i| f32::from(byte(8 + i) as i8));
                gfx.lights[n] = Light {
                    color: [0, 1, 2].map(|i| f32::from(byte(i))),
                    // F3DEX2 takes the direction as it is (in 128ths).
                    dir: if gbi2 { direction.map(|d| d / 128.0) } else { normalize(direction) },
                };
            }
            _ => {}
        }
    }

    fn load_vertices(&mut self, gfx: &mut Gfx, addr: u32, first: usize, n: usize) {
        let combined = gfx.combined();
        let modelview = *gfx.modelview.last().unwrap();
        let size = gfx.vertices.len();
        // Perfect Dark's are 12 bytes, with the offset of their colour in the colour table at 7.
        let pd = gfx.ucode == Ucode::F3dPd;
        let (stride, align) = if pd { (12, 0x7f_fffc) } else { (16, 0x7f_fff8) };
        let lit = gfx.geometry & LIGHTING != 0;
        let tex_gen = lit && gfx.geometry & TEXTURE_GEN != 0;
        let tex_gen_linear = gfx.geometry & TEXTURE_GEN_LINEAR != 0;
        let rare = gfx.ucode == Ucode::F3dRare && !tex_gen;
        let tex_scale = gfx.texture.scale;
        let clip_ratio = gfx.clip_ratio;
        let near = gfx.near();
        let fog_enabled = gfx.geometry & FOG != 0;
        let fog_factors = [f32::from(gfx.fog[0]), f32::from(gfx.fog[1])];
        let ambient = if lit { gfx.lights[gfx.lights_on].color } else { [0.0; 3] };
        for i in 0..n.min(size - first.min(size)) {
            let at = (addr as usize + stride * i) & align;
            let Some(v) = self.rdram.get(at..at + stride) else { break };
            let half = |k: usize| i16::from_be_bytes([v[2 * k], v[2 * k + 1]]);
            let clip = transform_pos(f32::from(half(0)), f32::from(half(1)), f32::from(half(2)), &combined);
            let mut vertex = Vertex { clip, ..Vertex::default() };
            let rgba: [u8; 4] = if pd {
                let c = &gfx.colors[usize::from(v[7])..];
                [c[0], c[1], c[2], c[3]]
            } else {
                [v[12], v[13], v[14], v[15]]
            };
            let normal = [rgba[0] as i8, rgba[1] as i8, rgba[2] as i8].map(f32::from);
            vertex.color = if lit {
                let eye = normalize(rotate(&normal, &modelview));
                let mut color = ambient;
                for light in &gfx.lights[..gfx.lights_on] {
                    let intensity = dot(&eye, &light.dir).max(0.0);
                    for (c, lit) in color.iter_mut().zip(light.color) {
                        *c += lit * intensity;
                    }
                }
                [color[0].min(255.0), color[1].min(255.0), color[2].min(255.0), f32::from(rgba[3])]
            } else {
                rgba.map(f32::from)
            };
            let (mut s, mut t) = (f32::from(half(4)), f32::from(half(5)));
            if tex_gen {
                let eye = normalize(rotate(&normal, &modelview));
                let (x, y) = (dot(&gfx.lookat[0], &eye), dot(&gfx.lookat[1], &eye));
                if tex_gen_linear {
                    (s, t) = ((-x).acos() * 325.949_3 * 32.0, (-y).acos() * 325.949_3 * 32.0);
                } else {
                    (s, t) = ((x + 1.0) * 512.0 * 32.0, (y + 1.0) * 512.0 * 32.0);
                }
            }
            vertex.st = if rare {
                [t * tex_scale[1] * 32.0, s * tex_scale[0] * 32.0]
            } else {
                [s * tex_scale[0], t * tex_scale[1]]
            };
            if pd {
                let m = &gfx.fixed[2];
                let clip: [i32; 4] = std::array::from_fn(|c| {
                    saturate((0..3).map(|k| i64::from(half(k)) * i64::from(m[k][c])).sum::<i64>() + i64::from(m[3][c]))
                });
                vertex.clip = clip.map(|c| c as f32 / 65536.0);
                gfx.pd_finish(&mut vertex, &clip, i % 2 == 1);
                gfx.work.vertices += 1;
                gfx.vertices[first + i] = vertex;
                continue;
            }
            let [_, _, z, w] = clip;
            if fog_enabled && w != 0.0 {
                let fog = (z / w) * fog_factors[0] + fog_factors[1];
                vertex.color[3] = fog.clamp(0.0, 255.0);
            }
            (vertex.codes, vertex.cull) = outside_pair(&clip, clip_ratio, near);
            vertex.screen = gfx.project(&vertex);
            gfx.work.vertices += 1;
            gfx.vertices[first + i] = vertex;
        }
    }

    /// A triangle of the vertex buffer, `flat` the vertex whose color a
    /// flat-shaded one takes.
    fn triangle(&mut self, gfx: &mut Gfx, v: [usize; 3], flat: usize) {
        gfx.work.triangles += 1;
        let v = v.map(|i| i.min(gfx.vertices.len() - 1));
        if gfx.ucode == Ucode::F3dPd {
            return self.pd_triangle(gfx, v);
        }
        let [a, b, c] = v.map(|i| &gfx.vertices[i]);
        // All three outside the same edge of the screen (or the near or
        // far plane): nothing to draw. (Not the clipping planes: what lies
        // between the screen and them is clipped only when it shows.)
        if a.cull & b.cull & c.cull != 0 {
            return;
        }
        let flat = (gfx.geometry & SHADING_SMOOTH == 0).then(|| gfx.vertices[v[flat.min(2)]].color);
        // Inside: as loaded.
        if a.codes | b.codes | c.codes == 0 {
            let mut screen = [a.screen, b.screen, c.screen];
            if let Some(color) = flat {
                screen.iter_mut().for_each(|corner| corner.color = color);
            }
            if !culled(gfx, area(&screen)) {
                self.send_triangle(gfx, screen);
            }
            return;
        }
        if let Ucode::F3dex2 { reject: true, .. } = gfx.ucode {
            return;
        }
        self.clipped_triangle(gfx, v, flat);
    }

    /// A triangle partly outside, with its vertices and its flat color if
    /// it has one (out of line: most are inside).
    #[inline(never)]
    fn clipped_triangle(&mut self, gfx: &mut Gfx, v: [usize; 3], flat: Option<[f32; 4]>) {
        // Partly outside: clipped in clip space against the sides (at the
        // clip ratio) and the near plane, each of which adds a corner at
        // most, in two arrays taking turns.
        let mut corners = [[gfx.vertices[v[0]]; 8]; 2];
        for (k, &i) in v.iter().enumerate() {
            corners[0][k] = gfx.vertices[i];
            if let Some(color) = flat {
                corners[0][k].color = color;
            }
        }
        // Which of the two holds the polygon, and its corners.
        let (mut from, mut len) = (0, 3);
        let r = gfx.clip_ratio;
        // The near plane first: behind it w goes negative, and the
        // sides' tests with it. Without near clipping, the eye's plane.
        let near: fn(&[f32; 4], f32) -> f32 = if gfx.near() { |c, _| c[2] + c[3] } else { |c, _| c[3] - BEHIND };
        let planes: [fn(&[f32; 4], f32) -> f32; 5] = [
            near,
            |c, r| c[0] + r * c[3],
            |c, r| r * c[3] - c[0],
            |c, r| c[1] + r * c[3],
            |c, r| r * c[3] - c[1],
        ];
        for plane in planes {
            // Each edge from the vertex before: where it crosses the
            // plane, then the vertex if it is inside (the microcode's
            // order, which its triangles show).
            let [first, second] = &mut corners;
            let (polygon, out) = if from == 0 { (&*first, second) } else { (&*second, first) };
            let mut n = 0;
            for i in 0..len {
                let (a, b) = (&polygon[(i + len - 1) % len], &polygon[i]);
                let (da, db) = (plane(&a.clip, r), plane(&b.clip, r));
                if (da >= 0.0) != (db >= 0.0) {
                    out[n] = a.lerp(b, da / (da - db));
                    n += 1;
                }
                if db >= 0.0 {
                    out[n] = *b;
                    n += 1;
                }
            }
            from ^= 1;
            len = n;
            if len < 3 {
                return;
            }
        }
        let polygon = &corners[from];
        let screen: [Screen; 8] = std::array::from_fn(|i| if i < len { gfx.project(&polygon[i]) } else { Screen::default() });
        let screen = &screen[..len];
        // Facing, by the polygon's winding on screen (y down), as the
        // vertices came; then a fan from the first vertex, the last
        // triangle first.
        if culled(gfx, area(screen)) {
            return;
        }
        // The microcode's order, which decides ties in y: each fan
        // triangle from its own two vertices back to the first.
        for i in (1..len - 1).rev() {
            self.send_triangle(gfx, [screen[i], screen[i + 1], screen[0]]);
        }
    }

    /// Perfect Dark's G_VTXCOLORBASE: the table its vertices' colours (or normals) are in, copied
    /// into DMEM a word at a time from the offset in w0's second byte down to 0, as the DMA brought
    /// it (from the address with its low three bits dropped). A smaller table leaves the rest.
    fn color_table(&mut self, gfx: &mut Gfx, w0: u32, w1: u32) {
        let at = gfx.address(w1) as usize & 0x7f_fff8;
        let end = (w0 >> 16 & 0xff) as usize + 4;
        for (i, color) in gfx.colors[..end].iter_mut().enumerate() {
            *color = self.rdram[(at + i) & 0x7f_ffff];
        }
    }

    /// A triangle as Perfect Dark's microcode (Rare's F3D) draws it, always smooth shaded. It does
    /// not clip at the near plane: what lies nearer is drawn at depth 0. It rejects a triangle whose
    /// three vertices lie beyond the same screen edge or the far plane, and clips one whose codes ask
    /// for it, in clip space, against the eye's plane (w = 0), the far one, then the sides at the
    /// clip ratio (y and x high, then low). A plane cuts the polygon by its vertices' codes: the
    /// list goes on from where it comes back in, the cut there first, the inside vertices, then the
    /// cut where it leaves. The polygon is a fan from the list's first vertex, first triangle first.
    #[inline(never)]
    fn pd_triangle(&mut self, gfx: &mut Gfx, v: [usize; 3]) {
        let [a, b, c] = v.map(|i| &gfx.vertices[i]);
        if a.cull & b.cull & c.cull != 0 {
            return;
        }
        if a.codes | b.codes | c.codes == 0 {
            let screen = [a.screen, b.screen, c.screen];
            if !culled(gfx, area(&screen)) {
                self.send_triangle(gfx, screen);
            }
            return;
        }
        // The list starts from the last vertex, two lists taking turns.
        let mut lists = [[gfx.vertices[v[2]]; 9]; 2];
        (lists[0][1], lists[0][2]) = (gfx.vertices[v[1]], gfx.vertices[v[0]]);
        let (mut from, mut len) = (0, 3);
        let r = gfx.clip_ratio;
        let planes: [(u8, Plane); 6] = [
            (PD_BEHIND, |c, _| c[3]),
            (PD_FAR, |c, _| c[3] - c[2]),
            (8, |c, r| r * c[3] - c[1]),
            (2, |c, r| r * c[3] - c[0]),
            (4, |c, r| c[1] + r * c[3]),
            (1, |c, r| c[0] + r * c[3]),
        ];
        for (code, plane) in planes {
            let [first, second] = &mut lists;
            let (polygon, out) = if from == 0 { (&first[..len], second) } else { (&second[..len], first) };
            let outside = |i: usize| polygon[i % len].codes & code != 0;
            let Some(gone) = (0..len).find(|&i| outside(i)) else { continue };
            // Where it comes back in, after the first vertex outside.
            let Some(back) = (gone + 1..gone + len).find(|&i| !outside(i)) else { return };
            let cut = |inside: &Vertex, outside: &Vertex| {
                let (d_in, d_out) = (plane(&inside.clip, r), plane(&outside.clip, r));
                let f = if d_in != d_out { (d_in / (d_in - d_out)).clamp(0.0, 1.0) } else { 0.0 };
                let mut vertex = inside.lerp(outside, f);
                let clip = vertex.clip.map(|c| (c * 65536.0).floor() as i32);
                gfx.pd_finish(&mut vertex, &clip, false);
                vertex
            };
            out[0] = cut(&polygon[back % len], &polygon[(back - 1) % len]);
            let mut n = 1;
            let mut i = back;
            while !outside(i) {
                out[n] = polygon[i % len];
                (n, i) = (n + 1, i + 1);
            }
            out[n] = cut(&polygon[(i - 1) % len], &polygon[i % len]);
            (from, len) = (from ^ 1, n + 1);
        }
        let polygon = &lists[from];
        for k in 0..len - 2 {
            let tri = [polygon[k + 2].screen, polygon[k + 1].screen, polygon[0].screen];
            if !culled(gfx, area(&tri)) {
                self.send_triangle(gfx, tri);
            }
        }
    }

    /// One triangle to the RDP, unless it has no area (on the quarter
    /// pixels the vertices are kept in), which the microcode leaves out.
    fn send_triangle(&mut self, gfx: &mut Gfx, tri: [Screen; 3]) {
        let [a, b, c] = tri;
        if (b.x - a.x) * (c.y - a.y) == (c.x - a.x) * (b.y - a.y) {
            return;
        }
        let words = setup(gfx, &tri);
        gfx.work.sent += 1;
        self.send(&words);
    }
}

/// Whether the geometry mode culls a polygon of this area (`area`).
fn culled(gfx: &Gfx, area: f32) -> bool {
    (gfx.geometry & CULL_BACK != 0 && area > 0.0) || (gfx.geometry & CULL_FRONT != 0 && area < 0.0)
}

/// Twice a polygon's area on screen, positive when its vertices go
/// clockwise (y down).
fn area(screen: &[Screen]) -> f32 {
    (1..screen.len() - 1)
        .map(|i| {
            let (a, b, c) = (screen[0], screen[i], screen[i + 1]);
            (b.x - a.x) * (c.y - a.y) - (c.x - a.x) * (b.y - a.y)
        })
        .sum()
}

/// F3DEX2's geometry mode as F3D has its bits (culling and smooth shading
/// move).
fn geometry_of_gbi2(mode: u32) -> u32 {
    let same = ZBUFFER | SHADE | FOG | LIGHTING | TEXTURE_GEN | TEXTURE_GEN_LINEAR;
    mode & same
        | if mode & 0x200 != 0 { CULL_FRONT } else { 0 }
        | if mode & 0x400 != 0 { CULL_BACK } else { 0 }
        | if mode & 0x20_0000 != 0 { SHADING_SMOOTH } else { 0 }
}

// Geometry mode bits, as F3D has them.
const ZBUFFER: u32 = 0x1;
const SHADE: u32 = 0x4;
const SHADING_SMOOTH: u32 = 0x200;
const CULL_FRONT: u32 = 0x1000;
const CULL_BACK: u32 = 0x2000;
const FOG: u32 = 0x1_0000;
const LIGHTING: u32 = 0x2_0000;
const TEXTURE_GEN: u32 = 0x4_0000;
const TEXTURE_GEN_LINEAR: u32 = 0x8_0000;

type Matrix = [[f32; 4]; 4];

/// A matrix as the microcode keeps it, in s15.16.
type Fixed = [[i32; 4]; 4];

fn saturate(v: i64) -> i32 {
    v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// Two s15.16 numbers multiplied as the vector unit's VMUDL, VMADM, VMADN and VMADH do: the
/// fraction floored, the product saturated.
fn fixed_mul(a: i32, b: i32) -> i32 {
    saturate((i64::from(a) * i64::from(b)) >> 16)
}

/// a then b in s15.16, as the microcode multiplies them: each product floored, the sum saturated.
fn fixed_multiply(a: &Fixed, b: &Fixed) -> Fixed {
    let sum = |r: usize, c: usize| (0..4).map(|k| (i64::from(a[r][k]) * i64::from(b[k][c])) >> 16).sum();
    std::array::from_fn(|r| std::array::from_fn(|c| saturate(sum(r, c))))
}

const IDENTITY: Matrix = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];

/// a then b, for row vectors (v × a × b).
#[inline(always)]
fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    [
        transform(&a[0], b),
        transform(&a[1], b),
        transform(&a[2], b),
        transform(&a[3], b),
    ]
}

#[inline(always)]
fn transform(v: &[f32; 4], m: &Matrix) -> [f32; 4] {
    let [v0, v1, v2, v3] = *v;
    [
        v0 * m[0][0] + v1 * m[1][0] + v2 * m[2][0] + v3 * m[3][0],
        v0 * m[0][1] + v1 * m[1][1] + v2 * m[2][1] + v3 * m[3][1],
        v0 * m[0][2] + v1 * m[1][2] + v2 * m[2][2] + v3 * m[3][2],
        v0 * m[0][3] + v1 * m[1][3] + v2 * m[2][3] + v3 * m[3][3],
    ]
}

#[inline(always)]
fn transform_pos(x: f32, y: f32, z: f32, m: &Matrix) -> [f32; 4] {
    [
        x * m[0][0] + y * m[1][0] + z * m[2][0] + m[3][0],
        x * m[0][1] + y * m[1][1] + z * m[2][1] + m[3][1],
        x * m[0][2] + y * m[1][2] + z * m[2][2] + m[3][2],
        x * m[0][3] + y * m[1][3] + z * m[2][3] + m[3][3],
    ]
}

#[inline(always)]
fn rotate(v: &[f32; 3], m: &Matrix) -> [f32; 3] {
    let [v0, v1, v2] = *v;
    [
        v0 * m[0][0] + v1 * m[1][0] + v2 * m[2][0],
        v0 * m[0][1] + v1 * m[1][1] + v2 * m[2][1],
        v0 * m[0][2] + v1 * m[1][2] + v2 * m[2][2],
    ]
}

fn dot(a: &[f32; 3], b: &[f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let length = dot(&v, &v).sqrt();
    if length > 0.0 { v.map(|c| c / length) } else { v }
}

#[derive(Clone, Copy, Default)]
struct Light {
    color: [f32; 3],
    dir: [f32; 3],
}

#[derive(Clone, Copy, Default)]
struct Texture {
    on: bool,
    tile: u32,
    level: u32,
    scale: [f32; 2],
}

#[derive(Clone, Copy, Default)]
struct Vertex {
    clip: [f32; 4],
    color: [f32; 4],
    /// Texture coordinates, s10.5 as the RDP counts them.
    st: [f32; 2],
    /// Outside x < -rw, x > rw, y < -rw, y > rw (r the clip ratio), and
    /// the near plane: what is clipped.
    codes: u8,
    /// Outside x < -w, x > w, y < -w, y > w, the near plane and the far
    /// one (z > w): what rejects triangles, and F3DEX2's G_CULLDL.
    cull: u8,
    /// On screen, as loaded (as the microcode keeps it).
    screen: Screen,
}

impl Vertex {
    /// Between two vertices in clip space (not yet on screen).
    fn lerp(&self, other: &Vertex, f: f32) -> Vertex {
        let mix = |a: f32, b: f32| a + (b - a) * f;
        Vertex {
            clip: std::array::from_fn(|i| mix(self.clip[i], other.clip[i])),
            color: std::array::from_fn(|i| mix(self.color[i], other.color[i])),
            st: std::array::from_fn(|i| mix(self.st[i], other.st[i])),
            codes: 0,
            cull: 0,
            screen: Screen::default(),
        }
    }
}

/// A vertex on screen: pixels (y down), depth, 1/w, color and s, t.
#[derive(Clone, Copy, Default)]
struct Screen {
    x: f32,
    y: f32,
    z: f32,
    inv_w: f32,
    color: [f32; 4],
    st: [f32; 2],
}

/// What a task made the microcode do, for how long it would take.
#[derive(Default)]
struct Work {
    commands: u64,
    vertices: u64,
    triangles: u64,
    /// Triangles that reached the RDP.
    sent: u64,
    /// S2DEX's commands, and its RSP cycles beside them (s2dex.rs).
    s2dex_commands: u64,
    s2dex: u64,
}

impl Work {
    /// The RSP's cycles, as fitted to the microcodes' own over the scenes of
    /// five games (within 8%, Paper Mario's within 15%; F3DEX 1.x over Mario
    /// Kart 64's one): F3D sets a triangle up slowly, F3DEX2 six times as
    /// fast. Perfect Dark's over six of its scenes (each within 2%, leaving
    /// out the time its microcode waits for the RDP the game keeps frozen).
    fn rsp_cycles(&self, ucode: Ucode) -> u64 {
        self.s2dex_cycles() + match ucode {
            Ucode::F3d | Ucode::F3dRare => 54 * self.commands + 61 * self.vertices + 740 * self.sent,
            Ucode::F3dPd => 82 * self.commands + 74 * self.vertices + 157 * self.triangles + 321 * self.sent,
            Ucode::F3dex { .. } => 43 * self.commands + 86 * self.vertices + 43 * self.triangles + 333 * self.sent,
            Ucode::F3dex2 { .. } | Ucode::S2dex { .. } => {
                32 * self.commands + 64 * self.vertices + 32 * self.triangles + 121 * self.sent
            }
        }
    }
}

struct Gfx {
    ucode: Ucode,
    work: Work,
    pc: u32,
    stack: Vec<u32>,
    segments: [u32; 16],
    modelview: Vec<Matrix>,
    projection: Matrix,
    /// The combined matrix while it is up to date (made from the two when needed).
    combined: Option<Matrix>,
    /// Perfect Dark's table of vertex colours in DMEM (G_VTXCOLORBASE), and the bytes after it a
    /// colour's offset may reach.
    colors: [u8; 0x104],
    /// Perfect Dark's modelview, projection and combined matrix as its microcode keeps them, and
    /// its perspective normalisation (G_MW_PERSPNORM: w is scaled by it, in 65536ths, before its
    /// reciprocal is taken, and x, y and z over w after).
    fixed: [Fixed; 3],
    persp_norm: u16,
    /// What F3DEX2 keeps as the combined matrix in DMEM (0x80): the last one made, or one put there
    /// with G_MOVEMEM (gSPForceMatrix, with G_MW_FORCEMTX saying it is up to date).
    mvp: Matrix,
    /// The viewport's scale and translation, x, y and z (in pixels).
    viewport: [f32; 8],
    lights: [Light; 8],
    lights_on: usize,
    lookat: [[f32; 3]; 2],
    /// 16 for F3D, 32 for F3DEX2 (which keeps 64 of room).
    vertices: [Vertex; 64],
    /// The geometry mode as F3D has its bits (and as F3DEX2 has them).
    geometry: u32,
    geometry2: u32,
    texture: Texture,
    other: [u32; 2],
    half: [u32; 2],
    texrect: Option<u64>,
    fog: [i16; 2],
    clip_ratio: f32,
    stack_depth: usize,
    /// The last G_SETSCISSOR (DMEM 0xc0), which S2DEX keeps and reads.
    scissor: [u32; 2],
    /// S2DEX's own state, while it runs (s2dex.rs).
    s2: s2dex::S2dex,
    /// The 3D microcode whose time the task's work is counted in, when it is not the task's own.
    model: Option<Ucode>,
}

fn outside_pair(c: &[f32; 4], r: f32, near: bool) -> (u8, u8) {
    let [x, y, z, w] = *c;
    let near_flag = if near { z < -w } else { w < BEHIND };
    let near_bit = u8::from(near_flag) << 4;
    let cull_sides = u8::from(x < -w) | (u8::from(x > w) << 1) | (u8::from(y < -w) << 2) | (u8::from(y > w) << 3);
    let cull = cull_sides | near_bit | (u8::from(z > w) << 5);
    let codes = if r == 1.0 {
        cull_sides | near_bit
    } else {
        let rw = r * w;
        u8::from(x < -rw) | (u8::from(x > rw) << 1) | (u8::from(y < -rw) << 2) | (u8::from(y > rw) << 3) | near_bit
    };
    (codes, cull)
}

/// Which planes a vertex lies outside of: x < -rw, x > rw, y < -rw, y > rw
/// (r the clip ratio), and the near plane (z < -w).
#[allow(dead_code)]
fn outside(c: &[f32; 4], r: f32, near: bool) -> u8 {
    let [x, y, z, w] = *c;
    u8::from(x < -r * w) | u8::from(x > r * w) << 1 | u8::from(y < -r * w) << 2 | u8::from(y > r * w) << 3
        | u8::from(if near { z < -w } else { w < BEHIND }) << 4
}

/// Without near clipping, what is clipped away is what lies at or behind
/// the eye (w at most this).
const BEHIND: f32 = 1.0 / 65536.0;

impl Gfx {
    fn new(ucode: Ucode) -> Gfx {
        Gfx {
            ucode,
            work: Work::default(),
            pc: 0,
            stack: Vec::new(),
            segments: [0; 16],
            modelview: vec![IDENTITY],
            projection: IDENTITY,
            combined: None,
            mvp: IDENTITY,
            colors: [0; 0x104],
            // As the data has them.
            fixed: [[[0; 4]; 4]; 3],
            persp_norm: 0xffff,
            viewport: [160.0, 120.0, 511.0, 0.0, 160.0, 120.0, 511.0, 0.0],
            lights: [Light::default(); 8],
            lights_on: 1,
            lookat: [[0.0, 1.0, 0.0], [1.0, 0.0, 0.0]],
            vertices: [Vertex::default(); 64],
            geometry: 0,
            geometry2: 0,
            texture: Texture::default(),
            // As the microcode's data has it.
            other: [0x0008_0cff, 0],
            half: [0; 2],
            texrect: None,
            fog: [0; 2],
            clip_ratio: 1.0,
            stack_depth: 10,
            scissor: [0xed00_0000, 0x0050_03c0],
            s2: s2dex::S2dex::default(),
            model: None,
        }
    }

    /// F3DEX2 loaded again by another microcode in the task: what it keeps above DMEM 0x180 (geometry mode,
    /// lights, fog, texture, clip ratio, whether the combined matrix is up to date) starts again as its data has
    /// it; matrices, viewport, segments, other modes, scissor and the list's stack stay.
    fn reenter(&mut self, ucode: Ucode) {
        let fresh = Gfx::new(ucode);
        self.ucode = ucode;
        self.model = Some(ucode);
        self.combined = None;
        self.lights = fresh.lights;
        self.lights_on = fresh.lights_on;
        self.lookat = fresh.lookat;
        self.geometry = fresh.geometry;
        self.geometry2 = fresh.geometry2;
        self.texture = fresh.texture;
        self.fog = fresh.fog;
        self.clip_ratio = fresh.clip_ratio;
    }

    /// A segmented address made physical.
    /// A segmented address made physical; F3DEX2 keeps the segment's high
    /// bits (the RDP ignores them).
    fn address(&self, addr: u32) -> u32 {
        let physical = self.segments[(addr >> 24 & 15) as usize].wrapping_add(addr & 0x00ff_ffff);
        if matches!(self.ucode, Ucode::F3dex2 { .. } | Ucode::S2dex { .. }) { physical } else { physical & 0x00ff_ffff }
    }

    /// A matrix for the projection or the modelview stack's top, loaded
    /// or multiplied in (the modelview pushed first, if asked).
    fn matrix(&mut self, m: Matrix, projection: bool, load: bool, push: bool) {
        if projection {
            self.projection = if load { m } else { multiply(&m, &self.projection) };
        } else {
            if push && self.modelview.len() < self.stack_depth {
                self.modelview.push(*self.modelview.last().unwrap());
            }
            let top = self.modelview.last_mut().unwrap();
            *top = if load { m } else { multiply(&m, top) };
        }
        self.combined = None;
    }

    /// Perfect Dark's matrices in s15.16: the modelview or the projection loaded or multiplied
    /// in, and their product made again, as its microcode does after each G_MTX. (A push saves the
    /// modelview to RDRAM, never read back: its G_POPMTX does nothing.)
    fn pd_matrix(&mut self, m: Fixed, projection: bool, load: bool) {
        let target = &mut self.fixed[usize::from(projection)];
        *target = if load { m } else { fixed_multiply(&m, target) };
        self.fixed[2] = fixed_multiply(&self.fixed[0], &self.fixed[1]);
    }

    /// Whether any of the vertices from `first` to `last` may be on
    /// screen: not all outside the same plane (F3D's clipping planes,
    /// F3DEX2's screen and far plane).
    fn visible(&self, first: u32, last: u32) -> bool {
        let last = last.min(self.vertices.len() as u32 - 1);
        let ex = matches!(self.ucode, Ucode::F3dex2 { .. } | Ucode::F3dex { .. });
        let codes = |v: &Vertex| if ex { v.cull } else { v.codes };
        (first..=last).fold(0x3f, |all, v| all & codes(&self.vertices[v as usize])) == 0
    }

    fn combined(&mut self) -> Matrix {
        if let Some(m) = self.combined {
            return m;
        }
        let m = multiply(self.modelview.last().unwrap(), &self.projection);
        (self.combined, self.mvp) = (Some(m), m);
        m
    }

    /// F3DEX2's G_MOVEWORD: as F3D's, but lights 24 bytes apart.
    fn move_word2(&mut self, index: u32, offset: u32, data: u32) {
        match index {
            0x02 => self.lights_on = (data / 24).min(7) as usize,
            0x0a => {
                let n = (offset / 24) as usize;
                if n < 8 && offset.is_multiple_of(24) {
                    self.lights[n].color = [(data >> 24) as f32, (data >> 16 & 0xff) as f32, (data >> 8 & 0xff) as f32];
                }
            }
            0x04 | 0x06 | 0x08 => self.move_word(index, offset, data),
            // G_MW_FORCEMTX, at DMEM 0x1d8: its byte at 0x1d9 says whether the combined matrix is up
            // to date (G_MTX and G_POPMTX clear it).
            0x0c => {
                let byte = 0x1d9u32.wrapping_sub(0x1d8 + offset);
                if byte < 4 {
                    self.combined = (data >> (24 - 8 * byte) & 0xff != 0).then_some(self.mvp);
                }
            }
            _ => {}
        }
    }

    /// G_MODIFYVTX: a loaded vertex's color, texture coordinates (as they
    /// are, not scaled), place or depth on screen.
    fn modify_vertex(&mut self, v: usize, what: u32, data: u32) {
        let Some(vertex) = self.vertices.get_mut(v) else { return };
        match what {
            0x10 => {
                vertex.color = [24, 16, 8, 0].map(|shift| (data >> shift & 0xff) as f32);
                vertex.screen.color = vertex.color;
            }
            0x14 => {
                vertex.st = [f32::from((data >> 16) as i16), f32::from(data as i16)];
                vertex.screen.st = vertex.st;
            }
            0x18 => {
                vertex.screen.x = f32::from((data >> 16) as i16) / 4.0;
                vertex.screen.y = f32::from(data as i16) / 4.0;
            }
            0x1c => vertex.screen.z = data as i32 as f32 / 65536.0 * 32.0,
            _ => {}
        }
    }

    fn move_word(&mut self, index: u32, offset: u32, data: u32) {
        match index {
            0x02 => self.lights_on = ((data.wrapping_sub(0x8000_0000) >> 5) as usize).saturating_sub(1).min(7),
            0x06 => self.segments[(offset >> 2 & 15) as usize] = data,
            0x04 => self.clip_ratio = f32::from((data as i16).unsigned_abs()).max(1.0),
            0x08 => self.fog = [(data >> 16) as i16, data as i16],
            0x0e if offset == 0 => self.persp_norm = data as u16,
            0x0a => {
                let n = (offset / 0x20) as usize;
                if n < 8 && offset & 0x1f == 0 {
                    self.lights[n].color = [(data >> 24) as f32, (data >> 16 & 0xff) as f32, (data >> 8 & 0xff) as f32];
                }
            }
            _ => {}
        }
    }

    /// Whether the microcode clips at the near plane.
    fn near(&self) -> bool {
        !matches!(self.ucode, Ucode::F3dex2 { near: false, .. } | Ucode::F3dex { near: false })
    }

    /// Rare's F3D (GoldenEye's, or Perfect Dark's).
    fn rare(&self) -> bool {
        matches!(self.ucode, Ucode::F3dRare | Ucode::F3dPd)
    }

    /// F3DEX 1.x: F3D's commands, changed.
    fn ex(&self) -> bool {
        matches!(self.ucode, Ucode::F3dex { .. })
    }

    fn project(&self, v: &Vertex) -> Screen {
        let [x, y, z, w] = v.clip;
        let inv_w = if w != 0.0 { 1.0 / w } else { 0.0 };
        let vp = &self.viewport;
        // On screen in quarter pixels, as the microcode keeps them.
        let quarter = |v: f32| (v * 4.0).floor() * 0.25;
        // Nearer than the near plane (when that is not clipped): as near.
        let depth = (z * inv_w * vp[2] + vp[6]).max(if self.near() { f32::MIN } else { vp[6] - vp[2] });
        Screen {
            x: quarter(x * inv_w * vp[0] + vp[4]),
            y: quarter(-y * inv_w * vp[1] + vp[5]),
            z: depth * 32.0,
            inv_w,
            color: v.color,
            st: v.st,
        }
    }

    /// A Perfect Dark vertex's codes (`pd_codes`), place on screen and fog from its clip
    /// coordinates in s15.16, in its microcode's fixed point: w scaled by the perspective
    /// normalisation (12 or 13 65536ths in the game, which leaves a near w some ten bits), its
    /// reciprocal from the RSP's table and a Newton-Raphson step, x, y and z times that and
    /// scaled back, then the viewport. x and y are whole quarter pixels within ±4090, the depth's
    /// integer part from 0 to 0x7fff (what lies nearer than the near plane is at 0), the fog from
    /// z over w in whole steps.
    fn pd_finish(&self, vertex: &mut Vertex, clip: &[i32; 4], second: bool) {
        (vertex.codes, vertex.cull) = pd_codes(clip, self.clip_ratio as i64, second);
        let norm = i32::from(self.persp_norm);
        let w = fixed_mul(clip[3], norm);
        let r = saturate(2 * i64::from(crate::vu::reciprocal(w, false) as i32));
        let r = fixed_mul(r, saturate((2 << 16) - i64::from(fixed_mul(r, w))));
        // A negative reciprocal's integer part is taken as 0x7fff.
        let inv_w = if r < 0 { 0x7fff_0000 | r & 0xffff } else { r };
        let over_w = |c: i32| fixed_mul(fixed_mul(c, inv_w), norm);
        // The viewport in quarter pixels (y down), z as is.
        let vp = &self.viewport;
        let place = |c: i32, scale: f32, at: f32| {
            (i64::from(at as i32) << 16) + i64::from(over_w(c)) * i64::from(scale as i32)
        };
        let quarters = |v: i64| (v >> 16).clamp(-4090, 4090) as f32 / 4.0;
        let depth = place(clip[2], vp[2], vp[6]);
        if self.geometry & FOG != 0 {
            let fog = ((i64::from(over_w(clip[2])) * i64::from(self.fog[0])) >> 16) + i64::from(self.fog[1]);
            vertex.color[3] = fog.clamp(0, 255) as f32;
        }
        vertex.screen = Screen {
            x: quarters(place(clip[0], vp[0] * 4.0, vp[4] * 4.0)),
            y: quarters(place(clip[1], -vp[1] * 4.0, vp[5] * 4.0)),
            z: ((depth >> 16).clamp(0, 0x7fff) as f32 + (depth & 0xffff) as f32 / 65536.0) * 32.0,
            inv_w: r as f32 / 65536.0,
            color: vertex.color,
            st: vertex.st,
        };
    }
}

/// How far inside a clipping plane a vertex is (clip coordinates, the clip ratio).
type Plane = fn(&[f32; 4], f32) -> f32;

/// Perfect Dark's colour table in DMEM.
const PD_COLORS: usize = 0x5e0;

// Perfect Dark's codes beside outside()'s four sides: w below 0 and beyond the far plane.
const PD_BEHIND: u8 = 0x10;
const PD_FAR: u8 = 0x20;

/// Perfect Dark's vertex codes, as its microcode's VCH and VCL make them from clip coordinates in
/// s15.16 (c ≤ -w and c ≥ w, also when w is negative): in `codes` what clips a triangle, the sides
/// at the clip ratio, the far plane and w below 0 (only the second vertex of each pair G_VTX loads
/// keeps that flag: the first's lands elsewhere); in `cull` what rejects one all three share, the
/// screen's edges and the far plane (outside()'s bits; no near plane).
fn pd_codes(c: &[i32; 4], r: i64, second: bool) -> (u8, u8) {
    let [x, y, z, w] = c.map(i64::from);
    let low = |c: i64, w: i64| if (c < 0) != (w < 0) { c + w <= 0 } else { w < 0 };
    let high = |c: i64, w: i64| if (c < 0) != (w < 0) { w < 0 } else { c - w >= 0 };
    let sides = |w: i64| {
        u8::from(low(x, w)) | u8::from(high(x, w)) << 1 | u8::from(low(y, w)) << 2 | u8::from(high(y, w)) << 3
    };
    let far = if high(z, w) { PD_FAR } else { 0 };
    let behind = if second && w < 0 { PD_BEHIND } else { 0 };
    (sides(i64::from(saturate(r * w))) | far | behind, sides(w) | far)
}

/// A triangle command's words (at most 22), kept off the heap.
#[derive(Default)]
struct Words {
    w: [u64; 22],
    len: usize,
}

impl Words {
    fn extend<const N: usize>(&mut self, words: [u64; N]) {
        self.w[self.len..self.len + N].copy_from_slice(&words);
        self.len += N;
    }
}

impl std::ops::Deref for Words {
    type Target = [u64];
    fn deref(&self) -> &[u64] {
        &self.w[..self.len]
    }
}

fn s16_16(v: f32) -> i32 {
    (v * 65536.0) as i32
}

/// The RDP's triangle command for three vertices on screen (after
/// libdragon's rdpq_triangle): edges, then shade, texture and depth
/// coefficients as the modes ask for them.
fn setup(gfx: &Gfx, tri: &[Screen; 3]) -> Words {
    let mut v = *tri;
    v.sort_by(|a, b| a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal));
    let [v1, v2, v3] = v;
    let y1 = (v1.y * 4.0).floor() / 4.0;
    let y2 = (v2.y * 4.0).floor() / 4.0;
    let y3 = (v3.y * 4.0).floor() / 4.0;
    let (y1f, y2f, y3f) = ((v1.y * 4.0).floor() as i32, (v2.y * 4.0).floor() as i32, (v3.y * 4.0).floor() as i32);
    let (hx, hy) = (v3.x - v1.x, y3 - y1);
    let (mx, my) = (v2.x - v1.x, y2 - y1);
    let (lx, ly) = (v3.x - v2.x, y3 - y2);
    let nz = hx * my - hy * mx;
    let factor = if nz.abs() > f32::MIN_POSITIVE { -1.0 / nz } else { 0.0 };
    let left = u64::from(nz < 0.0);
    let slope = |dx: f32, dy: f32| if dy.abs() > f32::MIN_POSITIVE { dx / dy } else { 0.0 };
    let (ish, ism, isl) = (slope(hx, hy), slope(mx, my), slope(lx, ly));
    let fy = y1.floor() - y1;
    let xh = v1.x + fy * ish;
    let xm = v1.x + fy * ism;
    let xl = v2.x;

    let shade = gfx.geometry & SHADE != 0;
    let texture = gfx.texture.on;
    let depth = gfx.geometry & ZBUFFER != 0;
    let command = 0xc8 | u64::from(shade) << 2 | u64::from(texture) << 1 | u64::from(depth);
    let level = u64::from(gfx.texture.level);
    let tile = u64::from(gfx.texture.tile);
    let fix = |v: i32| u64::from(v as u32);
    let mut words = Words::default();
    words.extend([
        command << 56 | left << 55 | level << 51 | tile << 48 | (fix(y3f) & 0x3fff) << 32 | (fix(y2f) & 0x3fff) << 16
            | fix(y1f) & 0x3fff,
        fix(s16_16(xl)) << 32 | fix(s16_16(isl)),
        fix(s16_16(xh)) << 32 | fix(s16_16(ish)),
        fix(s16_16(xm)) << 32 | fix(s16_16(ism)),
    ]);
    // An attribute's value at the top, and its change along x, the major
    // edge and y.
    let gradients = |a1: f32, a2: f32, a3: f32| {
        let (ma, ha) = (a2 - a1, a3 - a1);
        let dx = (hy * ma - my * ha) * factor;
        let dy = (mx * ha - hx * ma) * factor;
        let de = dy + dx * ish;
        [s16_16(a1 + fy * de), s16_16(dx), s16_16(de), s16_16(dy)]
    };
    // Four attributes packed as the RDP takes them: integer parts, then
    // fractions, of the value, d/dx, d/de and d/dy.
    let pack = |g: [[i32; 4]; 4]| -> [u64; 8] {
        let hi = |k: usize| {
            (u64::from((g[0][k] >> 16) as u16) << 48)
                | (u64::from((g[1][k] >> 16) as u16) << 32)
                | (u64::from((g[2][k] >> 16) as u16) << 16)
                | u64::from((g[3][k] >> 16) as u16)
        };
        let lo = |k: usize| {
            (u64::from(g[0][k] as u16) << 48)
                | (u64::from(g[1][k] as u16) << 32)
                | (u64::from(g[2][k] as u16) << 16)
                | u64::from(g[3][k] as u16)
        };
        [hi(0), hi(1), lo(0), lo(1), hi(2), hi(3), lo(2), lo(3)]
    };
    if shade {
        // The microcode's colors carry half a step, to round.
        let g: [[i32; 4]; 4] = std::array::from_fn(|c| gradients(v1.color[c] + 0.5, v2.color[c] + 0.5, v3.color[c] + 0.5));
        words.extend(pack(g));
    }
    if texture {
        // s/w and t/w with 1/w, scaled so that the largest 1/w is 0x3fff.
        let most = v1.inv_w.max(v2.inv_w).max(v3.inv_w);
        let norm = if most > 0.0 { 1.0 / most } else { 1.0 };
        let w: [f32; 3] = [v1.inv_w * norm, v2.inv_w * norm, v3.inv_w * norm];
        // The microcode's s and t are as for a 1/w of 0x7fff.
        let st = |i: usize, c: usize| [v1, v2, v3][i].st[c] * w[i] * (16383.0 / 32767.0);
        let g: [[i32; 4]; 4] = [
            gradients(st(0, 0), st(1, 0), st(2, 0)),
            gradients(st(0, 1), st(1, 1), st(2, 1)),
            gradients(w[0] * 16383.0, w[1] * 16383.0, w[2] * 16383.0),
            [0; 4],
        ];
        words.extend(pack(g));
    }
    if depth {
        let [z, dx, de, dy] = gradients(v1.z, v2.z, v3.z);
        words.extend([fix(z) << 32 | fix(dx), fix(de) << 32 | fix(dy)]);
    }
    words
}

/// Compares this with the microcode's commands for one task: the commands
/// other than triangles in order, word for word, and the triangles in
/// order, within tolerances.
fn compare(report: &mut Report, expected: &[Vec<u64>], actual: &[Vec<u64>]) {
    report.tasks += 1;
    let task = report.tasks;
    let note = |report: &mut Report, text: String| {
        if report.notes.len() < 60 {
            report.notes.push(text);
        }
    };
    // No-ops left out, and syncs by their kind alone (microcodes leave
    // anything in the rest).
    let normal = |commands: &[Vec<u64>]| -> Vec<Vec<u64>> {
        commands.iter().filter(|c| c[0] >> 56 & 0x3f != 0).map(|c| match c[0] >> 56 & 0x3f {
            0x26..=0x29 => vec![c[0] & 0xff00_0000_0000_0000],
            _ => c.clone(),
        }).collect()
    };
    let (expected, actual) = (normal(expected), normal(actual));
    let is_triangle = |c: &&Vec<u64>| (0x08..=0x0f).contains(&((c[0] >> 56) as u8 & 0x3f));
    let (tri_e, other_e): (Vec<&Vec<u64>>, Vec<&Vec<u64>>) = expected.iter().partition(is_triangle);
    let (tri_a, other_a): (Vec<&Vec<u64>>, Vec<&Vec<u64>>) = actual.iter().partition(is_triangle);
    if other_e.len() != other_a.len() || tri_e.len() != tri_a.len() {
        report.misaligned += 1;
        note(report, format!("task {task}: {} commands and {} triangles here, {} and {} from the RSP",
            other_e.len(), tri_e.len(), other_a.len(), tri_a.len()));
    }
    for (i, (e, a)) in other_e.iter().zip(&other_a).enumerate() {
        report.commands += 1;
        if e == a {
            report.same += 1;
        } else {
            note(report, format!("task {task} command {i}: {:016x?} vs {:016x?}", e, a));
        }
    }
    let (mut i, mut j, mut close_here) = (0, 0, 0);
    while i < tri_e.len() && j < tri_a.len() {
        report.triangles += 1;
        let (close, text) = triangle_difference(tri_e[i], tri_a[j]);
        if close {
            report.close += 1;
            close_here += 1;
            (i, j) = (i + 1, j + 1);
            continue;
        }
        // In step again past a few that only one side has.
        let matches = |i: usize, j: usize| i < tri_e.len() && j < tri_a.len() && triangle_difference(tri_e[i], tri_a[j]).0;
        if let Some(k) = (1..4).find(|&k| matches(i + k, j) && matches(i + k + 1, j + 1)) {
            note(report, format!("task {task} triangle {i}: {k} only here, {text}"));
            i += k;
        } else if let Some(k) = (1..4).find(|&k| matches(i, j + k) && matches(i + 1, j + k + 1)) {
            note(report, format!("task {task} triangle {i}: {k} only from the RSP, {text}"));
            j += k;
        } else {
            note(report, format!("task {task} triangle {i}: {text}"));
            (i, j) = (i + 1, j + 1);
        }
    }
    if close_here * 10 < tri_a.len() * 9 && report.poor.len() < 40 {
        report.poor.push(format!("task {task}: {close_here} of {} triangles close", tri_a.len()));
    }
}

/// Whether two triangles are close, and how they differ.
fn triangle_difference(e: &[u64], a: &[u64]) -> (bool, String) {
    let y = |w: u64, s: u32| ((w >> s) as i32) << 18 >> 18;
    let ys_e = [y(e[0], 0), y(e[0], 16), y(e[0], 32)];
    let ys_a = [y(a[0], 0), y(a[0], 16), y(a[0], 32)];
    let x = |w: &[u64], i: usize| (w[i] >> 32) as i32 as f32 / 65536.0;
    let xs_e = [x(e, 1), x(e, 2), x(e, 3)];
    let xs_a = [x(a, 1), x(a, 2), x(a, 3)];
    // The corners, from the major edge (the top and the bottom) and the low
    // edge's start (the middle); the middle edge's start goes unused when
    // it is short, and microcodes differ the most there. Which corner a
    // tie in y makes the middle (and so the edges' sides) is the
    // microcode's choice, so the corners are compared in any order.
    let corners = |w: &[u64], ys: [i32; 3]| {
        let slope = w[2] as u32 as i32 as f32 / 65536.0;
        let [yh, _, yl] = ys.map(|y| y as f32 / 4.0);
        let top = x(w, 2) + slope * (yh - yh.floor());
        [(top, ys[0]), (x(w, 1), ys[1]), (top + slope * (yl - yh), ys[2])]
    };
    let (ce, ca) = (corners(e, ys_e), corners(a, ys_a));
    let near = |p: (f32, i32), q: (f32, i32)| (p.0 - q.0).abs() <= 1.0 && (p.1 - q.1).abs() <= 1;
    let same_corners = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]]
        .iter()
        .any(|order| order.iter().enumerate().all(|(i, &k)| near(ce[i], ca[k])));
    let same_kind = e[0] >> 48 & !0x80 == a[0] >> 48 & !0x80;
    let close = same_corners && same_kind && e.len() == a.len();
    let attrs = |w: &[u64]| -> Vec<f32> {
        w[4..].chunks(8).flat_map(|c| {
            if c.len() == 8 {
                (0..4).map(|k| ((c[0] >> (48 - 16 * k) & 0xffff) << 16 | c[2] >> (48 - 16 * k) & 0xffff) as u32 as i32 as f32
                    / 65536.0).collect::<Vec<_>>()
            } else {
                vec![(c[0] >> 32) as i32 as f32 / 65536.0]
            }
        }).collect()
    };
    (close, format!("triangle {:04x}/{:04x} y {ys_e:?} vs {ys_a:?}, x {xs_e:?} vs {xs_a:?}, values {:?} vs {:?}",
        e[0] >> 48, a[0] >> 48, attrs(e), attrs(a)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: u32 = 0x1000;
    const LIST: u32 = 0x2000;
    const VIEWPORT: u32 = 0x3000;
    const MATRIX: u32 = 0x3100;
    const VERTICES: u32 = 0x3200;

    fn put32(n64: &mut N64, at: u32, value: u32) {
        n64.rdram[at as usize..at as usize + 4].copy_from_slice(&value.to_be_bytes());
    }

    fn put16(n64: &mut N64, at: u32, value: u16) {
        n64.rdram[at as usize..at as usize + 2].copy_from_slice(&value.to_be_bytes());
    }

    /// A machine with F3D's version text in its microcode data, a graphics
    /// task in DMEM, and the display list `commands` at LIST.
    fn machine(commands: &[(u32, u32)]) -> N64 {
        machine_of(b"RSP SW Version: 2.0D", commands)
    }

    /// Runs the machine's events up to a cycle, without the CPU.
    fn advance(n64: &mut N64, to: u64) {
        n64.cycles = to;
        n64.run_events();
    }

    #[test]
    fn a_sync_full_while_the_rsp_runs_interrupts_when_it_stops() {
        const SYNC_FULL: u64 = 0x29 << 56;
        let mut n64 = machine(&[]);
        n64.rsp.status &= !HALTED;
        n64.hle.from_rsp = true;
        n64.rdp_command(&[SYNC_FULL]);
        n64.hle.from_rsp = false;
        // Its own 50 clocks are done after 75 of the CPU's cycles.
        let done = n64.cycles + 75;
        advance(&mut n64, done + 1000);
        assert_eq!(n64.mi_intr & crate::DP_INTR, 0, "not while the RSP runs");
        // A state saved now keeps the interrupt that waits.
        let mut loaded = machine(&[]);
        loaded.load_state(&n64.save_state()).unwrap();
        for n64 in [&mut n64, &mut loaded] {
            n64.rsp.status |= HALTED;
            n64.rsp_stopped();
            assert_ne!(n64.mi_intr & crate::DP_INTR, 0, "at once, the SYNC_FULL done");
        }
        // Before it is done, the RSP's stop leaves it for then.
        n64.mi_intr = 0;
        n64.rsp.status &= !HALTED;
        n64.hle.from_rsp = true;
        n64.rdp_command(&[SYNC_FULL]);
        n64.hle.from_rsp = false;
        let done = n64.cycles + 75;
        n64.rsp_stopped();
        advance(&mut n64, done - 1);
        assert_eq!(n64.mi_intr & crate::DP_INTR, 0, "not before the SYNC_FULL is done");
        advance(&mut n64, done);
        assert_ne!(n64.mi_intr & crate::DP_INTR, 0);
        // From the CPU it comes when it is done, the RSP running or not.
        n64.mi_intr = 0;
        n64.rsp.status &= !HALTED;
        n64.rdp_command(&[SYNC_FULL]);
        let done = n64.cycles + 75;
        advance(&mut n64, done - 1);
        assert_eq!(n64.mi_intr & crate::DP_INTR, 0);
        advance(&mut n64, done);
        assert_ne!(n64.mi_intr & crate::DP_INTR, 0);
    }

    /// One SYNC_FULL's interrupt does not take the place of another's: a
    /// list the CPU sent, then a task's that waits for the RSP.
    #[test]
    fn sync_fulls_interrupt_each_in_turn() {
        const SYNC_FULL: u64 = 0x29 << 56;
        let mut n64 = machine(&[]);
        n64.rsp.status &= !HALTED;
        n64.rdp_command(&[SYNC_FULL]);
        let first = n64.cycles + 75;
        n64.hle.from_rsp = true;
        n64.rdp_command(&[SYNC_FULL]);
        n64.hle.from_rsp = false;
        advance(&mut n64, first);
        assert_ne!(n64.mi_intr & crate::DP_INTR, 0, "the CPU's");
        n64.mi_intr = 0;
        advance(&mut n64, first + 1000);
        assert_eq!(n64.mi_intr & crate::DP_INTR, 0, "the task's waits for the RSP");
        n64.rsp.status |= HALTED;
        n64.rsp_stopped();
        assert_ne!(n64.mi_intr & crate::DP_INTR, 0);
    }

    #[test]
    fn lists_that_load_another_microcode_go_to_the_rsp() {
        // F3DZEX: the list calls one through segment 6 (set by G_MOVEWORD), which loads S2DEX, or
        // ends first.
        const SUB: u32 = 0x4000;
        for (first, loads) in [(0xdd00_07ffu32, true), (0xdf00_0000, false)] {
            let mut n64 = machine_of(b"RSP Gfx ucode F3DZEX.NoN  fifo 2.08",
                &[(0xdb06_0018, SUB - 0x100), (0xde00_0000, 0x0600_0100), (0xdf00_0000, 0)]);
            for (i, word) in [first, 0x0010_0000, 0xdf00_0000, 0].into_iter().enumerate() {
                put32(&mut n64, SUB + 4 * i as u32, word);
            }
            n64.hle.enabled = true;
            assert_eq!(n64.gfx_task_started(), !loads, "the HLE runs it");
            assert_eq!(n64.hle.report.loading, u64::from(loads));
        }
    }

    fn machine_of(version: &[u8], commands: &[(u32, u32)]) -> N64 {
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        let mut n64 = N64::new(&rom).unwrap();
        n64.rdram[DATA as usize..DATA as usize + version.len()].copy_from_slice(version);
        for (offset, value) in [(0x00, 1), (0x04, 0), (0x18, 0x8000_0000 | DATA), (0x1c, 0x100), (0x24, 0x400),
            (0x30, 0x8000_0000 | LIST)] {
            n64.rsp.write_memory(0xfc0 + offset, value, !0);
        }
        for (i, &(w0, w1)) in commands.iter().enumerate() {
            put32(&mut n64, LIST + 8 * i as u32, w0);
            put32(&mut n64, LIST + 8 * i as u32 + 4, w1);
        }
        // A viewport of 320x240 (in quarter pixels, z as is), and a matrix
        // that scales by 1/256 (s15.16: integer parts, then fractions).
        for (i, v) in [640u16, 480, 511, 0, 640, 480, 511, 0].into_iter().enumerate() {
            put16(&mut n64, VIEWPORT + 2 * i as u32, v);
        }
        for i in 0..4u32 {
            let (int, frac) = if i == 3 { (1, 0) } else { (0, 0x100) };
            put16(&mut n64, MATRIX + 2 * (5 * i), int);
            put16(&mut n64, MATRIX + 32 + 2 * (5 * i), frac);
        }
        n64.hle.compare = true;
        n64
    }

    fn capture(n64: &mut N64) -> Vec<Vec<u64>> {
        let mut copy = n64.clone();
        copy.hle = Hle { capturing: true, capture_only: true, ..Hle::default() };
        let word = |o: u32| copy.rsp.read_memory(0xfc0 + o);
        let task = Task { flags: word(0x04), dram_stack_size: word(0x24), ucode_data: physical(word(0x18)),
            ucode_data_size: word(0x1c), data_ptr: physical(word(0x30)) };
        let ucode = copy.identify(&task).expect("F3D");
        copy.run_gfx(ucode, &task);
        copy.hle.captured
    }

    #[test]
    fn f3d_triangle_becomes_the_rdp_command() {
        let list = [
            (0x0380_0010, VIEWPORT),
            (0x0103_0040, MATRIX),
            (0xb700_0000, SHADE | SHADING_SMOOTH),
            (0x0420_0030, VERTICES),
            (0xbf00_0000, 10 << 8 | 20),
            (0xb800_0000, 0),
        ];
        let mut n64 = machine(&list);
        // (-128, -128), (128, -128), (0, 128): on screen (80, 180), (240,
        // 180) and (160, 60), red, green and blue.
        let vertices: [(i16, i16, u32); 3] =
            [(-128, -128, 0xff00_00ff), (128, -128, 0x00ff_00ff), (0, 128, 0x0000_ffff)];
        for (i, (x, y, color)) in vertices.into_iter().enumerate() {
            let at = VERTICES + 16 * i as u32;
            put16(&mut n64, at, x as u16);
            put16(&mut n64, at + 2, y as u16);
            put32(&mut n64, at + 12, color);
        }
        let commands = capture(&mut n64);
        assert_eq!(commands.len(), 1);
        let t = &commands[0];
        assert_eq!(t[0] >> 56, 0xcc, "shaded, untextured, without depth");
        let y = |shift: u32| (t[0] >> shift & 0x3fff) as u32;
        assert_eq!((y(0), y(16), y(32)), (240, 720, 720), "top at line 60, the others at 180");
        let x = |i: usize| (t[i] >> 32) as i32 as f32 / 65536.0;
        assert_eq!(x(2), 160.0, "the major edge from the top vertex");
        // The top vertex is blue: red 0, blue 255, with the half step.
        assert_eq!(t[4] >> 48, 0, "red at the top");
        assert_eq!(t[4] >> 16 & 0xffff, 255, "blue at the top");
    }

    #[test]
    fn rare_f3d_draws_up_to_four_triangles_a_command_and_takes_t_first() {
        let list = [
            (0x0380_0010, VIEWPORT),
            (0x0103_0040, MATRIX),
            (0xbb00_0001, 0x0800_0800),
            (0x0430_0030, VERTICES),
            // Vertices 1, 2, 0, then 2, 0, 1; the other two all 0: none.
            (0xb100_0010, 0x0000_0221),
            (0xb800_0000, 0),
        ];
        let mut n64 = machine_of(b"RSP SW Version: 2.0G", &list);
        for (i, (x, y, s, t)) in [(-128i16, -128i16, 0, 0), (128, -100, 0, 0), (0, 128, 64, 512)].into_iter().enumerate() {
            let at = VERTICES + 16 * i as u32;
            put16(&mut n64, at, x as u16);
            put16(&mut n64, at + 2, y as u16);
            put16(&mut n64, at + 8, s);
            put16(&mut n64, at + 10, t);
        }
        let commands = capture(&mut n64);
        assert_eq!(commands.len(), 2, "two triangles, the empty two left out");
        for t in &commands {
            assert_eq!(t[0] >> 56, 0xca, "textured, unshaded, without depth");
            let y = |shift: u32| (t[0] >> shift & 0x3fff) as u32;
            assert_eq!(y(0), 240, "the top at line 60");
            // At the top (s 64, t 512), with 1/w at half of 0x7fff: S from
            // t, T from s, the texture's scale of 1/32 taken as 1.
            assert_eq!((t[4] >> 48, t[4] >> 32 & 0xffff), (255, 31));
        }
    }

    /// Perfect Dark: 12-byte vertices (from bit 2 of their address) whose colour is at their offset
    /// in the table G_VTXCOLORBASE copies, smooth shading whatever the geometry mode says, and no
    /// clipping at the near plane: a triangle nearer than it is drawn at depth 0.
    #[test]
    fn perfect_dark_takes_colours_from_its_table_and_draws_what_is_nearer_than_the_near_plane() {
        const IDENTITY_AT: u32 = 0x3400;
        const COLORS: u32 = 0x3500;
        let list = [
            (0x0380_0010, VIEWPORT),
            // The projection scales by 1/256, the modelview is the identity.
            (0x0103_0040, MATRIX),
            (0x0102_0040, IDENTITY_AT),
            (0xb700_0000, SHADE | ZBUFFER),
            (0x0704_0008, COLORS),
            (0x0420_0024, VERTICES + 4),
            (0xb100_0002, 0x0000_0010),
            (0xb800_0000, 0),
        ];
        let mut n64 = machine(&list);
        for i in 0..4 {
            put16(&mut n64, IDENTITY_AT + 2 * (5 * i), 1);
        }
        put32(&mut n64, COLORS, 0xff00_00ff);
        put32(&mut n64, COLORS + 4, 0x00ff_00ff);
        // (80, 180) red, (240, 180) green and (160, 60) red, all at z -512: beyond the near plane.
        for (i, (x, y, color)) in [(-128i16, -128i16, 0u8), (128, -128, 4), (0, 128, 0)].into_iter().enumerate() {
            let at = VERTICES + 4 + 12 * i as u32;
            put16(&mut n64, at, x as u16);
            put16(&mut n64, at + 2, y as u16);
            put16(&mut n64, at + 4, -512i16 as u16);
            n64.rdram[at as usize + 7] = color;
        }
        let mut copy = n64.clone();
        copy.hle = Hle { capturing: true, capture_only: true, ..Hle::default() };
        let task = Task { flags: 1, dram_stack_size: 0x400, ucode_data: DATA, ucode_data_size: 0x100, data_ptr: LIST };
        copy.run_gfx(Ucode::F3dPd, &task);
        let commands = copy.hle.captured;
        assert_eq!(commands.len(), 1, "one triangle, the three degenerate ones left out");
        let t = &commands[0];
        assert_eq!(t[0] >> 56, 0xcd, "shaded with depth");
        assert_eq!((t[0] >> 32 & 0x3fff, t[0] & 0x3fff), (720, 240), "from line 60 to 180");
        assert_eq!((t[4] >> 48, t[4] >> 32 & 0xffff), (255, 0), "red at the top");
        assert_ne!(t[5] >> 32 & 0xffff, 0, "green changes along x: not flat");
        assert_eq!(t[12] >> 32, 0, "at depth 0");
    }

    #[test]
    fn f3d_texture_rectangle_takes_rdphalf_2_then_cont() {
        let list = [(0xe446_4370, 0x0042_8334), (0xb300_0000, 0x0000_0000), (0xb200_0000, 0x1000_0400),
            (0xb800_0000, 0)];
        let mut n64 = machine(&list);
        assert_eq!(capture(&mut n64), vec![vec![0xe446_4370_0042_8334, 0x0000_0000_1000_0400]]);
    }

    /// Mario Kart 64's menus: F3DEX 0.95 with F3D's G_RDPHALF_2 and
    /// G_RDPHALF_CONT after the rectangle, as the microcode draws them (the
    /// 0xb2 is no G_MODIFYVTX there); and F3DEX's own G_RDPHALF_1 and 2.
    #[test]
    fn f3dex_texture_rectangle_takes_the_next_two_commands_whatever_their_opcodes() {
        const MK64: &[u8] = b"RSP Gfx ucode F3DEX         0.95 Yoshitaka Yasumoto Nintendo.";
        let rect = (0xe450_0010, 0x0000_0008);
        let expected = vec![vec![0xe450_0010_0000_0008, 0x0000_0040_1000_0400]];
        for halves in [[0xb300_0000, 0xb200_0000], [0xb400_0000, 0xb300_0000]] {
            let list = [rect, (halves[0], 0x0000_0040), (halves[1], 0x1000_0400), (0xb800_0000, 0)];
            let mut n64 = machine_of(MK64, &list);
            assert_eq!(capture(&mut n64), expected, "after {halves:x?}");
        }
    }

    #[test]
    fn f3d_segments_resolve_image_addresses_and_tasks_end_with_the_break() {
        let list = [(0xbc00_0406, 0x0012_3400), (0xff10_013f, 0x0100_0010), (0xb800_0000, 0)];
        let mut n64 = machine(&list);
        assert_eq!(capture(&mut n64), vec![vec![0xff10_013f_0012_3410]]);

        n64.hle.compare = false;
        n64.set_gfx_hle(true);
        n64.rsp.status = INTBREAK | HALTED;
        n64.write_sp_register(0x0404_0010, 1);
        assert!(n64.rsp.status & HALTED == 0, "busy until the task's time is up");
        n64.rsp_event();
        assert_eq!(n64.rsp.status & (HALTED | BROKE | TASK_DONE), HALTED | BROKE | TASK_DONE);
        assert!(n64.mi_intr & SP_INTR != 0);
    }

    /// F3DEX2: its own opcodes and geometry bits, G_TRI2's second triangle
    /// first, and a triangle wholly off the screen left out even when it
    /// lies within the clipping planes.
    #[test]
    fn f3dex2_draws_tri2_second_first_and_leaves_out_what_is_off_screen() {
        let list = [
            (0xdc08_0008, VIEWPORT),
            // The projection, loaded (G_MTX's push bit is inverted).
            (0xda38_0007, MATRIX),
            (0xdb04_0004, 2),
            (0xd900_0000, 0x0020_0004),
            (0x0100_700e, VERTICES),
            (0x0600_0204, 0x0000_0406),
            (0x0508_0a0c, 0),
            (0xdf00_0000, 0),
        ];
        let mut n64 = machine_of(b"RSP Gfx ucode F3DEX       fifo 2.07  Yoshitaka Yasumoto 1998 Nintendo.", &list);
        // On screen (80, 180), (240, 180), (160, 60) and (240, 90); then
        // three left of the screen, within twice its width.
        let vertices: [(i16, i16); 7] = [(-128, -128), (128, -128), (0, 128), (128, 64), (-300, 0), (-290, 10), (-280, -10)];
        for (i, (x, y)) in vertices.into_iter().enumerate() {
            let at = VERTICES + 16 * i as u32;
            put16(&mut n64, at, x as u16);
            put16(&mut n64, at + 2, y as u16);
            put32(&mut n64, at + 12, 0x8040_20ff + i as u32 * 0x1000);
        }
        let commands = capture(&mut n64);
        assert_eq!(commands.len(), 2, "two triangles: the third is off the screen");
        let lines = |t: &[u64]| [0, 16, 32].map(|shift| (t[0] >> shift & 0x3fff) as u32);
        assert_eq!(commands[0][0] >> 56, 0xcc, "shaded, from F3DEX2's G_SHADE");
        assert_eq!(lines(&commands[0]), [240, 360, 720], "vertices 0, 2 and 3 first");
        assert_eq!(lines(&commands[1]), [240, 720, 720], "then 0, 1 and 2");
    }

    /// F3DEX 1.x: G_VTX's own fields, indices doubled, and G_LINE3D drawn
    /// as a quadrangle of two triangles.
    #[test]
    fn f3dex_loads_32_vertices_and_draws_quadrangles() {
        let list = [
            (0x0380_0010, VIEWPORT),
            (0x0103_0040, MATRIX),
            (0xb700_0000, SHADE | SHADING_SMOOTH),
            // Four vertices into 20 to 23.
            (0x0428_103f, VERTICES),
            (0xb500_0000, 0x2a28_2e2c),
            (0xb800_0000, 0),
        ];
        let mut n64 = machine_of(b"RSP Gfx ucode F3DEX         0.95 Yoshitaka Yasumoto Nintendo.", &list);
        // (80, 180), (240, 180), (240, 60) and (80, 60): vertices 21, 20, 23, 22.
        for (i, (x, y)) in [(128i16, -128i16), (-128, -128), (-128, 128), (128, 128)].into_iter().enumerate() {
            let at = VERTICES + 16 * i as u32;
            put16(&mut n64, at, x as u16);
            put16(&mut n64, at + 2, y as u16);
            put32(&mut n64, at + 12, 0xffff_ffff);
        }
        let commands = capture(&mut n64);
        assert_eq!(commands.len(), 2, "two triangles");
        let lines = |t: &[u64]| [0, 16, 32].map(|shift| (t[0] >> shift & 0x3fff) as u32);
        assert_eq!(lines(&commands[0]), [240, 240, 720]);
        assert_eq!(lines(&commands[1]), [240, 720, 720]);
    }

    #[test]
    fn other_microcodes_are_left_to_the_rsp() {
        let mut n64 = machine(&[(0xb800_0000, 0)]);
        n64.rdram[DATA as usize..DATA as usize + 20].copy_from_slice(b"RSP Gfx ucode F3DEX ");
        n64.set_gfx_hle(true);
        n64.rsp.status = HALTED;
        assert!(!n64.gfx_task_started());
    }
}
