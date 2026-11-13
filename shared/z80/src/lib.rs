//! Zilog Z80. Instruction decoding follows Zilog UM0080's x/y/z groups.
//! Bus operations occur at instruction boundaries; undocumented flag bits and
//! bus contention are not intended as transistor-level emulation.
//!
//! The Mega Drive uses it as the sound CPU, the Master System and Game Gear as their main CPU. The machine
//! gives it a `Bus` and calls `step`, which runs one instruction (or takes an interrupt) and returns its
//! T-states.

use savestate::state;

#[cfg(test)]
mod singlestep;
#[cfg(test)]
mod sst;

const S: u8 = 0x80;
const Z: u8 = 0x40;
const H: u8 = 0x10;
const P: u8 = 0x04;
const N: u8 = 0x02;
const C: u8 = 0x01;

pub trait Bus {
    fn read(&mut self, addr: u16) -> u8;
    fn write(&mut self, addr: u16, value: u8);
    fn input(&mut self, _port: u16) -> u8 {
        0xff
    }
    fn output(&mut self, _port: u16, _value: u8) {}
}

#[derive(Clone)]
pub struct Z80 {
    pub a: u8,
    pub f: u8,
    b: u8,
    c: u8,
    d: u8,
    e: u8,
    h: u8,
    l: u8,
    alternate: [u8; 8],
    ix: u16,
    iy: u16,
    pub sp: u16,
    pub pc: u16,
    i: u8,
    r: u8,
    iff1: bool,
    iff2: bool,
    im: u8,
    halted: bool,
    ei_delay: u8,
    /// The INT pin, level-triggered: while it is high and interrupts are enabled the next `step` takes the
    /// interrupt, so the machine lowers it when the device is acknowledged.
    pub irq: bool,
    /// The NMI pin's latched edge. The machine sets it and the next `step` takes the interrupt and clears it.
    /// It is not part of the saved state: a machine raises it right before it runs the CPU, never between
    /// two frames, where states are taken.
    pub nmi: bool,
}

impl Default for Z80 {
    fn default() -> Self {
        Self::new()
    }
}

state!(Z80 {
    a,
    f,
    b,
    c,
    d,
    e,
    h,
    l,
    alternate,
    ix,
    iy,
    sp,
    pc,
    i,
    r,
    iff1,
    iff2,
    im,
    halted,
    ei_delay,
    irq
});

impl Z80 {
    pub fn new() -> Self {
        Self {
            a: 0,
            f: 0,
            b: 0,
            c: 0,
            d: 0,
            e: 0,
            h: 0,
            l: 0,
            alternate: [0; 8],
            ix: 0,
            iy: 0,
            sp: 0xffff,
            pc: 0,
            i: 0,
            r: 0,
            iff1: false,
            iff2: false,
            im: 0,
            halted: false,
            ei_delay: 0,
            irq: false,
            nmi: false,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
    pub fn validate(&self) -> bool {
        self.im <= 2 && self.ei_delay <= 2
    }

    fn byte(&mut self, bus: &mut impl Bus) -> u8 {
        let n = bus.read(self.pc);
        self.pc = self.pc.wrapping_add(1);
        n
    }
    fn opcode(&mut self, bus: &mut impl Bus) -> u8 {
        self.r = (self.r & 0x80) | (self.r.wrapping_add(1) & 0x7f);
        self.byte(bus)
    }
    fn word(&mut self, bus: &mut impl Bus) -> u16 {
        let lo = self.byte(bus);
        u16::from_le_bytes([lo, self.byte(bus)])
    }
    fn read16(bus: &mut impl Bus, addr: u16) -> u16 {
        let lo = bus.read(addr);
        u16::from_le_bytes([lo, bus.read(addr.wrapping_add(1))])
    }
    fn write16(bus: &mut impl Bus, addr: u16, value: u16) {
        bus.write(addr, value as u8);
        bus.write(addr.wrapping_add(1), (value >> 8) as u8);
    }
    fn push(&mut self, bus: &mut impl Bus, value: u16) {
        self.sp = self.sp.wrapping_sub(1);
        bus.write(self.sp, (value >> 8) as u8);
        self.sp = self.sp.wrapping_sub(1);
        bus.write(self.sp, value as u8);
    }
    fn pop(&mut self, bus: &mut impl Bus) -> u16 {
        let n = Self::read16(bus, self.sp);
        self.sp = self.sp.wrapping_add(2);
        n
    }
    fn hl(&self, index: u8) -> u16 {
        match index {
            1 => self.ix,
            2 => self.iy,
            _ => u16::from_be_bytes([self.h, self.l]),
        }
    }
    fn set_hl(&mut self, index: u8, n: u16) {
        match index {
            1 => self.ix = n,
            2 => self.iy = n,
            _ => {
                [self.h, self.l] = n.to_be_bytes();
            }
        }
    }
    fn pair(&self, p: u8, index: u8) -> u16 {
        match p {
            0 => u16::from_be_bytes([self.b, self.c]),
            1 => u16::from_be_bytes([self.d, self.e]),
            2 => self.hl(index),
            _ => self.sp,
        }
    }
    fn set_pair(&mut self, p: u8, index: u8, n: u16) {
        match p {
            0 => [self.b, self.c] = n.to_be_bytes(),
            1 => [self.d, self.e] = n.to_be_bytes(),
            2 => self.set_hl(index, n),
            _ => self.sp = n,
        }
    }
    fn reg(&mut self, bus: &mut impl Bus, r: u8, index: u8, addr: u16) -> u8 {
        match r {
            0 => self.b,
            1 => self.c,
            2 => self.d,
            3 => self.e,
            4 => (self.hl(index) >> 8) as u8,
            5 => self.hl(index) as u8,
            6 => bus.read(addr),
            _ => self.a,
        }
    }
    fn set_reg(&mut self, bus: &mut impl Bus, r: u8, index: u8, addr: u16, n: u8) {
        match r {
            0 => self.b = n,
            1 => self.c = n,
            2 => self.d = n,
            3 => self.e = n,
            4 => self.set_hl(index, (self.hl(index) & 255) | (u16::from(n) << 8)),
            5 => self.set_hl(index, (self.hl(index) & 0xff00) | u16::from(n)),
            6 => bus.write(addr, n),
            _ => self.a = n,
        }
    }
    fn condition(&self, n: u8) -> bool {
        match n {
            0 => self.f & Z == 0,
            1 => self.f & Z != 0,
            2 => self.f & C == 0,
            3 => self.f & C != 0,
            4 => self.f & P == 0,
            5 => self.f & P != 0,
            6 => self.f & S == 0,
            _ => self.f & S != 0,
        }
    }
    fn sz(n: u8) -> u8 {
        (n & (S | 0x28)) | if n == 0 { Z } else { 0 }
    }
    fn szp(n: u8) -> u8 {
        Self::sz(n)
            | if n.count_ones().is_multiple_of(2) {
                P
            } else {
                0
            }
    }
    fn add(&mut self, n: u8, carry: bool) {
        let a = self.a;
        let c = u16::from(carry && self.f & C != 0);
        let r = u16::from(a) + u16::from(n) + c;
        self.a = r as u8;
        self.f = Self::sz(self.a)
            | if (a ^ n ^ self.a) & 16 != 0 { H } else { 0 }
            | if (!(a ^ n) & (a ^ self.a)) & 128 != 0 {
                P
            } else {
                0
            }
            | if r > 255 { C } else { 0 };
    }
    fn sub(&mut self, n: u8, carry: bool, compare: bool) {
        let a = self.a;
        let c = u16::from(carry && self.f & C != 0);
        let rhs = u16::from(n) + c;
        let r = a.wrapping_sub(n).wrapping_sub(c as u8);
        self.f = Self::sz(r)
            | N
            | if (a ^ n ^ r) & 16 != 0 { H } else { 0 }
            | if ((a ^ n) & (a ^ r)) & 128 != 0 { P } else { 0 }
            | if u16::from(a) < rhs { C } else { 0 };
        if compare {
            self.f = (self.f & !0x28) | (n & 0x28)
        } else {
            self.a = r;
        }
    }
    fn alu(&mut self, y: u8, n: u8) {
        match y {
            0 => self.add(n, false),
            1 => self.add(n, true),
            2 => self.sub(n, false, false),
            3 => self.sub(n, true, false),
            4 => {
                self.a &= n;
                self.f = Self::szp(self.a) | H
            }
            5 => {
                self.a ^= n;
                self.f = Self::szp(self.a)
            }
            6 => {
                self.a |= n;
                self.f = Self::szp(self.a)
            }
            _ => self.sub(n, false, true),
        }
    }

    pub fn step(&mut self, bus: &mut impl Bus) -> u32 {
        if self.nmi {
            // Taken at once, EI's delay or not; it leaves HALT, keeps IFF1 in IFF2 for RETN and enters at 0x66.
            self.nmi = false;
            self.halted = false;
            self.iff2 = self.iff1;
            self.iff1 = false;
            self.r = (self.r & 128) | (self.r.wrapping_add(1) & 127);
            self.push(bus, self.pc);
            self.pc = 0x66;
            return 11;
        }
        if self.irq && self.iff1 && self.ei_delay == 0 {
            self.halted = false;
            self.iff1 = false;
            self.iff2 = false;
            self.r = (self.r & 128) | (self.r.wrapping_add(1) & 127);
            self.push(bus, self.pc);
            self.pc = if self.im == 2 {
                Self::read16(bus, (u16::from(self.i) << 8) | 255)
            } else {
                0x38
            };
            return if self.im == 2 { 19 } else { 13 };
        }
        if self.halted {
            self.r = (self.r & 128) | (self.r.wrapping_add(1) & 127);
            return 4;
        }
        let mut index = 0;
        let mut prefix_cycles = 0;
        let op = loop {
            let n = self.opcode(bus);
            if n == 0xdd || n == 0xfd {
                index = if n == 0xdd { 1 } else { 2 };
                prefix_cycles += 4;
                // A stream of prefixes must still return control to the scheduler.
                if prefix_cycles >= 64 {
                    return prefix_cycles;
                }
            } else {
                break n;
            }
        };
        let cycles = if op == 0xcb {
            let addr = if index != 0 {
                let d = self.byte(bus) as i8;
                self.hl(index).wrapping_add_signed(i16::from(d))
            } else {
                self.hl(0)
            };
            let cb = if index != 0 {
                self.byte(bus)
            } else {
                self.opcode(bus)
            };
            self.cb(bus, cb, index, addr)
        } else if op == 0xed {
            let ed = self.opcode(bus);
            self.ed(bus, ed)
        } else {
            self.base(bus, op, index)
        };
        self.ei_delay = self.ei_delay.saturating_sub(1);
        cycles + prefix_cycles
    }

    fn base(&mut self, bus: &mut impl Bus, op: u8, index: u8) -> u32 {
        let x = op >> 6;
        let y = (op >> 3) & 7;
        let z = op & 7;
        let p = y >> 1;
        let q = y & 1;
        let memory = index != 0
            && ((x == 0 && matches!(z, 4..=6) && y == 6)
                || (x == 1 && op != 0x76 && (y == 6 || z == 6))
                || (x == 2 && z == 6));
        let addr = if memory {
            let d = self.byte(bus) as i8;
            self.hl(index).wrapping_add_signed(i16::from(d))
        } else {
            self.hl(index)
        };
        let mut cycles = match x {
            0 => match z {
                0 => match y {
                    0 => 4,
                    1 => {
                        std::mem::swap(&mut self.a, &mut self.alternate[0]);
                        std::mem::swap(&mut self.f, &mut self.alternate[1]);
                        4
                    }
                    2 => {
                        let d = self.byte(bus) as i8;
                        self.b = self.b.wrapping_sub(1);
                        if self.b != 0 {
                            self.pc = self.pc.wrapping_add_signed(i16::from(d));
                            13
                        } else {
                            8
                        }
                    }
                    3 => {
                        let d = self.byte(bus) as i8;
                        self.pc = self.pc.wrapping_add_signed(i16::from(d));
                        12
                    }
                    _ => {
                        let d = self.byte(bus) as i8;
                        if self.condition(y - 4) {
                            self.pc = self.pc.wrapping_add_signed(i16::from(d));
                            12
                        } else {
                            7
                        }
                    }
                },
                1 => {
                    if q == 0 {
                        let n = self.word(bus);
                        self.set_pair(p, index, n);
                        10
                    } else {
                        let a = self.hl(index);
                        let b = self.pair(p, index);
                        let n = u32::from(a) + u32::from(b);
                        self.set_hl(index, n as u16);
                        self.f = (self.f & (S | Z | P))
                            | ((n >> 8) as u8 & 0x28)
                            | if (a ^ b ^ n as u16) & 0x1000 != 0 {
                                H
                            } else {
                                0
                            }
                            | if n > 65535 { C } else { 0 };
                        11
                    }
                }
                2 => {
                    if q == 0 {
                        match p {
                            0 | 1 => bus.write(self.pair(p, 0), self.a),
                            2 => {
                                let a = self.word(bus);
                                Self::write16(bus, a, self.hl(index));
                            }
                            _ => {
                                let a = self.word(bus);
                                bus.write(a, self.a);
                            }
                        }
                        match p {
                            0 | 1 => 7,
                            2 => 16,
                            _ => 13,
                        }
                    } else {
                        match p {
                            0 | 1 => self.a = bus.read(self.pair(p, 0)),
                            2 => {
                                let a = self.word(bus);
                                let n = Self::read16(bus, a);
                                self.set_hl(index, n);
                            }
                            _ => {
                                let a = self.word(bus);
                                self.a = bus.read(a);
                            }
                        }
                        match p {
                            0 | 1 => 7,
                            2 => 16,
                            _ => 13,
                        }
                    }
                }
                3 => {
                    let n = self
                        .pair(p, index)
                        .wrapping_add(if q == 0 { 1 } else { u16::MAX });
                    self.set_pair(p, index, n);
                    6
                }
                4 | 5 => {
                    let a = self.reg(bus, y, index, addr);
                    let n = if z == 4 {
                        a.wrapping_add(1)
                    } else {
                        a.wrapping_sub(1)
                    };
                    self.f = (self.f & C)
                        | Self::sz(n)
                        | if (a ^ n) & 16 != 0 { H } else { 0 }
                        | if (z == 4 && a == 0x7f) || (z == 5 && a == 0x80) {
                            P
                        } else {
                            0
                        }
                        | if z == 5 { N } else { 0 };
                    self.set_reg(bus, y, index, addr, n);
                    if y == 6 { 11 } else { 4 }
                }
                6 => {
                    let n = self.byte(bus);
                    self.set_reg(bus, y, index, addr, n);
                    if y == 6 { 10 } else { 7 }
                }
                _ => {
                    match y {
                        0..=3 => {
                            let a = self.a;
                            let old = self.f & C;
                            let c = if y & 1 == 0 { a >> 7 } else { a & 1 };
                            self.a = match y {
                                0 => a.rotate_left(1),
                                1 => a.rotate_right(1),
                                2 => (a << 1) | old,
                                _ => (a >> 1) | (old << 7),
                            };
                            self.f = (self.f & (S | Z | P)) | (self.a & 0x28) | c;
                        }
                        4 => {
                            let a = self.a;
                            let mut correction = 0;
                            let mut carry = self.f & C;
                            if self.f & H != 0 || a & 15 > 9 {
                                correction |= 6;
                            }
                            if carry != 0 || a > 0x99 {
                                correction |= 0x60;
                                carry = C;
                            }
                            self.a = if self.f & N != 0 {
                                a.wrapping_sub(correction)
                            } else {
                                a.wrapping_add(correction)
                            };
                            self.f = (self.f & N)
                                | carry
                                | Self::szp(self.a)
                                | if (a ^ self.a) & 16 != 0 { H } else { 0 };
                        }
                        5 => {
                            self.a = !self.a;
                            self.f = (self.f & (S | Z | P | C)) | (self.a & 0x28) | H | N;
                        }
                        6 => self.f = (self.f & (S | Z | P)) | (self.a & 0x28) | C,
                        _ => {
                            self.f = (self.f & (S | Z | P))
                                | (self.a & 0x28)
                                | if self.f & C != 0 { H } else { C }
                        }
                    }
                    4
                }
            },
            1 => {
                if op == 0x76 {
                    self.halted = true;
                    4
                } else {
                    // Indexed memory forms LD H,(IX+d) and LD (IX+d),H use H, not IXH.
                    let ri = if memory { 0 } else { index };
                    let n = self.reg(bus, z, ri, addr);
                    self.set_reg(bus, y, ri, addr, n);
                    if y == 6 || z == 6 { 7 } else { 4 }
                }
            }
            2 => {
                let n = self.reg(bus, z, index, addr);
                self.alu(y, n);
                if z == 6 { 7 } else { 4 }
            }
            _ => match z {
                0 => {
                    if self.condition(y) {
                        self.pc = self.pop(bus);
                        11
                    } else {
                        5
                    }
                }
                1 => {
                    if q == 0 {
                        let n = self.pop(bus);
                        if p == 3 {
                            [self.a, self.f] = n.to_be_bytes();
                        } else {
                            self.set_pair(p, index, n);
                        }
                        10
                    } else {
                        match p {
                            0 => {
                                self.pc = self.pop(bus);
                                10
                            }
                            1 => {
                                std::mem::swap(&mut self.b, &mut self.alternate[2]);
                                std::mem::swap(&mut self.c, &mut self.alternate[3]);
                                std::mem::swap(&mut self.d, &mut self.alternate[4]);
                                std::mem::swap(&mut self.e, &mut self.alternate[5]);
                                std::mem::swap(&mut self.h, &mut self.alternate[6]);
                                std::mem::swap(&mut self.l, &mut self.alternate[7]);
                                4
                            }
                            2 => {
                                self.pc = self.hl(index);
                                4
                            }
                            _ => {
                                self.sp = self.hl(index);
                                6
                            }
                        }
                    }
                }
                2 => {
                    let a = self.word(bus);
                    if self.condition(y) {
                        self.pc = a;
                    }
                    10
                }
                3 => match y {
                    0 => {
                        self.pc = self.word(bus);
                        10
                    }
                    2 => {
                        let n = self.byte(bus);
                        bus.output((u16::from(self.a) << 8) | u16::from(n), self.a);
                        11
                    }
                    3 => {
                        let n = self.byte(bus);
                        self.a = bus.input((u16::from(self.a) << 8) | u16::from(n));
                        11
                    }
                    4 => {
                        let n = Self::read16(bus, self.sp);
                        Self::write16(bus, self.sp, self.hl(index));
                        self.set_hl(index, n);
                        19
                    }
                    5 => {
                        let n = self.pair(1, 0);
                        self.set_pair(1, 0, self.hl(0));
                        self.set_hl(0, n);
                        4
                    }
                    6 => {
                        self.iff1 = false;
                        self.iff2 = false;
                        4
                    }
                    7 => {
                        self.iff1 = true;
                        self.iff2 = true;
                        self.ei_delay = 2;
                        4
                    }
                    _ => 4,
                },
                4 => {
                    let a = self.word(bus);
                    if self.condition(y) {
                        self.push(bus, self.pc);
                        self.pc = a;
                        17
                    } else {
                        10
                    }
                }
                5 => {
                    if q == 0 {
                        let n = if p == 3 {
                            u16::from_be_bytes([self.a, self.f])
                        } else {
                            self.pair(p, index)
                        };
                        self.push(bus, n);
                        11
                    } else if p == 0 {
                        let a = self.word(bus);
                        self.push(bus, self.pc);
                        self.pc = a;
                        17
                    } else {
                        4
                    }
                }
                6 => {
                    let n = self.byte(bus);
                    self.alu(y, n);
                    7
                }
                _ => {
                    self.push(bus, self.pc);
                    self.pc = u16::from(y) * 8;
                    11
                }
            },
        };
        if memory {
            cycles += if x == 0 && z == 6 { 5 } else { 8 };
        }
        cycles
    }

    fn cb(&mut self, bus: &mut impl Bus, op: u8, index: u8, addr: u16) -> u32 {
        let x = op >> 6;
        let y = (op >> 3) & 7;
        let z = op & 7;
        let a = if index != 0 {
            bus.read(addr)
        } else {
            self.reg(bus, z, 0, addr)
        };
        let n = match x {
            0 => {
                let old = self.f & C;
                let c = if matches!(y, 0 | 2 | 4 | 6) {
                    a >> 7
                } else {
                    a & 1
                };
                let n = match y {
                    0 => a.rotate_left(1),
                    1 => a.rotate_right(1),
                    2 => (a << 1) | old,
                    3 => (a >> 1) | (old << 7),
                    4 => a << 1,
                    5 => (a >> 1) | (a & 128),
                    6 => (a << 1) | 1,
                    _ => a >> 1,
                };
                self.f = Self::szp(n) | c;
                n
            }
            1 => {
                self.f = (self.f & C)
                    | H
                    | if a & (1 << y) == 0 { Z | P } else { 0 }
                    | if y == 7 { a & S } else { 0 }
                    | if index != 0 {
                        (addr >> 8) as u8 & 0x28
                    } else {
                        a & 0x28
                    };
                a
            }
            2 => a & !(1 << y),
            _ => a | (1 << y),
        };
        if x != 1 {
            if index != 0 {
                bus.write(addr, n);
                if z != 6 {
                    self.set_reg(bus, z, 0, addr, n);
                }
            } else {
                self.set_reg(bus, z, 0, addr, n);
            }
        }
        if index != 0 {
            if x == 1 { 16 } else { 19 }
        } else if z == 6 {
            if x == 1 { 12 } else { 15 }
        } else {
            8
        }
    }

    fn ed(&mut self, bus: &mut impl Bus, op: u8) -> u32 {
        let x = op >> 6;
        let y = (op >> 3) & 7;
        let z = op & 7;
        let p = y >> 1;
        let q = y & 1;
        if x == 1 {
            return match z {
                0 => {
                    let n = bus.input(self.pair(0, 0));
                    if y != 6 {
                        self.set_reg(bus, y, 0, 0, n);
                    }
                    self.f = (self.f & C) | Self::szp(n);
                    12
                }
                1 => {
                    let n = if y == 6 { 0 } else { self.reg(bus, y, 0, 0) };
                    bus.output(self.pair(0, 0), n);
                    12
                }
                2 => {
                    let a = self.hl(0);
                    let b = self.pair(p, 0);
                    let carry = u32::from(self.f & C);
                    let n = if q == 0 {
                        u32::from(a).wrapping_sub(u32::from(b) + carry)
                    } else {
                        u32::from(a) + u32::from(b) + carry
                    };
                    let r = n as u16;
                    self.set_hl(0, r);
                    self.f = ((r >> 8) as u8 & (S | 0x28))
                        | if r == 0 { Z } else { 0 }
                        | if (a ^ b ^ r) & 0x1000 != 0 { H } else { 0 }
                        | if ((a ^ r) & if q == 0 { a ^ b } else { !(a ^ b) }) & 0x8000 != 0 {
                            P
                        } else {
                            0
                        }
                        | if n & 0x10000 != 0 { C } else { 0 }
                        | if q == 0 { N } else { 0 };
                    15
                }
                3 => {
                    let a = self.word(bus);
                    if q == 0 {
                        Self::write16(bus, a, self.pair(p, 0));
                    } else {
                        let n = Self::read16(bus, a);
                        self.set_pair(p, 0, n);
                    }
                    20
                }
                4 => {
                    let a = self.a;
                    self.a = 0;
                    self.sub(a, false, false);
                    8
                }
                5 => {
                    self.pc = self.pop(bus);
                    self.iff1 = self.iff2;
                    14
                }
                6 => {
                    self.im = match y {
                        2 | 6 => 1,
                        3 | 7 => 2,
                        _ => 0,
                    };
                    8
                }
                _ => {
                    match y {
                        0 => self.i = self.a,
                        1 => self.r = self.a,
                        2 | 3 => {
                            self.a = if y == 2 { self.i } else { self.r };
                            self.f =
                                (self.f & C) | Self::sz(self.a) | if self.iff2 { P } else { 0 };
                        }
                        4 | 5 => {
                            let a = self.hl(0);
                            let n = bus.read(a);
                            let result = if y == 4 {
                                (self.a << 4) | (n >> 4)
                            } else {
                                (n << 4) | (self.a & 15)
                            };
                            self.a = (self.a & 0xf0) | if y == 4 { n & 15 } else { n >> 4 };
                            bus.write(a, result);
                            self.f = (self.f & C) | Self::szp(self.a);
                        }
                        _ => {}
                    }
                    match y {
                        4 | 5 => 18,
                        6 | 7 => 8, // ED 77 and ED 7F: two-byte NOPs
                        _ => 9,
                    }
                }
            };
        }
        if x == 2 && y >= 4 && z <= 3 {
            let decrement = y & 1 != 0;
            let repeat = y & 2 != 0;
            let delta = if decrement { u16::MAX } else { 1 };
            let again;
            match z {
                0 => {
                    let n = bus.read(self.hl(0));
                    bus.write(self.pair(1, 0), n);
                    self.set_hl(0, self.hl(0).wrapping_add(delta));
                    self.set_pair(1, 0, self.pair(1, 0).wrapping_add(delta));
                    let bc = self.pair(0, 0).wrapping_sub(1);
                    self.set_pair(0, 0, bc);
                    let sum = self.a.wrapping_add(n);
                    self.f = (self.f & (S | Z | C))
                        | if bc != 0 { P } else { 0 }
                        | (sum & 8)
                        | ((sum & 2) << 4);
                    again = bc != 0;
                }
                1 => {
                    let n = bus.read(self.hl(0));
                    let r = self.a.wrapping_sub(n);
                    let half = (self.a ^ n ^ r) & 16 != 0;
                    self.set_hl(0, self.hl(0).wrapping_add(delta));
                    let bc = self.pair(0, 0).wrapping_sub(1);
                    self.set_pair(0, 0, bc);
                    let t = r.wrapping_sub(u8::from(half));
                    self.f = (self.f & C)
                        | (Self::sz(r) & !0x28)
                        | N
                        | if half { H } else { 0 }
                        | if bc != 0 { P } else { 0 }
                        | (t & 8)
                        | ((t & 2) << 4);
                    again = bc != 0 && r != 0;
                }
                2 => {
                    let n = bus.input(self.pair(0, 0));
                    bus.write(self.hl(0), n);
                    self.set_hl(0, self.hl(0).wrapping_add(delta));
                    self.b = self.b.wrapping_sub(1);
                    let t = u16::from(n)
                        + u16::from(self.c.wrapping_add(if decrement { 255 } else { 1 }));
                    self.block_io_flags(n, t);
                    again = self.b != 0;
                    if repeat && again {
                        self.block_io_repeat_flags(n);
                    }
                }
                _ => {
                    let n = bus.read(self.hl(0));
                    self.b = self.b.wrapping_sub(1);
                    bus.output(self.pair(0, 0), n);
                    self.set_hl(0, self.hl(0).wrapping_add(delta));
                    let t = u16::from(n) + u16::from(self.l);
                    self.block_io_flags(n, t);
                    again = self.b != 0;
                    if repeat && again {
                        self.block_io_repeat_flags(n);
                    }
                }
            }
            if repeat && again {
                self.pc = self.pc.wrapping_sub(2);
                21
            } else {
                16
            }
        } else {
            8
        } // Undefined ED encodings are two-byte NOPs on the Z80.
    }
    /// An INIR, OTIR, INDR or OTDR that goes round again also changes H and P/V by the carry, bit 7 of the
    /// data and B (SingleStepTests/z80 agrees on all 4,000 of its tests).
    fn block_io_repeat_flags(&mut self, n: u8) {
        let b = self.b;
        let (parity_of, half) = if self.f & C != 0 {
            if n & 0x80 != 0 {
                (b.wrapping_sub(1), b & 15 == 0)
            } else {
                (b.wrapping_add(1), b & 15 == 15)
            }
        } else {
            (b, self.f & H != 0)
        };
        if (parity_of & 7).count_ones() % 2 == 1 {
            self.f ^= P;
        }
        self.f = (self.f & !H) | if half { H } else { 0 };
    }
    fn block_io_flags(&mut self, n: u8, t: u16) {
        self.f = Self::sz(self.b)
            | if n & 128 != 0 { N } else { 0 }
            | if t > 255 { H | C } else { 0 }
            | if (((t as u8) & 7) ^ self.b).count_ones().is_multiple_of(2) {
                P
            } else {
                0
            };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Ram([u8; 65536]);
    impl Bus for Ram {
        fn read(&mut self, a: u16) -> u8 {
            self.0[a as usize]
        }
        fn write(&mut self, a: u16, v: u8) {
            self.0[a as usize] = v;
        }
    }
    fn run(code: &[u8], steps: usize) -> (Z80, Ram) {
        let mut ram = Ram([0; 65536]);
        ram.0[..code.len()].copy_from_slice(code);
        let mut cpu = Z80::new();
        for _ in 0..steps {
            cpu.step(&mut ram);
        }
        (cpu, ram)
    }
    #[test]
    fn arithmetic_and_overflow() {
        let (c, _) = run(&[0x3e, 0x7f, 0xc6, 1], 2);
        assert_eq!(c.a, 0x80);
        assert_eq!(c.f & (S | P | H | C), S | P | H);
        let (c, _) = run(&[0x3e, 0, 0xd6, 1], 2);
        assert_eq!(c.a, 255);
        assert_eq!(c.f & (N | C | H), N | C | H);
    }
    #[test]
    fn indexed_displacement_and_cb() {
        let (c, r) = run(
            &[
                0xdd, 0x21, 0, 0x20, 0xdd, 0x36, 0xfe, 0x81, 0xdd, 0xcb, 0xfe, 0x06, 0xdd, 0x7e,
                0xfe,
            ],
            4,
        );
        assert_eq!(r.0[0x1ffe], 3);
        assert_eq!(c.a, 3);
        assert_eq!(c.f & C, C);
    }
    #[test]
    fn ldir_copies_and_counts() {
        let (c, r) = run(&[0x21, 0, 0, 0x11, 0, 0x20, 0x01, 4, 0, 0xed, 0xb0], 7);
        assert_eq!(&r.0[0x2000..0x2004], &[0x21, 0, 0, 0x11]);
        assert_eq!(c.pair(0, 0), 0);
        assert_eq!(c.pc, 11);
    }
    #[test]
    fn ei_defers_irq_until_after_next_instruction() {
        let mut ram = Ram([0; 65536]);
        ram.0[..4].copy_from_slice(&[0xfb, 0, 0x76, 0]);
        let mut c = Z80::new();
        c.irq = true;
        c.step(&mut ram);
        assert_eq!(c.pc, 1);
        c.step(&mut ram);
        assert_eq!(c.pc, 2);
        assert_eq!(c.step(&mut ram), 13);
        assert_eq!(c.pc, 0x38);
        assert_eq!(ram.0[0xfffd], 2);
    }
    #[test]
    fn nmi_enters_0x66_keeps_iff1_for_retn_and_leaves_halt() {
        let mut ram = Ram([0; 65536]);
        // EI, HALT; the handler at 0x66 is RETN.
        ram.0[..3].copy_from_slice(&[0xfb, 0x76, 0]);
        ram.0[0x66..0x68].copy_from_slice(&[0xed, 0x45]);
        let mut c = Z80::new();
        c.step(&mut ram);
        c.step(&mut ram);
        assert!(c.halted && c.iff1);
        c.nmi = true;
        assert_eq!(c.step(&mut ram), 11);
        assert!(!c.nmi && !c.halted && !c.iff1 && c.iff2);
        assert_eq!(c.pc, 0x66);
        assert_eq!(ram.0[0xfffd], 2);
        c.step(&mut ram);
        // RETN copies IFF2 back, so the interrupted code can be interrupted again.
        assert!(c.iff1);
        assert_eq!(c.pc, 2);
    }
    #[test]
    fn nmi_is_not_held_back_by_ei_and_beats_an_interrupt() {
        let mut ram = Ram([0; 65536]);
        let mut c = Z80::new();
        c.iff1 = true;
        c.ei_delay = 2;
        c.irq = true;
        c.nmi = true;
        c.step(&mut ram);
        assert_eq!(c.pc, 0x66);
        // The handler runs with interrupts off, so the pending INT waits for it.
        c.ei_delay = 0;
        c.step(&mut ram);
        assert_eq!(c.pc, 0x67);
    }
}
