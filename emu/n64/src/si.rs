//! The serial interface and the PIF it reaches: 64 bytes of PIF RAM, read
//! and written whole by the SI's DMA (or a word at a time by the CPU), and
//! the commands the PIF takes in its last byte. The joybus frames in PIF
//! RAM are run when the SI reads it back (n64brew, PIF-NUS): a controller in
//! each of the first ports the host plugs in (one unless told more)
//! answers, and so does the cartridge's EEPROM on the fifth channel, if it
//! has one; the other ports have nothing.

use crate::{Cic, N64, SI_DMA, SI_INTR};
use savestate::state;

#[derive(Clone)]
pub struct Si {
    pub pif: [u8; 64],
    dram_addr: u32,
    busy: bool,
}

state!(Si { pif, dram_addr, busy });

impl Si {
    pub fn new() -> Si {
        Si { pif: [0; 64], dram_addr: 0, busy: false }
    }

    /// A word of PIF RAM (the PIF's ROM, locked once the console has
    /// booted, reads as 0).
    pub fn read_pif(&self, addr: u32) -> u32 {
        match addr & 0x7ff {
            i @ 0x7c0.. => {
                let i = i as usize - 0x7c0;
                u32::from_be_bytes(self.pif[i..i + 4].try_into().unwrap())
            }
            _ => 0,
        }
    }
}

impl N64 {
    pub(crate) fn read_si(&mut self, addr: u32) -> u32 {
        match addr & 0x1c {
            0x00 => self.si.dram_addr,
            // DMA busy, and the interrupt.
            0x18 => u32::from(self.si.busy) | u32::from(self.mi_intr & SI_INTR != 0) << 12,
            _ => 0,
        }
    }

    pub(crate) fn write_si(&mut self, addr: u32, value: u32) {
        match addr & 0x1c {
            0x00 => self.si.dram_addr = value & 0xff_fff8,
            0x04 => self.si_dma(false),
            0x10 => self.si_dma(true),
            0x18 => self.acknowledge(SI_INTR),
            _ => {}
        }
    }

    pub(crate) fn read_pif(&mut self, addr: u32) -> u32 {
        self.si.read_pif(addr)
    }

    /// The CPU writing a word of PIF RAM, which the PIF looks at at once.
    /// The SI bus takes words only: a narrower store writes the whole word
    /// the CPU sends, shifted into the store's lane (bus.rs).
    pub(crate) fn write_pif(&mut self, addr: u32, value: u32) {
        let i = (addr & 0x7ff) as usize;
        if i < 0x7c0 {
            return;
        }
        let i = i - 0x7c0;
        self.si.pif[i..i + 4].copy_from_slice(&value.to_be_bytes());
        self.pif_command();
    }

    /// A DMA of all 64 bytes of PIF RAM, from RDRAM or to it, which takes
    /// about 64 µs before the SI interrupt.
    fn si_dma(&mut self, to_pif: bool) {
        // Past the end of RDRAM nothing answers: reads are 0, writes lost.
        let d = self.si.dram_addr as usize & 0x7f_fff8;
        let n = 64.min(self.rdram.len() - d);
        if to_pif {
            self.touch_rdram(d, n);
        } else {
            self.wrote_rdram(d, n);
        }
        if to_pif {
            self.si.pif = [0; 64];
            self.si.pif[..n].copy_from_slice(&self.rdram[d..d + n]);
            self.pif_command();
        } else {
            self.run_joybus();
            self.rdram[d..d + n].copy_from_slice(&self.si.pif[..n]);
        }
        self.si.busy = true;
        let at = self.cycles + 6000;
        self.schedule(SI_DMA, at);
    }

    pub(crate) fn si_dma_done(&mut self) {
        self.si.busy = false;
        self.request(SI_INTR);
    }

    /// The commands in PIF RAM's last byte, as the PIF clears them when
    /// done: a joybus frame (run when it is read back), the CIC's challenge,
    /// the end of the boot and the locking of the PIF's ROM, and the boot
    /// code's checksum handshake, which answers in bit 7.
    fn pif_command(&mut self) {
        if self.si.pif[63] & 0x02 != 0 {
            let cic = self.cart.cic;
            challenge(cic, &mut self.si.pif[0x30..0x3f]);
        }
        let command = &mut self.si.pif[63];
        if *command & 0x20 != 0 {
            *command |= 0x80;
        }
        *command &= !0x7a;
    }

    /// Runs the joybus frame in PIF RAM: for each port in turn, a byte of
    /// how many bytes go out and one of how many come back, then those
    /// bytes. 0 skips a port, FE ends the frame, FD resets the port, FF is
    /// padding.
    fn run_joybus(&mut self) {
        if self.si.pif[63] & 1 != 0 {
            self.si.pif[63] &= !1;
        }
        let mut i = 0;
        let mut port = 0;
        while i < 63 && port < 5 {
            let tx = self.si.pif[i];
            i += 1;
            match tx {
                0x00 => {
                    port += 1;
                    continue;
                }
                0xfd => {
                    port += 1;
                    continue;
                }
                0xfe => break,
                0xff => continue,
                _ => {}
            }
            let rx_at = i;
            let (tx, rx) = (usize::from(tx & 0x3f), usize::from(self.si.pif[i] & 0x3f));
            i += 1;
            if i + tx + rx > 64 {
                break;
            }
            let command: Vec<u8> = self.si.pif[i..i + tx].to_vec();
            let reply = self.joybus(port, &command);
            match reply {
                Some(reply) => {
                    let n = reply.len().min(rx);
                    self.si.pif[i + tx..i + tx + n].copy_from_slice(&reply[..n]);
                }
                // Nothing on the port: the "no response" bit.
                None => self.si.pif[rx_at] |= 0x80,
            }
            i += tx + rx;
            port += 1;
        }
    }

    /// What a port answers a joybus command with: a standard controller in
    /// the ports plugged in, with a Rumble Pak in it or no pak, and the
    /// cartridge's EEPROM on channel 4.
    fn joybus(&mut self, port: usize, command: &[u8]) -> Option<Vec<u8>> {
        if port == 4 {
            return self.save.eeprom(command);
        }
        if port >= self.controllers {
            return None;
        }
        match command.first()? {
            // Its kind (a controller) and status (a pak in it, or none).
            0x00 | 0xff => Some(vec![0x05, 0x00, if self.rumble_paks { 0x01 } else { 0x02 }]),
            // A Rumble Pak read (n64brew, Rumble Pak): 0x80 from 0x8000, where a game
            // finds which pak it is, else 0; 32 bytes and their CRC.
            0x02 if self.rumble_paks && command.len() >= 3 => {
                let address = u16::from_be_bytes([command[1], command[2]]) & 0xffe0;
                let fill = if address & 0xe000 == 0x8000 { 0x80 } else { 0x00 };
                let mut reply = vec![fill; 32];
                reply.push(pak_crc(&reply));
                Some(reply)
            }
            // A write: to 0xC000 it runs the motor (a non-zero byte) or stops it.
            0x03 if self.rumble_paks && command.len() >= 35 => {
                let address = u16::from_be_bytes([command[1], command[2]]) & 0xffe0;
                let data = &command[3..35];
                if address & 0xe000 == 0xc000 {
                    self.motors[port] = data[0] != 0;
                }
                Some(vec![pak_crc(data)])
            }
            0x01 => {
                let [high, low] = self.buttons[port].to_be_bytes();
                let (x, y) = self.stick[port];
                Some(vec![high, low, x as u8, y as u8])
            }
            _ => None,
        }
    }
}

/// The CRC of a pak's 32 bytes, as the controller sends it after a read or
/// a write: CRC-8 over the bytes and a zero byte, polynomial 0x85.
fn pak_crc(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for i in 0..=data.len() {
        for bit in (0..8).rev() {
            let tap = if crc & 0x80 != 0 { 0x85 } else { 0 };
            crc <<= 1;
            if i < data.len() && data[i] >> bit & 1 != 0 {
                crc |= 1;
            }
            crc ^= tap;
        }
    }
    crc
}

/// The CIC's challenge and response (n64brew, CIC-NUS), which games with a
/// CIC 6105 use to check it is there: the PIF hands the CIC 15 bytes of
/// PIF RAM, a nibble at a time (the high one first), and puts back what it
/// answers. The 6105 runs each nibble through its own sum, carried from
/// one to the next; this is its routine as UltraCIC_C has it (cic_c.c,
/// Cic6105Algo), which gives what mupen64plus's (X-Scale's) does. Other
/// CICs only send the challenge back inverted.
fn challenge(cic: Cic, bytes: &mut [u8]) {
    if cic != Cic::Cic6105 {
        bytes.iter_mut().for_each(|b| *b = !*b);
        return;
    }
    let mut a = 5u8;
    let mut carry = 1u8;
    for byte in bytes.iter_mut() {
        let mut out = 0;
        for shift in [4, 0] {
            let nibble = *byte >> shift & 0xf;
            if nibble & 1 == 0 {
                a += 8;
            }
            if a & 2 == 0 {
                a += 4;
            }
            let x = (a + nibble) & 0xf;
            if carry == 0 {
                a = x + 7;
            } else {
                a = x;
            }
            a = (a + x) & 0xf;
            a = a + x + carry;
            carry = u8::from(a >= 0x10);
            a = !a & 0xf;
            out |= a << shift;
        }
        *byte = out;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A controller answers in each port plugged in, with its own buttons
    /// and stick; the ports after them answer nothing.
    #[test]
    fn controllers_in_the_ports_plugged_in() {
        let mut rom = vec![0u8; 0x1000];
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        let mut n64 = crate::N64::new(&rom).unwrap();
        assert_eq!(n64.joybus(0, &[0x00]), Some(vec![0x05, 0x00, 0x02]));
        assert_eq!(n64.joybus(1, &[0x00]), None);
        n64.set_controllers(2);
        n64.set_controller(1, 0x8000, (10, -20));
        assert_eq!(n64.joybus(1, &[0x00]), Some(vec![0x05, 0x00, 0x02]));
        assert_eq!(n64.joybus(1, &[0x01]), Some(vec![0x80, 0x00, 10, -20i8 as u8]));
        assert_eq!(n64.joybus(2, &[0x00]), None);
    }

    /// A Rumble Pak: the status says a pak is in, it reads 0x80 at 0x8000,
    /// where libultra's osMotorInit looks, and a write to 0xC000 runs or
    /// stops its motor.
    #[test]
    fn a_rumble_pak() {
        let mut rom = vec![0u8; 0x1000];
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        let mut n64 = crate::N64::new(&rom).unwrap();
        assert_eq!(n64.joybus(0, &[0x02, 0x80, 0x01]), None);
        n64.set_rumble_paks(true);
        assert_eq!(n64.joybus(0, &[0x00]), Some(vec![0x05, 0x00, 0x01]));
        // 0x8001: the address with its own CRC in the low bits.
        let read = n64.joybus(0, &[0x02, 0x80, 0x01]).unwrap();
        assert_eq!((&read[..32], read[32]), (&[0x80; 32][..], 0xb8));
        assert_eq!(n64.joybus(0, &[0x02, 0x00, 0x00]).unwrap()[..33], [[0; 32].as_slice(), &[0]].concat()[..]);
        let mut write = vec![0x03, 0xc0, 0x1b];
        write.extend([0x01; 32]);
        assert_eq!(n64.joybus(0, &write), Some(vec![0xeb]));
        assert!(n64.motor(0) && !n64.motor(1));
        write[3..].fill(0);
        n64.joybus(0, &write);
        assert!(!n64.motor(0));
    }

    /// A challenge, and the answer UltraCIC_C's and mupen64plus's routines
    /// both give.
    #[test]
    fn cic_6105_challenge() {
        let mut bytes = [0x68, 0x98, 0x86, 0xa4, 0xa5, 0xa0, 0xca, 0x3c, 0x3c, 0xd5, 0x6c, 0xd9, 0x12, 0x0c, 0x90];
        challenge(Cic::Cic6105, &mut bytes);
        assert_eq!(bytes, [0x97, 0xee, 0x4c, 0x80, 0x67, 0x37, 0xd5, 0x4a, 0x7d, 0x47, 0xf5, 0x6b, 0xa2, 0xa4, 0xbf]);
    }
}
