//! Two six-button controllers, multiplexed over the I/O chip's TH line.
use crate::{A, B, C, DOWN, LEFT, MODE, RIGHT, START, UP, X, Y, Z};
use savestate::state;

#[derive(Clone)]
pub(crate) struct Io {
    pub pads: [u16; 2],
    data: [u8; 3],
    control: [u8; 3],
    phase: [u8; 3],
    age: [u32; 3],
    serial: [u8; 18],
}
state!(Io {
    pads,
    data,
    control,
    phase,
    age,
    serial
});

impl Io {
    pub fn new() -> Self {
        Self {
            pads: [0; 2],
            data: [0x7f; 3],
            control: [0; 3],
            phase: [0; 3],
            age: [0; 3],
            serial: [0; 18],
        }
    }

    fn th(&self, port: usize) -> bool {
        self.control[port] & 0x40 == 0 || self.data[port] & 0x40 != 0
    }

    pub fn read(&self, register: usize, version: u8) -> u8 {
        match register {
            0 => version,
            1..=3 => {
                let port = register - 1;
                let th = self.th(port);
                let pad = self.pads.get(port).copied().unwrap_or(0);
                let mut pins = 0x3f;
                let map: &[(u16, u8)] = if th {
                    if self.phase[port] == 3 {
                        &[(Z, 0), (Y, 1), (X, 2), (MODE, 3), (B, 4), (C, 5)]
                    } else {
                        &[(UP, 0), (DOWN, 1), (LEFT, 2), (RIGHT, 3), (B, 4), (C, 5)]
                    }
                } else {
                    pins &= if self.phase[port] == 2 { 0x30 } else { 0x33 };
                    &[(UP, 0), (DOWN, 1), (A, 4), (START, 5)]
                };
                for &(button, bit) in map {
                    if pad & button != 0 {
                        pins &= !(1 << bit);
                    }
                }
                // The fourth low phase identifies a six-button pad by
                // returning ones on the four direction pins.
                if !th && self.phase[port] >= 3 {
                    pins |= 0x0f;
                }
                pins |= u8::from(th) << 6;
                (pins & !self.control[port]) | (self.data[port] & self.control[port])
            }
            4..=6 => self.control[register - 4],
            7..=15 => self.serial[register - 7],
            _ => 0xff,
        }
    }

    pub fn write(&mut self, register: usize, value: u8) {
        match register {
            1..=3 => {
                let port = register - 1;
                let old = self.th(port);
                self.data[port] = value;
                if !old && self.th(port) {
                    self.phase[port] = self.phase[port].saturating_add(1).min(4);
                    self.age[port] = 0;
                }
            }
            4..=6 => self.control[register - 4] = value,
            7..=15 => self.serial[register - 7] = value,
            _ => {}
        }
    }

    pub fn advance(&mut self, master_cycles: u32) {
        for port in 0..3 {
            self.age[port] = self.age[port].saturating_add(master_cycles);
            // The controller times out after roughly 1.5 milliseconds.
            if self.age[port] > 80_000 {
                self.phase[port] = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn buttons_and_th_multiplex() {
        let mut io = Io::new();
        io.pads[0] = A | C | LEFT | START;
        io.write(4, 0x40);
        assert_eq!(io.read(1, 0) & 0x7f, 0x5b);
        io.write(1, 0);
        assert_eq!(io.read(1, 0) & 0x7f, 3);
        assert_eq!(io.read(2, 0) & 0x3f, 0x3f);
    }
    #[test]
    fn extended_pad_and_timeout() {
        let mut io = Io::new();
        io.pads[0] = X | Z;
        io.write(4, 0x40);
        for _ in 0..3 {
            io.write(1, 0);
            io.write(1, 0x40);
        }
        assert_eq!(io.read(1, 0) & 0xf, 0xa);
        io.advance(100_000);
        assert_eq!(io.read(1, 0) & 0xf, 0xf);
    }
}
