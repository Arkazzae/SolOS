//! Motorola 68000 interpreter. The bus is big endian, as is the processor.
//!
//! Instruction semantics follow Motorola's M68000 Family Programmer's
//! Reference Manual. Timing is instruction-level, with approximate EA costs;
//! this is not a bus-cycle-accurate implementation.
use savestate::state;

#[cfg(test)]
mod singlestep;

const C: u16 = 1;
const V: u16 = 2;
const Z: u16 = 4;
const N: u16 = 8;
const X: u16 = 16;
const S: u16 = 0x2000;

pub(crate) trait Bus {
    fn read8(&mut self, addr: u32) -> u8;
    fn write8(&mut self, addr: u32, value: u8);
    fn read16(&mut self, addr: u32) -> u16 {
        u16::from_be_bytes([self.read8(addr), self.read8(addr.wrapping_add(1))])
    }
    fn read32(&mut self, addr: u32) -> u32 {
        (u32::from(self.read16(addr)) << 16) | u32::from(self.read16(addr.wrapping_add(2)))
    }
    fn write16(&mut self, addr: u32, value: u16) {
        let [hi, lo] = value.to_be_bytes();
        self.write8(addr, hi);
        self.write8(addr.wrapping_add(1), lo);
    }
    fn write32(&mut self, addr: u32, value: u32) {
        self.write16(addr, (value >> 16) as u16);
        self.write16(addr.wrapping_add(2), value as u16);
    }
    fn fetch16(&mut self, addr: u32) -> u16 {
        self.read16(addr)
    }
    fn reset_devices(&mut self) {}
}

#[derive(Clone, Copy)]
struct Fault {
    addr: u32,
    read: bool,
    instruction: bool,
}
/// Stops bus effects at the first misaligned access. In particular, forwarding
/// word accesses intact preserves the VDP data/control port side effects.
struct CheckedBus<'a, B: Bus> {
    bus: &'a mut B,
    fault: Option<Fault>,
}
impl<B: Bus> CheckedBus<'_, B> {
    fn aligned(&mut self, addr: u32, read: bool, instruction: bool) -> bool {
        if self.fault.is_some() {
            return false;
        }
        if addr & 1 != 0 {
            self.fault = Some(Fault {
                addr,
                read,
                instruction,
            });
            return false;
        }
        true
    }
}
impl<B: Bus> Bus for CheckedBus<'_, B> {
    fn read8(&mut self, addr: u32) -> u8 {
        if self.fault.is_some() {
            0
        } else {
            self.bus.read8(addr)
        }
    }
    fn write8(&mut self, addr: u32, value: u8) {
        if self.fault.is_none() {
            self.bus.write8(addr, value);
        }
    }
    fn read16(&mut self, addr: u32) -> u16 {
        if self.aligned(addr, true, false) {
            self.bus.read16(addr)
        } else {
            0
        }
    }
    fn read32(&mut self, addr: u32) -> u32 {
        if self.aligned(addr, true, false) {
            self.bus.read32(addr)
        } else {
            0
        }
    }
    fn write16(&mut self, addr: u32, value: u16) {
        if self.aligned(addr, false, false) {
            self.bus.write16(addr, value);
        }
    }
    fn write32(&mut self, addr: u32, value: u32) {
        if self.aligned(addr, false, false) {
            self.bus.write32(addr, value);
        }
    }
    fn fetch16(&mut self, addr: u32) -> u16 {
        if self.aligned(addr, true, true) {
            self.bus.fetch16(addr)
        } else {
            0
        }
    }
    fn reset_devices(&mut self) {
        if self.fault.is_none() {
            self.bus.reset_devices();
        }
    }
}

#[derive(Clone)]
pub(crate) struct Cpu {
    pub(crate) d: [u32; 8],
    pub(crate) a: [u32; 8],
    pub(crate) pc: u32,
    pub(crate) sr: u16,
    pub(crate) stopped: bool,
    usp: u32,
    ssp: u32,
    pub(crate) instructions: u64,
    halted: bool,
    // Ephemeral: a trace exception must not follow an instruction exception.
    exception_taken: bool,
}
state!(Cpu {
    d,
    a,
    pc,
    sr,
    stopped,
    usp,
    ssp,
    instructions,
    halted
});

#[derive(Clone, Copy, PartialEq, Debug)]
enum Size {
    Byte,
    Word,
    Long,
}
impl Size {
    fn bytes(self) -> u32 {
        match self {
            Self::Byte => 1,
            Self::Word => 2,
            Self::Long => 4,
        }
    }
    fn mask(self) -> u32 {
        match self {
            Self::Byte => 0xff,
            Self::Word => 0xffff,
            Self::Long => u32::MAX,
        }
    }
    fn sign(self) -> u32 {
        match self {
            Self::Byte => 0x80,
            Self::Word => 0x8000,
            Self::Long => 0x8000_0000,
        }
    }
    fn decode(op: u16) -> Option<Self> {
        match (op >> 6) & 3 {
            0 => Some(Self::Byte),
            1 => Some(Self::Word),
            2 => Some(Self::Long),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug)]
enum Ea {
    D(usize),
    A(usize),
    Mem(u32),
    Imm(u32),
    Invalid,
}
impl Ea {
    fn memory(self) -> bool {
        matches!(self, Self::Mem(_))
    }
    fn writable(self) -> bool {
        matches!(self, Self::D(_) | Self::A(_) | Self::Mem(_))
    }
}

impl Cpu {
    pub(crate) fn new() -> Self {
        Self {
            d: [0; 8],
            a: [0; 8],
            pc: 0,
            sr: 0x2700,
            stopped: false,
            usp: 0,
            ssp: 0,
            instructions: 0,
            halted: false,
            exception_taken: false,
        }
    }
    pub(crate) fn reset(&mut self, bus: &mut impl Bus) {
        *self = Self::new();
        self.a[7] = bus.read32(0);
        self.ssp = self.a[7];
        self.pc = bus.read32(4);
    }
    fn word(&mut self, bus: &mut impl Bus) -> u16 {
        let x = bus.fetch16(self.pc);
        self.pc = self.pc.wrapping_add(2);
        x
    }
    fn long(&mut self, bus: &mut impl Bus) -> u32 {
        let hi = self.word(bus) as u32;
        (hi << 16) | self.word(bus) as u32
    }
    fn immediate(&mut self, bus: &mut impl Bus, size: Size) -> u32 {
        if size == Size::Long {
            self.long(bus)
        } else {
            self.word(bus) as u32 & size.mask()
        }
    }
    fn set_sr(&mut self, value: u16) {
        let value = value & 0xa71f;
        if (self.sr ^ value) & S != 0 {
            if value & S != 0 {
                self.usp = self.a[7];
                self.a[7] = self.ssp;
            } else {
                self.ssp = self.a[7];
                self.a[7] = self.usp;
            }
        }
        self.sr = value;
    }
    fn push16(&mut self, bus: &mut impl Bus, value: u16) {
        self.a[7] = self.a[7].wrapping_sub(2);
        bus.write16(self.a[7], value);
    }
    fn push32(&mut self, bus: &mut impl Bus, value: u32) {
        self.a[7] = self.a[7].wrapping_sub(4);
        bus.write32(self.a[7], value);
    }
    fn pop16(&mut self, bus: &mut impl Bus) -> u16 {
        let value = bus.read16(self.a[7]);
        self.a[7] = self.a[7].wrapping_add(2);
        value
    }
    fn pop32(&mut self, bus: &mut impl Bus) -> u32 {
        let value = bus.read32(self.a[7]);
        self.a[7] = self.a[7].wrapping_add(4);
        value
    }
    fn exception(&mut self, bus: &mut impl Bus, vector: u32, pc: u32) -> u32 {
        self.exception_taken = true;
        let old_sr = self.sr;
        self.set_sr((self.sr | S) & !0x8000);
        self.push32(bus, pc);
        self.push16(bus, old_sr);
        self.pc = bus.read32(vector * 4);
        self.stopped = false;
        34
    }
    pub(crate) fn interrupt(&mut self, bus: &mut impl Bus, level: u8) -> u32 {
        if self.halted
            || level == 0
            || level > 7
            || (level != 7 && level <= ((self.sr >> 8) & 7) as u8)
        {
            return 0;
        }
        let cycles = self.exception(bus, 24 + level as u32, self.pc);
        self.sr = (self.sr & !0x0700) | ((level as u16) << 8);
        cycles + 10
    }
    fn indexed(&mut self, bus: &mut impl Bus, base: u32) -> u32 {
        let ext = self.word(bus);
        let reg = ((ext >> 12) & 7) as usize;
        let index = if ext & 0x8000 != 0 {
            self.a[reg]
        } else {
            self.d[reg]
        };
        let index = if ext & 0x0800 == 0 {
            index as i16 as i32 as u32
        } else {
            index
        };
        base.wrapping_add(index)
            .wrapping_add(ext as i8 as i32 as u32)
    }
    fn ea(&mut self, bus: &mut impl Bus, mode: u16, reg: u16, size: Size) -> Ea {
        let r = reg as usize;
        match mode {
            0 => Ea::D(r),
            1 => Ea::A(r),
            2 => Ea::Mem(self.a[r]),
            3 => {
                let addr = self.a[r];
                self.a[r] = addr.wrapping_add(if r == 7 && size == Size::Byte {
                    2
                } else {
                    size.bytes()
                });
                Ea::Mem(addr)
            }
            4 => {
                self.a[r] = self.a[r].wrapping_sub(if r == 7 && size == Size::Byte {
                    2
                } else {
                    size.bytes()
                });
                Ea::Mem(self.a[r])
            }
            5 => {
                let d = self.word(bus) as i16 as i32 as u32;
                Ea::Mem(self.a[r].wrapping_add(d))
            }
            6 => Ea::Mem(self.indexed(bus, self.a[r])),
            7 => match reg {
                0 => Ea::Mem(self.word(bus) as i16 as i32 as u32),
                1 => Ea::Mem(self.long(bus)),
                2 => {
                    let base = self.pc;
                    let d = self.word(bus) as i16 as i32 as u32;
                    Ea::Mem(base.wrapping_add(d))
                }
                3 => {
                    let base = self.pc;
                    Ea::Mem(self.indexed(bus, base))
                }
                4 => Ea::Imm(self.immediate(bus, size)),
                _ => Ea::Invalid,
            },
            _ => Ea::Invalid,
        }
    }
    fn control_ea(&mut self, bus: &mut impl Bus, op: u16) -> Ea {
        let mode = (op >> 3) & 7;
        let reg = op & 7;
        if matches!(mode, 2 | 5 | 6) || (mode == 7 && reg <= 3) {
            self.ea(bus, mode, reg, Size::Long)
        } else {
            Ea::Invalid
        }
    }
    fn read(&mut self, bus: &mut impl Bus, ea: Ea, size: Size) -> u32 {
        let value = match ea {
            Ea::D(r) => self.d[r],
            Ea::A(r) => self.a[r],
            Ea::Imm(x) => x,
            Ea::Mem(addr) => match size {
                Size::Byte => bus.read8(addr) as u32,
                Size::Word => bus.read16(addr) as u32,
                Size::Long => bus.read32(addr),
            },
            Ea::Invalid => 0,
        };
        value & size.mask()
    }
    fn write(&mut self, bus: &mut impl Bus, ea: Ea, size: Size, value: u32) {
        let value = value & size.mask();
        match ea {
            Ea::D(r) => self.d[r] = (self.d[r] & !size.mask()) | value,
            Ea::A(r) => {
                self.a[r] = if size == Size::Word {
                    value as i16 as i32 as u32
                } else {
                    value
                }
            }
            Ea::Mem(addr) => match size {
                Size::Byte => bus.write8(addr, value as u8),
                Size::Word => bus.write16(addr, value as u16),
                Size::Long => bus.write32(addr, value),
            },
            _ => {}
        }
    }
    fn ea_cycles(op: u16, size: Size) -> u32 {
        let extra = if size == Size::Long { 4 } else { 0 };
        match (op >> 3) & 7 {
            0 | 1 => 0,
            2 | 3 => 4 + extra,
            4 => 6 + extra,
            5 => 8 + extra,
            6 => 10 + extra,
            _ => match op & 7 {
                0 | 2 => 8 + extra,
                1 => 12 + extra,
                3 => 10 + extra,
                4 => 4 + extra,
                _ => 0,
            },
        }
    }
    fn logical(&mut self, value: u32, size: Size) {
        let v = value & size.mask();
        self.sr &= !(N | Z | V | C);
        if v == 0 {
            self.sr |= Z;
        }
        if v & size.sign() != 0 {
            self.sr |= N;
        }
    }
    fn add(&mut self, dst: u32, src: u32, size: Size, extend: bool) -> u32 {
        let dst = dst & size.mask();
        let src = src & size.mask();
        let x = u64::from(extend && self.sr & X != 0);
        let wide = dst as u64 + src as u64 + x;
        let result = wide as u32 & size.mask();
        let old_z = self.sr & Z;
        self.logical(result, size);
        if (!(dst ^ src) & (dst ^ result)) & size.sign() != 0 {
            self.sr |= V;
        }
        self.sr &= !X;
        if wide > size.mask() as u64 {
            self.sr |= C | X;
        }
        if extend && old_z == 0 {
            self.sr &= !Z;
        }
        result
    }
    fn sub(&mut self, dst: u32, src: u32, size: Size, extend: bool, compare: bool) -> u32 {
        let dst = dst & size.mask();
        let src = src & size.mask();
        let x = u64::from(extend && self.sr & X != 0);
        let result = dst.wrapping_sub(src).wrapping_sub(x as u32) & size.mask();
        let old_z = self.sr & Z;
        let old_x = self.sr & X;
        self.logical(result, size);
        if ((dst ^ src) & (dst ^ result)) & size.sign() != 0 {
            self.sr |= V;
        }
        self.sr &= !X;
        if (src as u64 + x) > dst as u64 {
            self.sr |= C | X;
        }
        if compare {
            self.sr = (self.sr & !X) | old_x;
        }
        if extend && old_z == 0 {
            self.sr &= !Z;
        }
        result
    }
    fn condition(&self, condition: u16) -> bool {
        let c = self.sr & C != 0;
        let v = self.sr & V != 0;
        let z = self.sr & Z != 0;
        let n = self.sr & N != 0;
        match condition {
            0 => true,
            1 => false,
            2 => !c && !z,
            3 => c || z,
            4 => !c,
            5 => c,
            6 => !z,
            7 => z,
            8 => !v,
            9 => v,
            10 => !n,
            11 => n,
            12 => n == v,
            13 => n != v,
            14 => !z && n == v,
            _ => z || n != v,
        }
    }
    fn illegal(&mut self, bus: &mut impl Bus, pc: u32) -> u32 {
        self.exception(bus, 4, pc)
    }
    fn privilege(&mut self, bus: &mut impl Bus, pc: u32) -> bool {
        if self.sr & S != 0 {
            true
        } else {
            self.exception(bus, 8, pc);
            false
        }
    }
    pub(crate) fn step(&mut self, bus: &mut impl Bus) -> u32 {
        if self.stopped || self.halted {
            return 4;
        }
        let before = self.clone();
        self.exception_taken = false;
        let pc = self.pc;
        let trace = self.sr & 0x8000 != 0;
        let mut checked = CheckedBus { bus, fault: None };
        let op = self.word(&mut checked);
        self.instructions = self.instructions.wrapping_add(1);
        let cycles = if checked.fault.is_none() {
            self.execute(&mut checked, op, pc)
        } else {
            0
        };
        if let Some(fault) = checked.fault {
            // No later bus access after the offending access reaches hardware.
            // Register rollback is deterministic; 68000 bus-prefetch/internal
            // register changes on these faults are not modelled cycle by cycle.
            *self = before;
            self.instructions = self.instructions.wrapping_add(1);
            return self.address_error(bus, fault, op, pc);
        }
        if trace && !self.exception_taken {
            self.exception(bus, 9, self.pc);
            return cycles + 34;
        }
        cycles
    }
    fn address_error(&mut self, bus: &mut impl Bus, fault: Fault, ir: u16, pc: u32) -> u32 {
        let old_sr = self.sr;
        self.set_sr((self.sr | S) & !0x8000);
        self.stopped = false;
        if self.a[7] & 1 != 0 {
            self.halted = true;
            return 50;
        }
        let function_code =
            (if old_sr & S != 0 { 4 } else { 0 }) | if fault.instruction { 2 } else { 1 };
        let status =
            function_code | if fault.read { 16 } else { 0 } | if fault.instruction { 0 } else { 8 };
        // The 68000's group-0 diagnostic frame is not a later CPU's
        // restartable frame: SSW, access address, IR, SR, PC (14 bytes).
        self.push32(bus, pc);
        self.push16(bus, old_sr);
        self.push16(bus, ir);
        self.push32(bus, fault.addr);
        self.push16(bus, status);
        self.pc = bus.read32(3 * 4);
        if self.pc & 1 != 0 {
            self.halted = true;
        }
        50
    }
    fn execute(&mut self, bus: &mut impl Bus, op: u16, pc: u32) -> u32 {
        let reg = ((op >> 9) & 7) as usize;
        let mode = (op >> 3) & 7;
        let r = op & 7;
        match op >> 12 {
            1..=3 => {
                let size = match op >> 12 {
                    1 => Size::Byte,
                    2 => Size::Long,
                    _ => Size::Word,
                };
                let dst_mode = (op >> 6) & 7;
                if (size == Size::Byte && (mode == 1 || dst_mode == 1))
                    || (mode == 7 && r > 4)
                    || (dst_mode == 7 && reg > 1)
                {
                    return self.illegal(bus, pc);
                }
                let src = self.ea(bus, mode, r, size);
                let value = self.read(bus, src, size);
                let dst = self.ea(bus, dst_mode, reg as u16, size);
                if !dst.writable() {
                    return self.illegal(bus, pc);
                }
                self.write(bus, dst, size, value);
                if dst_mode != 1 {
                    self.logical(value, size);
                }
                let dst_op = (dst_mode << 3) | reg as u16;
                4 + Self::ea_cycles(op, size) + Self::ea_cycles(dst_op, size)
            }
            0 => self.group0(bus, op, pc),
            4 => self.group4(bus, op, pc),
            5 => {
                if op & 0x00c0 == 0x00c0 {
                    let cc = (op >> 8) & 15;
                    if mode == 1 {
                        let base = self.pc;
                        let displacement = self.word(bus) as i16 as i32 as u32;
                        if self.condition(cc) {
                            return 12;
                        }
                        let value = (self.d[r as usize] as u16).wrapping_sub(1);
                        self.d[r as usize] = (self.d[r as usize] & 0xffff0000) | value as u32;
                        if value != 0xffff {
                            self.pc = base.wrapping_add(displacement);
                            10
                        } else {
                            14
                        }
                    } else {
                        if mode == 7 && r > 1 {
                            return self.illegal(bus, pc);
                        }
                        let ea = self.ea(bus, mode, r, Size::Byte);
                        self.write(
                            bus,
                            ea,
                            Size::Byte,
                            if self.condition(cc) { 0xff } else { 0 },
                        );
                        8 + Self::ea_cycles(op, Size::Byte)
                    }
                } else {
                    let Some(size) = Size::decode(op) else {
                        return self.illegal(bus, pc);
                    };
                    if (mode == 1 && size == Size::Byte) || (mode == 7 && r > 1) {
                        return self.illegal(bus, pc);
                    }
                    let quick = if reg == 0 { 8 } else { reg as u32 };
                    let ea = self.ea(bus, mode, r, size);
                    let actual_size = if mode == 1 { Size::Long } else { size };
                    let old = self.read(bus, ea, actual_size);
                    let value = if mode == 1 {
                        if op & 0x100 != 0 {
                            old.wrapping_sub(quick)
                        } else {
                            old.wrapping_add(quick)
                        }
                    } else if op & 0x100 != 0 {
                        self.sub(old, quick, size, false, false)
                    } else {
                        self.add(old, quick, size, false)
                    };
                    self.write(bus, ea, actual_size, value);
                    if mode == 1 || size == Size::Long {
                        8 + Self::ea_cycles(op, size)
                    } else {
                        4 + Self::ea_cycles(op, size) + if ea.memory() { 4 } else { 0 }
                    }
                }
            }
            6 => {
                let cc = (op >> 8) & 15;
                let base = self.pc;
                let displacement = if op as u8 == 0 {
                    self.word(bus) as i16 as i32 as u32
                } else {
                    op as i8 as i32 as u32
                };
                if cc == 1 {
                    self.push32(bus, self.pc);
                    self.pc = base.wrapping_add(displacement);
                    18
                } else if self.condition(cc) {
                    self.pc = base.wrapping_add(displacement);
                    10
                } else if op as u8 == 0 {
                    12
                } else {
                    8
                }
            }
            7 => {
                if op & 0x100 != 0 {
                    return self.illegal(bus, pc);
                }
                self.d[reg] = op as i8 as i32 as u32;
                self.logical(self.d[reg], Size::Long);
                4
            }
            8 | 9 | 11 | 12 | 13 => self.alu(bus, op, pc),
            14 => self.shifts(bus, op, pc),
            10 => self.exception(bus, 10, pc),
            15 => self.exception(bus, 11, pc),
            _ => self.illegal(bus, pc),
        }
    }
    fn group0(&mut self, bus: &mut impl Bus, op: u16, pc: u32) -> u32 {
        let mode = (op >> 3) & 7;
        let r = op & 7;
        if op & 0xf138 == 0x0108 {
            // MOVEP, bytes on alternating bus addresses.
            let reg = ((op >> 9) & 7) as usize;
            let displacement = self.word(bus) as i16 as i32 as u32;
            let addr = self.a[r as usize].wrapping_add(displacement);
            let long = op & 0x40 != 0;
            let count = if long { 4 } else { 2 };
            if op & 0x80 != 0 {
                for i in 0..count {
                    bus.write8(
                        addr.wrapping_add(i * 2),
                        (self.d[reg] >> ((count - i - 1) * 8)) as u8,
                    );
                }
            } else {
                let mut value = 0;
                for i in 0..count {
                    value = value << 8 | bus.read8(addr.wrapping_add(i * 2)) as u32;
                }
                self.d[reg] = if long {
                    value
                } else {
                    self.d[reg] & 0xffff0000 | value
                };
            }
            return if long { 24 } else { 16 };
        }
        if op & 0x0100 != 0 || op & 0xff00 == 0x0800 {
            let dynamic = op & 0x0100 != 0;
            let bit = if dynamic {
                self.d[((op >> 9) & 7) as usize]
            } else {
                self.word(bus) as u32
            };
            let operation = (op >> 6) & 3;
            if mode == 1
                || (mode == 7 && (r > (if dynamic { 4 } else { 3 }) || (operation != 0 && r > 1)))
            {
                return self.illegal(bus, pc);
            }
            let size = if mode == 0 { Size::Long } else { Size::Byte };
            let ea = self.ea(bus, mode, r, size);
            let mask = 1u32 << (bit % (size.bytes() * 8));
            let old = self.read(bus, ea, size);
            self.sr = (self.sr & !Z) | if old & mask == 0 { Z } else { 0 };
            let value = match operation {
                1 => old ^ mask,
                2 => old & !mask,
                3 => old | mask,
                _ => old,
            };
            if operation != 0 {
                self.write(bus, ea, size, value);
            }
            return (if dynamic { 4 } else { 8 })
                + Self::ea_cycles(op, size)
                + if operation != 0 { 4 } else { 0 };
        }
        let operation = (op >> 8) & 15;
        if !matches!(operation, 0 | 2 | 4 | 6 | 10 | 12) {
            return self.illegal(bus, pc);
        }
        if matches!(op, 0x003c | 0x023c | 0x0a3c | 0x007c | 0x027c | 0x0a7c) {
            let full = op & 0x40 != 0;
            if full && !self.privilege(bus, pc) {
                return 34;
            }
            let value = self.word(bus);
            let value = if full { value } else { value & 0xff };
            let result = match operation {
                0 => self.sr | value,
                2 => self.sr & if full { value } else { value | 0xff00 },
                _ => self.sr ^ value,
            };
            self.set_sr(result);
            return 20;
        }
        let Some(size) = Size::decode(op) else {
            return self.illegal(bus, pc);
        };
        if mode == 1 || (mode == 7 && r > 1) {
            return self.illegal(bus, pc);
        }
        let immediate = self.immediate(bus, size);
        let ea = self.ea(bus, mode, r, size);
        let old = self.read(bus, ea, size);
        let value = match operation {
            0 => {
                let x = old | immediate;
                self.logical(x, size);
                x
            }
            2 => {
                let x = old & immediate;
                self.logical(x, size);
                x
            }
            4 => self.sub(old, immediate, size, false, false),
            6 => self.add(old, immediate, size, false),
            10 => {
                let x = old ^ immediate;
                self.logical(x, size);
                x
            }
            _ => self.sub(old, immediate, size, false, true),
        };
        if operation != 12 {
            self.write(bus, ea, size, value);
        }
        (if size == Size::Long { 16 } else { 8 })
            + Self::ea_cycles(op, size)
            + if ea.memory() && operation != 12 { 4 } else { 0 }
    }
    fn group4(&mut self, bus: &mut impl Bus, op: u16, pc: u32) -> u32 {
        let mode = (op >> 3) & 7;
        let r = op & 7;
        let reg = ((op >> 9) & 7) as usize;
        match op {
            0x4e70 => {
                if !self.privilege(bus, pc) {
                    return 34;
                }
                bus.reset_devices();
                return 132;
            }
            0x4e71 => return 4,
            0x4e72 => {
                if !self.privilege(bus, pc) {
                    return 34;
                }
                let value = self.word(bus);
                self.set_sr(value);
                self.stopped = true;
                return 4;
            }
            0x4e73 => {
                if !self.privilege(bus, pc) {
                    return 34;
                }
                let value = self.pop16(bus);
                let target = self.pop32(bus);
                self.set_sr(value);
                self.pc = target;
                return 20;
            }
            0x4e75 => {
                self.pc = self.pop32(bus);
                return 16;
            }
            0x4e76 => {
                return if self.sr & V != 0 {
                    self.exception(bus, 7, self.pc)
                } else {
                    4
                };
            }
            0x4e77 => {
                let ccr = self.pop16(bus);
                self.pc = self.pop32(bus);
                self.sr = self.sr & 0xff00 | ccr & 0x1f;
                return 20;
            }
            0x4afc => return self.illegal(bus, pc),
            _ => {}
        }
        if op & 0xfff0 == 0x4e40 {
            return self.exception(bus, 32 + u32::from(op & 15), self.pc);
        }
        if op & 0xfff8 == 0x4e50 {
            let displacement = self.word(bus) as i16 as i32 as u32;
            let saved = if r == 7 { self.a[7].wrapping_sub(4) } else { self.a[r as usize] };
            self.push32(bus, saved);
            self.a[r as usize] = self.a[7];
            self.a[7] = self.a[7].wrapping_add(displacement);
            return 16;
        }
        if op & 0xfff8 == 0x4e58 {
            self.a[7] = self.a[r as usize];
            self.a[r as usize] = self.pop32(bus);
            return 12;
        }
        if op & 0xfff0 == 0x4e60 {
            if !self.privilege(bus, pc) {
                return 34;
            }
            if op & 8 != 0 {
                self.a[r as usize] = self.usp;
            } else {
                self.usp = self.a[r as usize];
            }
            return 4;
        }
        if op & 0xff80 == 0x4e80 {
            let ea = self.control_ea(bus, op);
            let Ea::Mem(target) = ea else {
                return self.illegal(bus, pc);
            };
            if op & 0x40 == 0 {
                self.push32(bus, self.pc);
            }
            self.pc = target;
            return (if op & 0x40 == 0 { 12 } else { 4 }) + Self::ea_cycles(op, Size::Long);
        }
        if op & 0xf1c0 == 0x41c0 {
            let Ea::Mem(addr) = self.control_ea(bus, op) else {
                return self.illegal(bus, pc);
            };
            self.a[reg] = addr;
            return Self::ea_cycles(op, Size::Word);
        }
        if op & 0xf1c0 == 0x4180 {
            if mode == 1 || (mode == 7 && r > 4) {
                return self.illegal(bus, pc);
            }
            let ea = self.ea(bus, mode, r, Size::Word);
            let bound = self.read(bus, ea, Size::Word) as i16;
            let value = self.d[reg] as i16;
            self.sr &= !(Z | V | C);
            if value < 0 {
                self.sr |= N;
                return self.exception(bus, 6, self.pc) + 6;
            }
            if value > bound {
                self.sr &= !N;
                return self.exception(bus, 6, self.pc) + 6;
            }
            return 10 + Self::ea_cycles(op, Size::Word);
        }
        if op & 0xfff8 == 0x4840 {
            self.d[r as usize] = self.d[r as usize].rotate_left(16);
            self.logical(self.d[r as usize], Size::Long);
            return 4;
        }
        if op & 0xffb8 == 0x4880 {
            if op & 0x40 != 0 {
                self.d[r as usize] = self.d[r as usize] as i16 as i32 as u32;
                self.logical(self.d[r as usize], Size::Long);
            } else {
                self.d[r as usize] =
                    self.d[r as usize] & 0xffff0000 | self.d[r as usize] as i8 as i16 as u16 as u32;
                self.logical(self.d[r as usize], Size::Word);
            }
            return 4;
        }
        if op & 0xfb80 == 0x4880 {
            return self.movem(bus, op, pc);
        }
        if op & 0xffc0 == 0x4840 {
            let Ea::Mem(addr) = self.control_ea(bus, op) else {
                return self.illegal(bus, pc);
            };
            self.push32(bus, addr);
            return 8 + Self::ea_cycles(op, Size::Long);
        }
        if op & 0xffc0 == 0x4800 {
            if mode == 1 || (mode == 7 && r > 1) {
                return self.illegal(bus, pc);
            }
            let ea = self.ea(bus, mode, r, Size::Byte);
            let old = self.read(bus, ea, Size::Byte);
            let value = self.bcd(0, old as u8, true);
            self.write(bus, ea, Size::Byte, value as u32);
            return 6 + Self::ea_cycles(op, Size::Byte);
        }
        if matches!(op & 0xffc0, 0x40c0 | 0x44c0 | 0x46c0) {
            let operation = op & 0xffc0;
            if mode == 1 || (mode == 7 && r > (if operation == 0x40c0 { 1 } else { 4 })) {
                return self.illegal(bus, pc);
            }
            if operation == 0x46c0 && !self.privilege(bus, pc) {
                return 34;
            }
            let ea = self.ea(bus, mode, r, Size::Word);
            if operation == 0x40c0 {
                self.write(bus, ea, Size::Word, self.sr as u32);
            } else {
                let value = self.read(bus, ea, Size::Word) as u16;
                if operation == 0x44c0 {
                    self.sr = self.sr & 0xff00 | value & 0x1f;
                } else {
                    self.set_sr(value);
                }
            }
            return (if operation == 0x40c0 { 6 } else { 12 }) + Self::ea_cycles(op, Size::Word);
        }
        if op & 0xffc0 == 0x4ac0 {
            if mode == 1 || (mode == 7 && r > 1) {
                return self.illegal(bus, pc);
            }
            let ea = self.ea(bus, mode, r, Size::Byte);
            let old = self.read(bus, ea, Size::Byte);
            self.logical(old, Size::Byte);
            self.write(bus, ea, Size::Byte, old | 0x80);
            return 4 + Self::ea_cycles(op, Size::Byte) + if ea.memory() { 10 } else { 0 };
        }
        let operation = op & 0xff00;
        if !matches!(operation, 0x4000 | 0x4200 | 0x4400 | 0x4600 | 0x4a00) {
            return self.illegal(bus, pc);
        }
        let Some(size) = Size::decode(op) else {
            return self.illegal(bus, pc);
        };
        if mode == 1 || (mode == 7 && r > 1) {
            return self.illegal(bus, pc);
        }
        let ea = self.ea(bus, mode, r, size);
        let old = self.read(bus, ea, size);
        let value = match operation {
            0x4000 => self.sub(0, old, size, true, false),
            0x4200 => {
                self.logical(0, size);
                0
            }
            0x4400 => self.sub(0, old, size, false, false),
            0x4600 => {
                self.logical(!old, size);
                !old
            }
            _ => {
                self.logical(old, size);
                old
            }
        };
        if operation != 0x4a00 {
            self.write(bus, ea, size, value);
        }
        (if size == Size::Long { 6 } else { 4 })
            + Self::ea_cycles(op, size)
            + if ea.memory() && operation != 0x4a00 {
                4
            } else {
                0
            }
    }
    fn movem(&mut self, bus: &mut impl Bus, op: u16, pc: u32) -> u32 {
        let load = op & 0x0400 != 0;
        let size = if op & 0x40 == 0 {
            Size::Word
        } else {
            Size::Long
        };
        let mode = (op >> 3) & 7;
        let r = (op & 7) as usize;
        if mode < 2
            || (mode == 3 && !load)
            || (mode == 4 && load)
            || (mode == 7 && r > if load { 3 } else { 1 })
        {
            return self.illegal(bus, pc);
        }
        let mask = self.word(bus);
        let mut addr = if mode == 3 || mode == 4 {
            self.a[r]
        } else {
            let Ea::Mem(addr) = self.ea(bus, mode, r as u16, size) else {
                return self.illegal(bus, pc);
            };
            addr
        };
        let original = self.a[r];
        for bit in 0..16 {
            if mask & (1 << bit) == 0 {
                continue;
            }
            let index = if mode == 4 { 15 - bit } else { bit };
            if mode == 4 {
                addr = addr.wrapping_sub(size.bytes());
            }
            if load {
                let value = self.read(bus, Ea::Mem(addr), size);
                let value = if size == Size::Word {
                    value as i16 as i32 as u32
                } else {
                    value
                };
                if index < 8 {
                    self.d[index] = value;
                } else {
                    self.a[index - 8] = value;
                }
            } else {
                let value = if index < 8 {
                    self.d[index]
                } else if index - 8 == r && mode == 4 {
                    original
                } else {
                    self.a[index - 8]
                };
                self.write(bus, Ea::Mem(addr), size, value);
            }
            if mode != 4 {
                addr = addr.wrapping_add(size.bytes());
            }
        }
        if mode == 3 || mode == 4 {
            self.a[r] = addr;
        }
        (if load { 12 } else { 8 }) + mask.count_ones() * if size == Size::Long { 8 } else { 4 }
    }
    /// ABCD, SBCD and NBCD (as 0 - src). The chip corrects the plain binary result: 6 for a carry out of the
    /// low digit, 0x60 for one out of the high digit, and the high digit is judged on the corrected value for
    /// an addition but on the plain difference for a subtraction. Operands that are not BCD digits come out the
    /// way SingleStepTests/680x0 has them, as do the flags the manual calls undefined (V is set where the
    /// correction flips the sign bit of the plain result).
    fn bcd(&mut self, dst: u8, src: u8, subtract: bool) -> u8 {
        let x = i16::from(self.sr & X != 0);
        let old_z = self.sr & Z;
        let (dst, src) = (i16::from(dst), i16::from(src));
        let (plain, value, carry) = if subtract {
            let plain = dst - src - x;
            let corrected = plain - if (dst & 15) - (src & 15) - x < 0 { 6 } else { 0 };
            let value = corrected - if plain < 0 { 0x60 } else { 0 };
            (plain as u8, value as u8, corrected < 0)
        } else {
            let plain = dst + src + x;
            let corrected = plain + if (dst & 15) + (src & 15) + x > 9 { 6 } else { 0 };
            let carry = corrected & 0x3f0 > 0x90;
            (plain as u8, (corrected + if carry { 0x60 } else { 0 }) as u8, carry)
        };
        self.sr &= !(C | X | Z | N | V);
        if carry {
            self.sr |= C | X;
        }
        if value == 0 {
            self.sr |= old_z;
        }
        if value & 0x80 != 0 {
            self.sr |= N;
        }
        let flipped = if subtract { plain & !value } else { !plain & value };
        if flipped & 0x80 != 0 {
            self.sr |= V;
        }
        value
    }
    fn alu(&mut self, bus: &mut impl Bus, op: u16, pc: u32) -> u32 {
        let group = op >> 12;
        let reg = ((op >> 9) & 7) as usize;
        let mode = (op >> 3) & 7;
        let r = (op & 7) as usize;
        if group == 12 {
            match op & 0xf1f8 {
                0xc140 => {
                    self.d.swap(reg, r);
                    return 6;
                }
                0xc148 => {
                    self.a.swap(reg, r);
                    return 6;
                }
                0xc188 => {
                    std::mem::swap(&mut self.d[reg], &mut self.a[r]);
                    return 6;
                }
                _ => {}
            }
        }
        if op & 0xf1f0 == 0xc100 || op & 0xf1f0 == 0x8100 {
            let memory = op & 8 != 0;
            let src_ea = if memory {
                self.ea(bus, 4, r as u16, Size::Byte)
            } else {
                Ea::D(r)
            };
            let src = self.read(bus, src_ea, Size::Byte) as u8;
            let dst_ea = if memory {
                self.ea(bus, 4, reg as u16, Size::Byte)
            } else {
                Ea::D(reg)
            };
            let dst = self.read(bus, dst_ea, Size::Byte) as u8;
            let value = self.bcd(dst, src, group == 8);
            self.write(bus, dst_ea, Size::Byte, value as u32);
            return if memory { 18 } else { 6 };
        }
        if (op & 0xf130 == 0xd100 || op & 0xf130 == 0x9100) && op & 0xc0 != 0xc0 {
            let Some(size) = Size::decode(op) else {
                return self.illegal(bus, pc);
            };
            let memory = op & 8 != 0;
            let src_ea = if memory {
                self.ea(bus, 4, r as u16, size)
            } else {
                Ea::D(r)
            };
            let src = self.read(bus, src_ea, size);
            let dst_ea = if memory {
                self.ea(bus, 4, reg as u16, size)
            } else {
                Ea::D(reg)
            };
            let dst = self.read(bus, dst_ea, size);
            let value = if group == 9 {
                self.sub(dst, src, size, true, false)
            } else {
                self.add(dst, src, size, true)
            };
            self.write(bus, dst_ea, size, value);
            return if memory {
                if size == Size::Long { 30 } else { 18 }
            } else if size == Size::Long {
                8
            } else {
                4
            };
        }
        if op & 0xf138 == 0xb108 && op & 0xc0 != 0xc0 {
            let Some(size) = Size::decode(op) else {
                return self.illegal(bus, pc);
            };
            let src_ea = self.ea(bus, 3, r as u16, size);
            let src = self.read(bus, src_ea, size);
            let dst_ea = self.ea(bus, 3, reg as u16, size);
            let dst = self.read(bus, dst_ea, size);
            self.sub(dst, src, size, false, true);
            return if size == Size::Long { 20 } else { 12 };
        }
        let opmode = (op >> 6) & 7;
        if opmode == 3 || opmode == 7 {
            if matches!(group, 9 | 11 | 13) {
                let size = if opmode == 3 { Size::Word } else { Size::Long };
                if mode == 7 && r > 4 {
                    return self.illegal(bus, pc);
                }
                let ea = self.ea(bus, mode, r as u16, size);
                let src = self.read(bus, ea, size);
                let src = if size == Size::Word {
                    src as i16 as i32 as u32
                } else {
                    src
                };
                match group {
                    9 => self.a[reg] = self.a[reg].wrapping_sub(src),
                    13 => self.a[reg] = self.a[reg].wrapping_add(src),
                    _ => {
                        self.sub(self.a[reg], src, Size::Long, false, true);
                    }
                }
                return (if group == 11 { 6 } else { 8 }) + Self::ea_cycles(op, size);
            }
            if mode == 1 || (mode == 7 && r > 4) {
                return self.illegal(bus, pc);
            }
            let ea = self.ea(bus, mode, r as u16, Size::Word);
            let src = self.read(bus, ea, Size::Word);
            if group == 12 {
                let value = if opmode == 7 {
                    (self.d[reg] as i16 as i32).wrapping_mul(src as i16 as i32) as u32
                } else {
                    (self.d[reg] & 0xffff) * src
                };
                self.d[reg] = value;
                self.logical(value, Size::Long);
                return 70 + Self::ea_cycles(op, Size::Word);
            }
            if src == 0 {
                // The flags are undefined in the manual; the chip leaves them clear (X is kept).
                self.sr &= !(N | Z | V | C);
                return self.exception(bus, 5, self.pc) + 4;
            }
            self.sr &= !(V | C);
            if opmode == 7 {
                let dividend = self.d[reg] as i32 as i64;
                let divisor = src as i16 as i64;
                let quotient = dividend / divisor;
                let remainder = dividend % divisor;
                if !(-32768..=32767).contains(&quotient) {
                    self.sr |= V;
                } else {
                    self.d[reg] = ((remainder as u16 as u32) << 16) | quotient as u16 as u32;
                    self.logical(quotient as u32, Size::Word);
                }
                158 + Self::ea_cycles(op, Size::Word)
            } else {
                let quotient = self.d[reg] / src;
                let remainder = self.d[reg] % src;
                if quotient > 0xffff {
                    self.sr |= V;
                } else {
                    self.d[reg] = (remainder << 16) | quotient;
                    self.logical(quotient, Size::Word);
                }
                140 + Self::ea_cycles(op, Size::Word)
            }
        } else {
            let Some(size) = Size::decode(op) else {
                return self.illegal(bus, pc);
            };
            let to_ea = opmode >= 4;
            if to_ea && mode == 0 && matches!(group, 8 | 12) {
                return self.illegal(bus, pc);
            }
            if (mode == 1 && (to_ea || size == Size::Byte || matches!(group, 8 | 12)))
                || (mode == 7 && r > if to_ea { 1 } else { 4 })
            {
                return self.illegal(bus, pc);
            }
            let ea = self.ea(bus, mode, r as u16, size);
            let ea_value = self.read(bus, ea, size);
            let data_value = self.d[reg] & size.mask();
            let (dst, src) = if to_ea {
                (ea_value, data_value)
            } else {
                (data_value, ea_value)
            };
            let value = match group {
                8 => {
                    let value = dst | src;
                    self.logical(value, size);
                    value
                }
                9 => self.sub(dst, src, size, false, false),
                11 if to_ea => {
                    let value = dst ^ src;
                    self.logical(value, size);
                    value
                }
                11 => self.sub(dst, src, size, false, true),
                12 => {
                    let value = dst & src;
                    self.logical(value, size);
                    value
                }
                _ => self.add(dst, src, size, false),
            };
            if group != 11 || to_ea {
                self.write(bus, if to_ea { ea } else { Ea::D(reg) }, size, value);
            }
            (if size == Size::Long { 6 } else { 4 })
                + Self::ea_cycles(op, size)
                + if to_ea && ea.memory() { 4 } else { 0 }
        }
    }
    fn shifts(&mut self, bus: &mut impl Bus, op: u16, pc: u32) -> u32 {
        let memory = op & 0x00c0 == 0x00c0;
        if memory && op & 0x0800 != 0 {
            return self.illegal(bus, pc);
        }
        let size = if memory {
            Size::Word
        } else {
            Size::decode(op).unwrap()
        };
        let kind = if memory { (op >> 9) & 3 } else { (op >> 3) & 3 };
        let left = op & 0x100 != 0;
        let reg = ((op >> 9) & 7) as usize;
        let count = if memory {
            1
        } else if op & 0x20 != 0 {
            self.d[reg] & 63
        } else if reg == 0 {
            8
        } else {
            reg as u32
        };
        let ea = if memory {
            let mode = (op >> 3) & 7;
            let r = op & 7;
            if mode < 2 || (mode == 7 && r > 1) {
                return self.illegal(bus, pc);
            }
            self.ea(bus, mode, r, size)
        } else {
            Ea::D((op & 7) as usize)
        };
        let mut value = self.read(bus, ea, size);
        let mut x = self.sr & X != 0;
        let mut carry = if kind == 2 { x } else { false };
        let mut overflow = false;
        for step in 0..count {
            let old_sign = value & size.sign() != 0;
            if left {
                carry = old_sign;
                value = (value << 1) & size.mask();
                if kind == 2 {
                    value |= u32::from(x);
                } else if kind == 3 {
                    value |= u32::from(carry);
                }
                if kind == 0 && old_sign != (value & size.sign() != 0) {
                    overflow = true;
                }
            } else {
                carry = value & 1 != 0;
                value >>= 1;
                if (kind == 0 && old_sign) || (kind == 2 && x) || (kind == 3 && carry) {
                    value |= size.sign();
                }
                // ASR past the operand's width shifts out copies of the sign, but the chip reports C = X = 0
                // (equal to the width it is still the sign).
                if kind == 0 && step >= size.bytes() * 8 {
                    carry = false;
                }
            }
            if kind != 3 {
                x = carry;
            }
        }
        self.logical(value, size);
        if carry {
            self.sr |= C;
        }
        if overflow {
            self.sr |= V;
        }
        if kind != 3 && count != 0 {
            self.sr = self.sr & !X | if x { X } else { 0 };
        }
        self.write(bus, ea, size, value);
        if memory {
            8 + Self::ea_cycles(op, size)
        } else {
            (if size == Size::Long { 8 } else { 6 }) + 2 * count
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Clone)]
    struct Ram(Vec<u8>);
    impl Bus for Ram {
        fn read8(&mut self, addr: u32) -> u8 {
            self.0[(addr as usize) & 0xffff]
        }
        fn write8(&mut self, addr: u32, value: u8) {
            self.0[(addr as usize) & 0xffff] = value;
        }
    }
    fn machine(words: &[u16]) -> (Cpu, Ram) {
        let mut ram = Ram(vec![0; 65536]);
        ram.write32(0, 0xff00);
        ram.write32(4, 0x100);
        for (i, &word) in words.iter().enumerate() {
            ram.write16(0x100 + i as u32 * 2, word);
        }
        let mut cpu = Cpu::new();
        cpu.reset(&mut ram);
        (cpu, ram)
    }
    #[test]
    fn arithmetic_width_and_flags() {
        let (mut cpu, mut ram) = machine(&[0x707f, 0x5200, 0x0400, 0x0001]); // moveq 127,d0; addq.b #1,d0; subi.b #1,d0
        cpu.step(&mut ram);
        cpu.step(&mut ram);
        assert_eq!(cpu.d[0], 0x80);
        assert_eq!(cpu.sr & 31, N | V);
        cpu.step(&mut ram);
        assert_eq!(cpu.d[0], 0x7f);
        assert_eq!(cpu.sr & 31, V);
        assert_eq!(cpu.add(0xff, 1, Size::Byte, false), 0);
        assert_eq!(cpu.sr & 31, X | Z | C);
        assert_eq!(cpu.sub(0, 1, Size::Byte, false, false), 0xff);
        assert_eq!(cpu.sr & 31, X | N | C);
        cpu.sub(1, 1, Size::Word, false, true);
        assert_eq!(cpu.sr & 31, X | Z);
    }
    #[test]
    fn move_postincrement_and_stack_byte_alignment() {
        let (mut cpu, mut ram) = machine(&[0x20fc, 0x1234, 0x5678, 0x1f00]); // move.l #$12345678,(a0)+; move.b d0,-(a7)
        cpu.a[0] = 0x200;
        cpu.d[0] = 0xab;
        cpu.step(&mut ram);
        assert_eq!(ram.read32(0x200), 0x12345678);
        assert_eq!(cpu.a[0], 0x204);
        cpu.step(&mut ram);
        assert_eq!(cpu.a[7], 0xfefe);
        assert_eq!(ram.read8(0xfefe), 0xab);
    }
    #[test]
    fn dbra_and_subroutine() {
        let (mut cpu, mut ram) = machine(&[
            0x7002, 0x7200, 0x5281, 0x51c8, 0xfffc, 0x6104, 0x4e71, 0x4e71, 0x5481, 0x4e75,
        ]);
        for _ in 0..8 {
            cpu.step(&mut ram);
        }
        assert_eq!(cpu.d[1], 3);
        assert_eq!(cpu.d[0], 0xffff);
        cpu.step(&mut ram);
        assert_eq!(cpu.pc, 0x110);
        cpu.step(&mut ram);
        cpu.step(&mut ram);
        assert_eq!(cpu.pc, 0x10c);
        assert_eq!(cpu.d[1], 5);
    }
    #[test]
    fn interrupt_switches_stack_and_rte_restores_user_mode() {
        let (mut cpu, mut ram) = machine(&[]);
        ram.write32(30 * 4, 0x300);
        ram.write16(0x300, 0x4e73);
        cpu.usp = 0xe000;
        cpu.set_sr(0);
        assert_eq!(cpu.a[7], 0xe000);
        assert_eq!(cpu.interrupt(&mut ram, 6), 44);
        assert_eq!(cpu.pc, 0x300);
        assert_eq!(ram.read16(0xfefa), 0);
        assert_eq!(ram.read32(0xfefc), 0x100);
        cpu.step(&mut ram);
        assert_eq!(cpu.pc, 0x100);
        assert_eq!(cpu.a[7], 0xe000);
        assert_eq!(cpu.sr, 0);
    }
    #[test]
    fn illegal_and_privilege_violations_use_faulting_pc() {
        let (mut cpu, mut ram) = machine(&[0x4afc]);
        ram.write32(4 * 4, 0x400);
        cpu.step(&mut ram);
        assert_eq!(cpu.pc, 0x400);
        assert_eq!(ram.read32(cpu.a[7] + 2), 0x100);
        let (mut cpu, mut ram) = machine(&[0x4e72, 0x2700]);
        ram.write32(8 * 4, 0x500);
        cpu.usp = 0xd000;
        cpu.set_sr(0);
        cpu.step(&mut ram);
        assert_eq!(cpu.pc, 0x500);
        assert!(!cpu.stopped);
        assert_eq!(ram.read32(cpu.a[7] + 2), 0x100);
    }
    #[test]
    fn movem_predecrement_and_postincrement_word_sign_extension() {
        let (mut cpu, mut ram) = machine(&[0x48e7, 0xc080, 0x4cdf, 0x0103, 0x4c98, 0x0001]);
        cpu.d[0] = 0x12345678;
        cpu.d[1] = 0xabcdef00;
        cpu.a[0] = 0x201;
        cpu.step(&mut ram);
        assert_eq!(cpu.a[7], 0xfef4);
        assert_eq!(ram.read32(0xfef4), 0x12345678);
        assert_eq!(ram.read32(0xfef8), 0xabcdef00);
        assert_eq!(ram.read32(0xfefc), 0x201);
        cpu.d[0] = 0;
        cpu.d[1] = 0;
        cpu.a[0] = 0;
        cpu.step(&mut ram);
        assert_eq!(cpu.d[0], 0x12345678);
        assert_eq!(cpu.d[1], 0xabcdef00);
        assert_eq!(cpu.a[0], 0x201);
        assert_eq!(cpu.a[7], 0xff00);
        cpu.a[0] = 0x400;
        ram.write16(0x400, 0x8001);
        cpu.step(&mut ram);
        assert_eq!(cpu.d[0], 0xffff8001);
        assert_eq!(cpu.a[0], 0x402);
    }
    #[test]
    fn pc_relative_base_is_extension_word_address() {
        let (mut cpu, mut ram) = machine(&[0x41fa, 0x0010, 0x303a, 0x000c]);
        ram.write16(0x112, 0xbeef);
        cpu.step(&mut ram);
        assert_eq!(cpu.a[0], 0x112);
        cpu.step(&mut ram);
        assert_eq!(cpu.d[0], 0xbeef);
    }
    #[test]
    fn division_overflow_and_signed_remainder() {
        let (mut cpu, mut ram) = machine(&[0x81fc, 0x0003, 0x80fc, 0x0001]);
        cpu.d[0] = (-10i32) as u32;
        cpu.step(&mut ram);
        assert_eq!(cpu.d[0], 0xfffffffd);
        cpu.d[0] = 0x10000;
        cpu.step(&mut ram);
        assert_eq!(cpu.d[0], 0x10000);
        assert_ne!(cpu.sr & V, 0);
    }
    #[test]
    fn shift_zero_count_preserves_extend_and_rox_copies_it_to_carry() {
        let (mut cpu, mut ram) = machine(&[0xe228, 0xe230]); // lsr.b d1,d0; roxr.b d1,d0
        cpu.d[1] = 0;
        cpu.d[0] = 0x80;
        cpu.sr |= X;
        cpu.step(&mut ram);
        assert_eq!(cpu.sr & 31, X | N);
        cpu.step(&mut ram);
        assert_eq!(cpu.sr & 31, X | N | C);
    }
    #[test]
    fn bcd_and_extend_use_cumulative_zero() {
        let mut cpu = Cpu::new();
        cpu.sr |= Z;
        assert_eq!(cpu.bcd(0x99, 1, false), 0);
        assert_eq!(cpu.sr & (X | C | Z), X | C | Z);
        assert_eq!(cpu.bcd(0, 0, true), 0x99);
        assert_eq!(cpu.sr & (X | C | Z), X | C);
        cpu.sr = X | Z;
        assert_eq!(cpu.add(0xff, 0, Size::Byte, true), 0);
        assert_ne!(cpu.sr & Z, 0);
        cpu.sr = X;
        assert_eq!(cpu.add(0xff, 0, Size::Byte, true), 0);
        assert_eq!(cpu.sr & Z, 0);
    }
    #[test]
    fn move_destination_observes_source_postincrement() {
        let (mut cpu, mut ram) = machine(&[0x20d8, 0x3058]); // move.l (a0)+,(a0)+; movea.w (a0)+,a0
        cpu.a[0] = 0x200;
        ram.write32(0x200, 0x12345678);
        ram.write16(0x208, 0x8001);
        cpu.step(&mut ram);
        assert_eq!(ram.read32(0x204), 0x12345678);
        assert_eq!(cpu.a[0], 0x208);
        cpu.step(&mut ram);
        assert_eq!(cpu.a[0], 0xffff8001);
    }
    #[test]
    fn addx_and_cmpm_same_stack_register_update_twice() {
        let (mut cpu, mut ram) = machine(&[0xdf0f, 0xbf0f]); // addx.b -(a7),-(a7); cmpm.b (a7)+,(a7)+
        cpu.a[7] = 0x300;
        ram.write8(0x2fe, 1);
        ram.write8(0x2fc, 0xff);
        cpu.sr = S | Z;
        cpu.step(&mut ram);
        assert_eq!(cpu.a[7], 0x2fc);
        assert_eq!(ram.read8(0x2fc), 0);
        assert_eq!(cpu.sr & 31, X | Z | C);
        cpu.step(&mut ram);
        assert_eq!(cpu.a[7], 0x300);
        assert_eq!(cpu.sr & 31, X);
    }
    #[test]
    fn movem_stores_original_predecrement_base_and_ignores_loaded_postincrement_base() {
        let (mut cpu, mut ram) = machine(&[0x48e7, 0x0001, 0x4cdf, 0x8000]);
        cpu.step(&mut ram);
        assert_eq!(cpu.a[7], 0xfefc);
        assert_eq!(ram.read32(0xfefc), 0xff00);
        ram.write32(0xfefc, 0x12345678);
        cpu.step(&mut ram);
        assert_eq!(cpu.a[7], 0xff00);
    }
    #[test]
    fn dynamic_btst_immediate_is_valid_static_immediate_is_illegal() {
        let (mut cpu, mut ram) = machine(&[0x013c, 0x0002, 0x083c, 0x0001, 0x0002]);
        ram.write32(4 * 4, 0x400);
        cpu.d[0] = 9;
        cpu.sr |= Z | C;
        cpu.step(&mut ram);
        assert_eq!(cpu.pc, 0x104);
        assert_eq!(cpu.sr & (Z | C), C);
        cpu.step(&mut ram);
        assert_eq!(cpu.pc, 0x400);
    }
    #[test]
    fn odd_word_read_stacks_68000_diagnostic_frame_and_aborts_later_writes() {
        let (mut cpu, mut ram) = machine(&[0x2290]); // move.l (a0),(a1)
        ram.write32(3 * 4, 0x300);
        cpu.a[0] = 0x201;
        cpu.a[1] = 0x400;
        ram.write32(0x400, 0xdeadbeef);
        assert_eq!(cpu.step(&mut ram), 50);
        assert_eq!(cpu.pc, 0x300);
        assert_eq!(ram.read32(0x400), 0xdeadbeef);
        assert_eq!(cpu.a[7], 0xfef2);
        assert_eq!(ram.read16(0xfef2), 0x1d); // supervisor data read
        assert_eq!(ram.read32(0xfef4), 0x201);
        assert_eq!(ram.read16(0xfef8), 0x2290);
        assert_eq!(ram.read16(0xfefa), 0x2700);
        assert_eq!(ram.read32(0xfefc), 0x100);
    }
    #[test]
    fn odd_word_write_and_odd_instruction_fetch_distinguish_status_bits() {
        let (mut cpu, mut ram) = machine(&[0x3080]); // move.w d0,(a0)
        ram.write32(3 * 4, 0x300);
        cpu.a[0] = 0x201;
        cpu.d[0] = 0xbeef;
        cpu.step(&mut ram);
        assert_eq!(ram.read16(cpu.a[7]), 0x0d); // supervisor data write
        assert_eq!(ram.read16(0x200), 0);
        let (mut cpu, mut ram) = machine(&[]);
        ram.write32(3 * 4, 0x301);
        cpu.pc = 0x101;
        cpu.step(&mut ram);
        assert_eq!(ram.read16(cpu.a[7]), 0x16); // supervisor instruction read
        assert!(cpu.halted); // second odd fetch while taking the address exception
        assert_eq!(cpu.interrupt(&mut ram, 7), 0);
    }
    #[test]
    fn trace_does_not_replace_an_instruction_exception() {
        let (mut cpu, mut ram) = machine(&[0x4afc]);
        ram.write32(4 * 4, 0x400);
        ram.write32(9 * 4, 0x500);
        cpu.sr |= 0x8000;
        cpu.step(&mut ram);
        assert_eq!(cpu.pc, 0x400);
        assert_eq!(cpu.a[7], 0xfefa);
        let (mut cpu, mut ram) = machine(&[0x4e71]);
        ram.write32(9 * 4, 0x500);
        cpu.sr |= 0x8000;
        cpu.step(&mut ram);
        assert_eq!(cpu.pc, 0x500);
        assert_eq!(ram.read32(cpu.a[7] + 2), 0x102);
    }
    #[test]
    fn every_opcode_finishes_an_instruction_without_panicking() {
        let mut ram = Ram(vec![0; 65536]);
        for opcode in 0..=u16::MAX {
            let mut cpu = Cpu::new();
            cpu.pc = 0x100;
            cpu.a = [0x400; 8];
            cpu.a[7] = 0xff00;
            ram.write16(0x100, opcode);
            ram.write32(0x102, 0);
            ram.write32(0x106, 0);
            assert!(cpu.step(&mut ram) > 0, "opcode {opcode:04x}");
        }
    }
    #[test]
    fn address_long_ops_do_not_alias_extend_or_cmpm_decoding() {
        let (mut cpu, mut ram) = machine(&[0xb5c9, 0xd5c9, 0x95c9]); // cmpa.l a1,a2; adda.l a1,a2; suba.l a1,a2
        cpu.a[1] = 0x200;
        cpu.a[2] = 0x300;
        cpu.sr |= X;
        cpu.step(&mut ram);
        assert_eq!(cpu.pc, 0x102);
        assert_eq!(cpu.sr & 31, X);
        assert_eq!(cpu.a[2], 0x300);
        cpu.step(&mut ram);
        assert_eq!(cpu.pc, 0x104);
        assert_eq!(cpu.a[2], 0x500);
        assert_eq!(cpu.sr & 31, X);
        cpu.step(&mut ram);
        assert_eq!(cpu.pc, 0x106);
        assert_eq!(cpu.a[2], 0x300);
        assert_eq!(cpu.sr & 31, X);
    }
}
