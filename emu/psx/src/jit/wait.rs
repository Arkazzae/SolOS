//! Waits for the vertical blank as PsyQ's VSync does them: a loop counting
//! a timeout down on the stack until a counter, which the blank's interrupt
//! moves, reaches a mark. Until the next event nothing the loop reads can
//! change, so its turns up to then are run at once, as the compiled blocks
//! would run them: cycle for cycle, ending at the same block end, with the
//! registers and the timeout as they would be there.

use crate::Psx;

/// Cycles of the loop's first block (eight instructions, two loads from
/// RAM) and of a turn (the second: six instructions, one load), as the
/// compiled blocks count them.
const FIRST: u64 = 8 + 2 * 4;
const TURN: u64 = FIRST + 6 + 4;

/// The loop: its two blocks, the register they both work in, where the
/// timeout is (a base register and offset), the register holding what the
/// timeout ends at, the counter's address, and the register with the mark.
#[derive(Clone, Copy)]
pub struct Wait {
    pub first: u32,
    second: u32,
    work: usize,
    base: usize,
    offset: u32,
    end: usize,
    counter: u32,
    mark: usize,
}

/// The register fields of an instruction.
fn rs(op: u32) -> usize {
    (op >> 21 & 31) as usize
}

fn rt(op: u32) -> usize {
    (op >> 16 & 31) as usize
}

/// Where a branch at `at` goes.
fn target(at: u32, op: u32) -> u32 {
    at.wrapping_add(4).wrapping_add(((op & 0xffff) as i16 as i32 as u32) << 2)
}

/// The loop whose first block is at `pc`, if one is:
///
/// ```text
/// first:  lw t, off(s); nop; addiu t, t, -1; sw t, off(s)
///         lw t, off(s); nop; bne t, e, second; nop
/// second: lui t, hi; lw t, lo(t); nop; slt t, t, m
///         bne t, zero, first; nop
/// ```
pub fn find(psx: &Psx, pc: u32) -> Option<Wait> {
    let at = |base: u32, i: u32| psx.peek32(base.wrapping_add(4 * i));
    let (load, step, store, again, branch) = (at(pc, 0), at(pc, 2), at(pc, 3), at(pc, 4), at(pc, 6));
    let (base, work, offset) = (rs(load), rt(load), load & 0xffff);
    let (w, b) = (work as u32, base as u32);
    let first_fits = load >> 26 == 0x23
        && [1, 5, 7].iter().all(|&i| at(pc, i) == 0)
        && step == (0x09 << 26 | w << 21 | w << 16 | 0xffff)
        && store == (0x2b << 26 | b << 21 | w << 16 | offset)
        && again == load
        && branch >> 26 == 0x05
        && rs(branch) == work;
    if !first_fits {
        return None;
    }
    let (end, second) = (rt(branch), target(pc + 24, branch));
    let (high, low, compare, back) = (at(second, 0), at(second, 1), at(second, 3), at(second, 4));
    let mark = rt(compare);
    let second_fits = high >> 26 == 0x0f
        && rt(high) == work
        && at(second, 2) == 0
        && at(second, 5) == 0
        && low >> 26 == 0x23
        && rs(low) == work
        && rt(low) == work
        && compare & 0xfc00_07ff == 0x2a
        && rs(compare) == work
        && (compare >> 11 & 31) as usize == work
        && back >> 26 == 0x05
        && rs(back) == work
        && rt(back) == 0
        && target(second + 16, back) == pc;
    if !second_fits || work == 0 || [base, end, mark].contains(&work) {
        return None;
    }
    let counter = (high << 16).wrapping_add((low & 0xffff) as i16 as i32 as u32);
    Some(Wait { first: pc, second, work, base, offset, end, counter, mark })
}

impl Psx {
    /// Runs the wait's turns up to the next event, from its first block;
    /// false (nothing done) where it cannot be told ahead: the wait over,
    /// the timeout coming, the counter or timeout not in RAM.
    pub(crate) fn wait_turns(&mut self, wait: &Wait) -> bool {
        let timeout = self.cpu.r[wait.base].wrapping_add(wait.offset as i16 as i32 as u32);
        let in_ram = |a: u32| a & 0x1fff_ffff < 0x80_0000 && a & 3 == 0;
        if !in_ram(timeout) || !in_ram(wait.counter) || self.code_pages[(timeout as usize & 0x1f_ffff) >> 12] != 0 {
            return false;
        }
        let word = |psx: &Psx, a: u32| psx.peek32(a);
        if word(self, wait.counter) as i32 >= self.cpu.r[wait.mark] as i32 {
            return false;
        }
        // The timeout ends at the turn that counts it down to `end`: the
        // turns before that one can run.
        let left = word(self, timeout);
        let safe = match left.wrapping_sub(self.cpu.r[wait.end]) {
            0 => u64::MAX,
            k => u64::from(k - 1),
        };
        if safe == 0 || self.cycles >= self.next_event {
            return false;
        }
        // The blocks end where the cycles first reach the event: after the
        // first block of a turn, or after a whole one.
        let whole = (self.next_event - self.cycles - 1) / TURN;
        let partway = self.cycles + whole * TURN + FIRST >= self.next_event;
        let (turns, partway) = if whole + 1 > safe { (safe, false) } else { (whole + 1, partway) };
        let counted = left.wrapping_sub(turns as u32);
        let i = timeout as usize & 0x1f_fffc;
        self.ram[i..i + 4].copy_from_slice(&counted.to_le_bytes());
        let (pc, work, cycles) = if partway {
            (wait.second, counted, (turns - 1) * TURN + FIRST)
        } else {
            (wait.first, 1, turns * TURN)
        };
        self.cycles += cycles;
        self.cpu.r[wait.work] = work;
        self.cpu.pc = pc;
        self.cpu.next_pc = pc.wrapping_add(4);
        true
    }
}
