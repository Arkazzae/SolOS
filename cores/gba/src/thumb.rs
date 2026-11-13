//! Thumb instructions: 16-bit opcodes, most of them short forms of ARM's.

use crate::Gba;
use crate::cpu::{C, T, multiply_cycles, shift_by_immediate, shift_by_register};

impl Gba {
    pub(crate) fn step_thumb(&mut self) {
        let op = self.cpu.pipe[0] as u16;
        self.cpu.pipe[0] = self.cpu.pipe[1];
        let pc = self.cpu.r[15];
        self.cpu.pipe[1] = u32::from(self.fetch16(pc));
        self.execute_thumb(op);
        self.advance(pc, 2);
    }

    fn execute_thumb(&mut self, op: u16) {
        let op = u32::from(op);
        match op >> 8 {
            0x00..=0x17 => self.thumb_shift(op),
            0x18..=0x1f => self.thumb_add_sub(op),
            0x20..=0x3f => self.thumb_immediate(op),
            0x40..=0x43 => self.thumb_alu(op),
            0x44..=0x47 => self.thumb_high(op),
            0x48..=0x4f => {
                let addr = (self.cpu.r[15] & !3).wrapping_add((op & 0xff) << 2);
                let value = self.read32(addr, false);
                self.idle(1);
                self.cpu.r[(op >> 8 & 7) as usize] = value;
            }
            0x50..=0x5f => self.thumb_register_offset(op),
            0x60..=0x8f => self.thumb_immediate_offset(op),
            0x90..=0x9f => {
                let rd = (op >> 8 & 7) as usize;
                let addr = self.cpu.r[13].wrapping_add((op & 0xff) << 2);
                self.load_store(addr, rd, op & 1 << 11 != 0, 4);
            }
            0xa0..=0xaf => {
                let base = if op & 1 << 11 != 0 { self.cpu.r[13] } else { self.cpu.r[15] & !3 };
                self.cpu.r[(op >> 8 & 7) as usize] = base.wrapping_add((op & 0xff) << 2);
            }
            0xb0 => {
                let offset = (op & 0x7f) << 2;
                let sp = self.cpu.r[13];
                self.cpu.r[13] = if op & 0x80 != 0 { sp.wrapping_sub(offset) } else { sp.wrapping_add(offset) };
            }
            0xb4 | 0xb5 => self.thumb_push(op),
            0xbc | 0xbd => self.thumb_pop(op),
            0xc0..=0xcf => self.thumb_multiple(op),
            0xd0..=0xdd => {
                if self.cpu.condition(op >> 8 & 15) {
                    let offset = (op as i8 as i32 as u32) << 1;
                    self.jump(self.cpu.r[15].wrapping_add(offset));
                    if offset as i32 <= -4 {
                        self.loop_branch();
                    }
                }
            }
            0xdf => self.swi(op & 0xff),
            0xe0..=0xe7 => {
                let offset = ((op << 21) as i32 >> 20) as u32;
                self.jump(self.cpu.r[15].wrapping_add(offset));
                if offset as i32 <= -4 {
                    self.loop_branch();
                }
            }
            // BL, in two halves: the first puts the top of the offset in
            // LR, the second branches from there and leaves the return
            // address (with bit 0 set, for BX) in LR.
            0xf0..=0xf7 => {
                let offset = ((op << 21) as i32 >> 9) as u32;
                self.cpu.r[14] = self.cpu.r[15].wrapping_add(offset);
            }
            0xf8..=0xff => {
                let back = self.cpu.r[15].wrapping_sub(2) | 1;
                let target = self.cpu.r[14].wrapping_add((op & 0x7ff) << 1);
                self.cpu.r[14] = back;
                self.jump(target);
            }
            _ => self.undefined(),
        }
    }

    /// LSL, LSR and ASR by an amount in the opcode.
    fn thumb_shift(&mut self, op: u32) {
        let value = self.cpu.r[(op >> 3 & 7) as usize];
        let (result, carry) = shift_by_immediate(op >> 11, op >> 6 & 31, value, self.cpu.flag(C));
        self.cpu.set_nz(result);
        self.cpu.set_flag(C, carry);
        self.cpu.r[op as usize & 7] = result;
    }

    fn thumb_add_sub(&mut self, op: u32) {
        let a = self.cpu.r[(op >> 3 & 7) as usize];
        let b = if op & 1 << 10 != 0 { op >> 6 & 7 } else { self.cpu.r[(op >> 6 & 7) as usize] };
        let result = if op & 1 << 9 != 0 { self.cpu.sub(a, b, true, true) } else { self.cpu.add(a, b, false, true) };
        self.cpu.r[op as usize & 7] = result;
    }

    /// MOV, CMP, ADD and SUB with an 8-bit immediate.
    fn thumb_immediate(&mut self, op: u32) {
        let rd = (op >> 8 & 7) as usize;
        let (a, b) = (self.cpu.r[rd], op & 0xff);
        match op >> 11 & 3 {
            0 => {
                self.cpu.set_nz(b);
                self.cpu.r[rd] = b;
            }
            1 => {
                self.cpu.sub(a, b, true, true);
            }
            2 => self.cpu.r[rd] = self.cpu.add(a, b, false, true),
            _ => self.cpu.r[rd] = self.cpu.sub(a, b, true, true),
        }
    }

    fn thumb_alu(&mut self, op: u32) {
        let rd = op as usize & 7;
        let (a, b) = (self.cpu.r[rd], self.cpu.r[(op >> 3 & 7) as usize]);
        let carry = self.cpu.flag(C);
        let shift = |gba: &mut Gba, kind: u32| {
            gba.idle(1);
            let (result, carry) = shift_by_register(kind, b & 0xff, a, carry);
            gba.cpu.set_flag(C, carry);
            result
        };
        let result = match op >> 6 & 15 {
            0x0 => a & b,
            0x1 => a ^ b,
            0x2 => shift(self, 0),
            0x3 => shift(self, 1),
            0x4 => shift(self, 2),
            0x5 => self.cpu.add(a, b, carry, true),
            0x6 => self.cpu.sub(a, b, carry, true),
            0x7 => shift(self, 3),
            0x8 => {
                self.cpu.set_nz(a & b);
                return;
            }
            0x9 => self.cpu.sub(0, b, true, true),
            0xa => {
                self.cpu.sub(a, b, true, true);
                return;
            }
            0xb => {
                self.cpu.add(a, b, false, true);
                return;
            }
            0xc => a | b,
            0xd => {
                self.idle(multiply_cycles(a, true));
                a.wrapping_mul(b)
            }
            0xe => a & !b,
            _ => !b,
        };
        self.cpu.set_nz(result);
        self.cpu.r[rd] = result;
    }

    /// ADD, CMP and MOV reaching r8-r15, and BX.
    fn thumb_high(&mut self, op: u32) {
        let rd = (op & 7 | op >> 4 & 8) as usize;
        let b = self.cpu.r[(op >> 3 & 15) as usize];
        match op >> 8 & 3 {
            0 => {
                let result = self.cpu.r[rd].wrapping_add(b);
                self.set_reg(rd, result);
            }
            1 => {
                self.cpu.sub(self.cpu.r[rd], b, true, true);
            }
            2 => self.set_reg(rd, b),
            _ => {
                self.cpu.set_flag(T, b & 1 != 0);
                self.jump(b);
            }
        }
    }

    fn thumb_register_offset(&mut self, op: u32) {
        let addr = self.cpu.r[(op >> 3 & 7) as usize].wrapping_add(self.cpu.r[(op >> 6 & 7) as usize]);
        let rd = op as usize & 7;
        match op >> 9 & 7 {
            0 => self.load_store(addr, rd, false, 4),
            1 => self.load_store(addr, rd, false, 2),
            2 => self.load_store(addr, rd, false, 1),
            3 => self.load_signed(addr, rd, 1),
            4 => self.load_store(addr, rd, true, 4),
            5 => self.load_store(addr, rd, true, 2),
            6 => self.load_store(addr, rd, true, 1),
            _ => self.load_signed(addr, rd, 2),
        }
    }

    fn thumb_immediate_offset(&mut self, op: u32) {
        let base = self.cpu.r[(op >> 3 & 7) as usize];
        let (offset, load, rd) = (op >> 6 & 31, op & 1 << 11 != 0, op as usize & 7);
        match op >> 12 {
            6 => self.load_store(base.wrapping_add(offset << 2), rd, load, 4),
            7 => self.load_store(base.wrapping_add(offset), rd, load, 1),
            _ => self.load_store(base.wrapping_add(offset << 1), rd, load, 2),
        }
    }

    fn load_store(&mut self, addr: u32, rd: usize, load: bool, size: u32) {
        if load {
            let value = match size {
                4 => self.read32_rotated(addr, false),
                2 => self.read16_rotated(addr, false),
                _ => u32::from(self.read8(addr, false)),
            };
            self.idle(1);
            self.cpu.r[rd] = value;
        } else {
            let value = self.cpu.r[rd];
            match size {
                4 => self.write32(addr, value, false),
                2 => self.write16(addr, value as u16, false),
                _ => self.write8(addr, value as u8, false),
            }
            self.cpu.seq = false;
        }
    }

    fn load_signed(&mut self, addr: u32, rd: usize, size: u32) {
        let value = if size == 1 { self.read8(addr, false) as i8 as u32 } else { self.read16_signed(addr, false) };
        self.idle(1);
        self.cpu.r[rd] = value;
    }

    /// PUSH: the listed registers (and LR with bit 8) below SP. An empty
    /// list pushes r15 alone, moving SP by 64.
    fn thumb_push(&mut self, op: u32) {
        let mut list = op & 0xff | (op & 0x100) << 6;
        let bytes = if list == 0 { 64 } else { 4 * list.count_ones() };
        if list == 0 {
            list = 1 << 15;
        }
        let start = self.cpu.r[13].wrapping_sub(bytes);
        self.cpu.r[13] = start;
        self.store_list(start, list);
    }

    fn thumb_pop(&mut self, op: u32) {
        let mut list = op & 0xff | (op & 0x100) << 7;
        let bytes = if list == 0 { 64 } else { 4 * list.count_ones() };
        if list == 0 {
            list = 1 << 15;
        }
        let start = self.cpu.r[13];
        self.cpu.r[13] = start.wrapping_add(bytes);
        self.load_list(start, list);
    }

    /// STMIA and LDMIA with writeback. A base listed (and loaded) is not
    /// written back; one stored after the first register is stored written
    /// back.
    fn thumb_multiple(&mut self, op: u32) {
        let rb = (op >> 8 & 7) as usize;
        let list = op & 0xff;
        let base = self.cpu.r[rb];
        let (list, bytes) = if list == 0 { (1 << 15, 64) } else { (list, 4 * list.count_ones()) };
        let end = base.wrapping_add(bytes);
        if op & 1 << 11 != 0 {
            self.cpu.r[rb] = end;
            self.load_list(base, list);
        } else {
            let mut addr = base;
            let mut seq = false;
            for (i, n) in (0..16).filter(|n| list & 1 << n != 0).enumerate() {
                let value = if n == 15 { self.cpu.r[15].wrapping_add(2) } else { self.cpu.r[n] };
                self.write32(addr, value, seq);
                seq = true;
                addr = addr.wrapping_add(4);
                if i == 0 {
                    self.cpu.r[rb] = end;
                }
            }
            self.cpu.seq = false;
        }
    }

    fn store_list(&mut self, mut addr: u32, list: u32) {
        let mut seq = false;
        for n in (0..16).filter(|n| list & 1 << n != 0) {
            let value = if n == 15 { self.cpu.r[15].wrapping_add(2) } else { self.cpu.r[n] };
            self.write32(addr, value, seq);
            seq = true;
            addr = addr.wrapping_add(4);
        }
        self.cpu.seq = false;
    }

    fn load_list(&mut self, mut addr: u32, list: u32) {
        let mut seq = false;
        for n in (0..16).filter(|n| list & 1 << n != 0) {
            let value = self.read32(addr, seq);
            seq = true;
            addr = addr.wrapping_add(4);
            if n == 15 {
                self.idle(1);
                self.jump(value);
                return;
            }
            self.cpu.r[n] = value;
        }
        self.idle(1);
    }
}
