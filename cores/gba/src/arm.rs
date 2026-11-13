//! ARM instructions (ARMv4): 32-bit opcodes, each with a condition.

use crate::Gba;
use crate::cpu::{C, T, multiply_cycles, shift_by_immediate, shift_by_register};

impl Gba {
    pub(crate) fn step_arm(&mut self) {
        let op = self.cpu.pipe[0];
        self.cpu.pipe[0] = self.cpu.pipe[1];
        let pc = self.cpu.r[15];
        self.cpu.pipe[1] = self.fetch32(pc);
        if self.cpu.condition(op >> 28) {
            self.execute_arm(op);
        }
        self.advance(pc, 4);
    }

    fn execute_arm(&mut self, op: u32) {
        match op >> 25 & 7 {
            0 if op & 0x0fff_fff0 == 0x012f_ff10 => self.arm_bx(op),
            0 if op & 0x90 == 0x90 => match (op >> 5 & 3, op >> 23 & 3) {
                (0, 0) if op & 1 << 22 == 0 => self.arm_multiply(op),
                (0, 1) => self.arm_multiply_long(op),
                (0, 2) if op & 0x0030_0f00 == 0 => self.arm_swap(op),
                (0, _) => self.undefined(),
                _ => self.arm_halfword_transfer(op),
            },
            0 | 1 if op >> 23 & 3 == 2 && op & 1 << 20 == 0 => match (op >> 21 & 1, op >> 25 & 1) {
                (0, 0) if op & 0xfff == 0 => self.arm_mrs(op),
                (1, _) => self.arm_msr(op),
                _ => self.undefined(),
            },
            0 | 1 => self.arm_data_processing(op),
            3 if op & 0x10 != 0 => self.undefined(),
            2 | 3 => self.arm_single_transfer(op),
            4 => self.arm_block_transfer(op),
            5 => self.arm_branch(op),
            7 if op & 1 << 24 != 0 => self.swi(op >> 16 & 0xff),
            _ => self.undefined(),
        }
    }

    fn arm_bx(&mut self, op: u32) {
        let target = self.cpu.r[op as usize & 15];
        self.cpu.set_flag(T, target & 1 != 0);
        self.jump(target);
    }

    fn arm_branch(&mut self, op: u32) {
        let offset = ((op << 8) as i32 >> 6) as u32;
        let pc = self.cpu.r[15];
        if op & 1 << 24 != 0 {
            self.cpu.r[14] = pc.wrapping_sub(4);
            return self.jump(pc.wrapping_add(offset));
        }
        self.jump(pc.wrapping_add(offset));
        if offset as i32 <= -8 {
            self.loop_branch();
        }
    }

    fn arm_data_processing(&mut self, op: u32) {
        let flags = op & 1 << 20 != 0;
        let (rn, rd) = ((op >> 16 & 15) as usize, (op >> 12 & 15) as usize);
        let carry = self.cpu.flag(C);
        let mut a = self.cpu.r[rn];
        let (b, shifter_carry) = if op & 1 << 25 != 0 {
            let rotate = op >> 7 & 0x1e;
            let value = (op & 0xff).rotate_right(rotate);
            (value, if rotate == 0 { carry } else { value >> 31 != 0 })
        } else if op & 0x10 == 0 {
            shift_by_immediate(op >> 5 & 3, op >> 7 & 31, self.cpu.r[op as usize & 15], carry)
        } else {
            // Shifting by a register takes a cycle more, by which r15 has
            // moved on another word.
            self.idle(1);
            let amount = self.cpu.r[(op >> 8 & 15) as usize] & 0xff;
            let rm = op as usize & 15;
            let value = if rm == 15 { self.cpu.r[15].wrapping_add(4) } else { self.cpu.r[rm] };
            if rn == 15 {
                a = a.wrapping_add(4);
            }
            shift_by_register(op >> 5 & 3, amount, value, carry)
        };
        // Flags are set when asked, except when the result goes to r15:
        // then the CPSR comes back from the SPSR.
        let set = flags && rd != 15;
        let logical = |gba: &mut Gba, result: u32| {
            if set {
                gba.cpu.set_nz(result);
                gba.cpu.set_flag(C, shifter_carry);
            }
            result
        };
        let result = match op >> 21 & 15 {
            0x0 => logical(self, a & b),
            0x1 => logical(self, a ^ b),
            0x2 => self.cpu.sub(a, b, true, set),
            0x3 => self.cpu.sub(b, a, true, set),
            0x4 => self.cpu.add(a, b, false, set),
            0x5 => self.cpu.add(a, b, carry, set),
            0x6 => self.cpu.sub(a, b, carry, set),
            0x7 => self.cpu.sub(b, a, carry, set),
            0x8 => {
                logical(self, a & b);
                return self.restore_on_r15(flags, rd);
            }
            0x9 => {
                logical(self, a ^ b);
                return self.restore_on_r15(flags, rd);
            }
            0xa => {
                self.cpu.sub(a, b, true, set);
                return self.restore_on_r15(flags, rd);
            }
            0xb => {
                self.cpu.add(a, b, false, set);
                return self.restore_on_r15(flags, rd);
            }
            0xc => logical(self, a | b),
            0xd => logical(self, b),
            0xe => logical(self, a & !b),
            _ => logical(self, !b),
        };
        if rd == 15 {
            if flags {
                self.cpu.set_cpsr(self.cpu.spsr());
            }
            self.jump(result);
        } else {
            self.cpu.r[rd] = result;
        }
    }

    /// A comparison with S and r15 as its destination puts the SPSR back
    /// in the CPSR (an old way to return from an exception).
    fn restore_on_r15(&mut self, flags: bool, rd: usize) {
        if flags && rd == 15 {
            self.cpu.set_cpsr(self.cpu.spsr());
        }
    }

    fn arm_mrs(&mut self, op: u32) {
        let value = if op & 1 << 22 != 0 { self.cpu.spsr() } else { self.cpu.cpsr };
        self.cpu.r[(op >> 12 & 15) as usize] = value;
    }

    fn arm_msr(&mut self, op: u32) {
        let value =
            if op & 1 << 25 != 0 { (op & 0xff).rotate_right(op >> 7 & 0x1e) } else { self.cpu.r[op as usize & 15] };
        // The flags (bits 31-28) and control bits (7-0) are all the
        // ARM7TDMI has; user mode may only change the flags.
        let mut mask = 0;
        if op & 1 << 19 != 0 {
            mask |= 0xf000_0000;
        }
        if op & 1 << 16 != 0 {
            mask |= 0xff;
        }
        if op & 1 << 22 != 0 {
            let spsr = self.cpu.spsr();
            self.cpu.set_spsr(spsr & !mask | value & mask);
        } else {
            if self.cpu.mode() == crate::cpu::USER {
                mask &= 0xf000_0000;
            }
            // MSR does not switch between ARM and Thumb.
            mask &= !T;
            self.cpu.set_cpsr(self.cpu.cpsr & !mask | value & mask);
        }
    }

    fn arm_multiply(&mut self, op: u32) {
        let (rd, rn) = ((op >> 16 & 15) as usize, (op >> 12 & 15) as usize);
        let (rs, rm) = (self.cpu.r[(op >> 8 & 15) as usize], self.cpu.r[op as usize & 15]);
        let mut result = rm.wrapping_mul(rs);
        let mut cycles = multiply_cycles(rs, true);
        if op & 1 << 21 != 0 {
            result = result.wrapping_add(self.cpu.r[rn]);
            cycles += 1;
        }
        self.idle(cycles);
        if op & 1 << 20 != 0 {
            self.cpu.set_nz(result);
        }
        self.set_reg(rd, result);
    }

    fn arm_multiply_long(&mut self, op: u32) {
        let (hi, lo) = ((op >> 16 & 15) as usize, (op >> 12 & 15) as usize);
        let (rs, rm) = (self.cpu.r[(op >> 8 & 15) as usize], self.cpu.r[op as usize & 15]);
        let signed = op & 1 << 22 != 0;
        let mut result =
            if signed { (i64::from(rm as i32) * i64::from(rs as i32)) as u64 } else { u64::from(rm) * u64::from(rs) };
        let mut cycles = multiply_cycles(rs, signed) + 1;
        if op & 1 << 21 != 0 {
            result = result.wrapping_add(u64::from(self.cpu.r[hi]) << 32 | u64::from(self.cpu.r[lo]));
            cycles += 1;
        }
        self.idle(cycles);
        if op & 1 << 20 != 0 {
            self.cpu.set_flag(crate::cpu::N, result >> 63 != 0);
            self.cpu.set_flag(crate::cpu::Z, result == 0);
        }
        self.set_reg(lo, result as u32);
        self.set_reg(hi, (result >> 32) as u32);
    }

    /// LDR, STR, LDRB and STRB: an offset from the opcode or a shifted
    /// register, added or taken off before or after the access.
    fn arm_single_transfer(&mut self, op: u32) {
        let (rn, rd) = ((op >> 16 & 15) as usize, (op >> 12 & 15) as usize);
        let offset = if op & 1 << 25 == 0 {
            op & 0xfff
        } else {
            shift_by_immediate(op >> 5 & 3, op >> 7 & 31, self.cpu.r[op as usize & 15], self.cpu.flag(C)).0
        };
        let (pre, up, byte, load) = (op & 1 << 24 != 0, op & 1 << 23 != 0, op & 1 << 22 != 0, op & 1 << 20 != 0);
        let base = self.cpu.r[rn];
        let moved = if up { base.wrapping_add(offset) } else { base.wrapping_sub(offset) };
        let addr = if pre { moved } else { base };
        // Post-indexing always writes the base back.
        let writeback = !pre || op & 1 << 21 != 0;
        if load {
            let value = if byte { u32::from(self.read8(addr, false)) } else { self.read32_rotated(addr, false) };
            if writeback {
                self.cpu.r[rn] = moved;
            }
            self.idle(1);
            self.set_reg(rd, value);
        } else {
            // r15 is stored 12 ahead.
            let value = if rd == 15 { self.cpu.r[15].wrapping_add(4) } else { self.cpu.r[rd] };
            if byte {
                self.write8(addr, value as u8, false);
            } else {
                self.write32(addr, value, false);
            }
            self.cpu.seq = false;
            if writeback {
                self.set_reg(rn, moved);
            }
        }
    }

    /// LDRH, STRH, LDRSB and LDRSH.
    fn arm_halfword_transfer(&mut self, op: u32) {
        let (rn, rd) = ((op >> 16 & 15) as usize, (op >> 12 & 15) as usize);
        let offset = if op & 1 << 22 != 0 { op >> 4 & 0xf0 | op & 0xf } else { self.cpu.r[op as usize & 15] };
        let (pre, up, load) = (op & 1 << 24 != 0, op & 1 << 23 != 0, op & 1 << 20 != 0);
        let base = self.cpu.r[rn];
        let moved = if up { base.wrapping_add(offset) } else { base.wrapping_sub(offset) };
        let addr = if pre { moved } else { base };
        let writeback = !pre || op & 1 << 21 != 0;
        if load {
            let value = match op >> 5 & 3 {
                1 => self.read16_rotated(addr, false),
                2 => self.read8(addr, false) as i8 as u32,
                _ => self.read16_signed(addr, false),
            };
            if writeback {
                self.cpu.r[rn] = moved;
            }
            self.idle(1);
            self.set_reg(rd, value);
        } else {
            // Only STRH stores (the others are doubleword transfers of
            // later ARMs).
            if op >> 5 & 3 == 1 {
                let value = if rd == 15 { self.cpu.r[15].wrapping_add(4) } else { self.cpu.r[rd] };
                self.write16(addr, value as u16, false);
            }
            self.cpu.seq = false;
            if writeback {
                self.set_reg(rn, moved);
            }
        }
    }

    /// SWP and SWPB: a load and a store in one, holding the bus.
    fn arm_swap(&mut self, op: u32) {
        let addr = self.cpu.r[(op >> 16 & 15) as usize];
        let (rd, rm) = ((op >> 12 & 15) as usize, self.cpu.r[op as usize & 15]);
        let value = if op & 1 << 22 != 0 {
            let value = self.read8(addr, false);
            self.write8(addr, rm as u8, false);
            u32::from(value)
        } else {
            let value = self.read32_rotated(addr, false);
            self.write32(addr, rm, false);
            value
        };
        self.idle(1);
        self.set_reg(rd, value);
    }

    /// LDM and STM: the listed registers, lowest at the lowest address. An
    /// empty list moves r15 alone, and the base by 64 bytes.
    fn arm_block_transfer(&mut self, op: u32) {
        let rn = (op >> 16 & 15) as usize;
        let list = op & 0xffff;
        let (pre, up, user, writeback, load) =
            (op & 1 << 24 != 0, op & 1 << 23 != 0, op & 1 << 22 != 0, op & 1 << 21 != 0, op & 1 << 20 != 0);
        let (list, bytes) = if list == 0 { (1 << 15, 64) } else { (list, 4 * list.count_ones()) };
        let base = self.cpu.r[rn];
        let end = if up { base.wrapping_add(bytes) } else { base.wrapping_sub(bytes) };
        // Lowest address first, whichever way the base moves.
        let mut addr = match (pre, up) {
            (false, true) => base,
            (true, true) => base.wrapping_add(4),
            (false, false) => end.wrapping_add(4),
            (true, false) => end,
        };
        // With ^, user mode's registers, unless it is LDM of r15 (which
        // puts the SPSR back instead).
        let user_bank = user && !(load && list & 1 << 15 != 0);
        let mut seq = false;
        if load {
            if writeback {
                self.cpu.r[rn] = end;
            }
            for n in (0..16).filter(|n| list & 1 << n != 0) {
                let value = self.read32(addr, seq);
                seq = true;
                addr = addr.wrapping_add(4);
                if n == 15 {
                    if user {
                        self.cpu.set_cpsr(self.cpu.spsr());
                    }
                    self.idle(1);
                    self.jump(value);
                    return;
                } else if user_bank {
                    self.cpu.set_user_reg(n, value);
                } else {
                    self.cpu.r[n] = value;
                }
            }
            self.idle(1);
        } else {
            // The base goes back after the first store: a base listed first
            // is stored as it was, one listed later as written back.
            for (i, n) in (0..16).filter(|n| list & 1 << n != 0).enumerate() {
                let value = match n {
                    15 => self.cpu.r[15].wrapping_add(4),
                    _ if user_bank => self.cpu.user_reg(n),
                    _ => self.cpu.r[n],
                };
                self.write32(addr, value, seq);
                seq = true;
                addr = addr.wrapping_add(4);
                if i == 0 && writeback {
                    self.cpu.r[rn] = end;
                }
            }
            self.cpu.seq = false;
        }
    }
}
