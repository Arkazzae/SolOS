//! SIO0: the serial port the pads and memory cards share. The CPU selects
//! a port (JOY_CTRL), sends a byte and gets one back as the device sends
//! its own; a device that wants the next byte pulls /ACK a little after,
//! which can raise IRQ7. A digital pad answers 01h (its address) with FFh,
//! the read command 42h with its ID (41h) and 5Ah, then two bytes of
//! buttons, low when pressed. A memory card answers 81h, then reads,
//! writes and identifies itself sector by sector (128 bytes; 1024 of
//! them), each with a checksum.

use crate::Psx;
use savestate::state;

/// The pad's buttons, as the host gives them (set when pressed).
pub mod buttons {
    pub const SELECT: u16 = 1 << 0;
    pub const L3: u16 = 1 << 1;
    pub const R3: u16 = 1 << 2;
    pub const START: u16 = 1 << 3;
    pub const UP: u16 = 1 << 4;
    pub const RIGHT: u16 = 1 << 5;
    pub const DOWN: u16 = 1 << 6;
    pub const LEFT: u16 = 1 << 7;
    pub const L2: u16 = 1 << 8;
    pub const R2: u16 = 1 << 9;
    pub const L1: u16 = 1 << 10;
    pub const R1: u16 = 1 << 11;
    pub const TRIANGLE: u16 = 1 << 12;
    pub const CIRCLE: u16 = 1 << 13;
    pub const CROSS: u16 = 1 << 14;
    pub const SQUARE: u16 = 1 << 15;
}

/// Cycles from the end of a byte to the device's /ACK, and how long /ACK
/// stays low.
const ACK_DELAY: u64 = 338;
const ACK_PULSE: u64 = 96;

/// A memory card's size: 1024 sectors of 128 bytes.
pub const CARD: usize = 0x2_0000;

#[derive(Clone, Copy, Default, PartialEq)]
enum Device {
    #[default]
    None,
    Pad,
    /// Address 01h through a Multitap, and slots B to D's pads (1 to 3) by
    /// their own addresses (crate::tap).
    Tap,
    TapSlot(u8),
    /// A memory card, and the command it was given (R, W or S).
    Card(u8),
    /// Not a device that answers: the rest of the transfer reads FFh.
    Nothing,
}

/// A memory card in a slot.
#[derive(Clone)]
pub struct Card {
    pub data: Box<[u8; CARD]>,
    /// The sector of the command, the byte before, the checksum so far,
    /// and a sector being written.
    sector: u16,
    previous: u8,
    checksum: u8,
    buffer: [u8; 128],
}

impl Card {
    /// A card as formatted: the header, fifteen free directory entries,
    /// no broken sectors.
    pub fn formatted() -> Card {
        let mut data: Box<[u8; CARD]> = vec![0; CARD].into_boxed_slice().try_into().unwrap();
        let frame = |data: &mut [u8; CARD], n: usize, f: &dyn Fn(&mut [u8])| {
            let s = &mut data[n * 128..n * 128 + 128];
            f(s);
            s[127] = s[..127].iter().fold(0, |a, b| a ^ b);
        };
        for n in [0, 63] {
            frame(&mut data, n, &|s| s[..2].copy_from_slice(b"MC"));
        }
        for n in 1..16 {
            frame(&mut data, n, &|s| {
                s[0] = 0xa0;
                s[8..10].fill(0xff);
            });
        }
        for n in 16..36 {
            frame(&mut data, n, &|s| {
                s[..4].fill(0xff);
                s[8..10].fill(0xff);
            });
        }
        Card::new(data)
    }

    pub fn new(data: Box<[u8; CARD]>) -> Card {
        Card { data, sector: 0, previous: 0, checksum: 0, buffer: [0; 128] }
    }

    /// A sector as a read gives it, for the BIOS's driver the core answers;
    /// None past the card's end.
    pub(crate) fn sector(&self, n: u32) -> Option<&[u8]> {
        (n <= 0x3ff).then(|| &self.data[n as usize * 128..][..128])
    }


    /// The card's answer to a byte of a command (`step` counts from the
    /// command byte after 81h), with its flag; whether it wants another,
    /// and whether it was written.
    fn transfer(&mut self, flag: &mut u8, command: u8, step: u32, value: u8) -> (u8, bool, bool) {
        let previous = std::mem::replace(&mut self.previous, value);
        match (command, step) {
            (_, 0) => (*flag, matches!(value, b'R' | b'W' | b'S'), false),
            (_, 1) => (0x5a, true, false),
            (_, 2) => (0x5d, true, false),
            (b'S', 3) => (0x5c, true, false),
            (b'S', 4) => (0x5d, true, false),
            (b'S', 5) => (0x04, true, false),
            (b'S', 6 | 7) => (0x00, true, false),
            (b'S', _) => (0x80, false, false),
            (_, 3) => {
                self.sector = u16::from(value) << 8;
                self.checksum = value;
                (0x00, true, false)
            }
            (_, 4) => {
                self.sector |= u16::from(value);
                self.checksum ^= value;
                (previous, true, false)
            }
            (b'R', 5) => (0x5c, true, false),
            (b'R', 6) => (0x5d, true, false),
            (b'R', 7 | 8) if self.sector > 0x3ff => (0xff, step == 7, false),
            (b'R', 7) => ((self.sector >> 8) as u8, true, false),
            (b'R', 8) => (self.sector as u8, true, false),
            (b'R', 9..=136) => {
                let byte = self.data[usize::from(self.sector) * 128 + step as usize - 9];
                self.checksum ^= byte;
                (byte, true, false)
            }
            (b'R', 137) => (self.checksum, true, false),
            (b'R', _) => (0x47, false, false),
            (b'W', 5..=132) => {
                self.buffer[step as usize - 5] = value;
                self.checksum ^= value;
                (previous, true, false)
            }
            (b'W', 133) => {
                // The checksum: the sector is written if it and the number
                // are good.
                let good = value == self.checksum && self.sector <= 0x3ff;
                self.checksum = if self.sector > 0x3ff {
                    0xff
                } else if good {
                    0x47
                } else {
                    0x4e
                };
                if good {
                    let at = usize::from(self.sector) * 128;
                    self.data[at..at + 128].copy_from_slice(&self.buffer);
                    *flag &= !0x08;
                }
                (previous, true, good)
            }
            (b'W', 134) => (0x5c, true, false),
            (b'W', 135) => (0x5d, true, false),
            (b'W', _) => (self.checksum, false, false),
            _ => (0xff, false, false),
        }
    }
}

#[derive(Clone, Default)]
pub struct Sio {
    ctrl: u16,
    mode: u16,
    baud: u16,
    rx: Option<u8>,
    /// /ACK is low, and the interrupt it raised.
    ack: bool,
    irq: bool,
    device: Device,
    step: u8,
    /// The byte coming back while one goes out, when it has come, and
    /// when the device pulls /ACK after it.
    incoming: Option<u8>,
    rx_at: Option<u64>,
    ack_at: Option<u64>,
    /// The pads' buttons, set when pressed: a pad in port 1, and one in
    /// port 2 while the host plugs it in (the host's, not saved); and each
    /// one's DualShock, its mode saved after the machine (Psx::save_state).
    pub pads: [u16; 2],
    pub second_pad: bool,
    pub dual_shocks: [crate::pad::Pad; 2],
    /// A Multitap in a port while plugged in, slots B to D's pads with it;
    /// saved after the DualShocks.
    pub tap: crate::tap::Tap,
    /// The memory cards in the slots, and whether one was written since
    /// the host last asked.
    pub cards: [Option<Card>; 2],
    pub card_written: bool,
    /// Each card's flag: bit 3 until it is written after it was put in.
    /// The machine's, not the host's: the card stays in while the machine
    /// is saved, and a game that cleared the bit finds no new card when
    /// the state is loaded.
    card_flags: [u8; 2],
}

state!(Sio { ctrl, mode, baud, rx, ack, irq, step, incoming, rx_at, ack_at, pads, card_flags });

impl Sio {
    /// With a formatted memory card in slot 1.
    pub fn new() -> Sio {
        Sio { cards: [Some(Card::formatted()), None], card_flags: [0x08; 2], ..Sio::default() }
    }

    /// Puts a card in a slot (or takes it out), new to the console.
    pub fn insert(&mut self, slot: usize, card: Option<Card>) {
        self.cards[slot] = card;
        self.card_flags[slot] = 0x08;
    }

    /// Whether the card in a slot was not written since it was put in: the
    /// BIOS's driver reports it as a new card.
    pub(crate) fn card_fresh(&self, slot: usize) -> bool {
        self.card_flags[slot] & 0x08 != 0
    }

    /// Writes a sector of the card in a slot as a write command does; false
    /// past the card's end or without a card.
    pub(crate) fn write_card_sector(&mut self, slot: usize, n: u32, sector: &[u8]) -> bool {
        let Some(card) = self.cards[slot].as_mut().filter(|_| n <= 0x3ff) else { return false };
        card.data[n as usize * 128..][..128].copy_from_slice(sector);
        self.card_flags[slot] &= !0x08;
        self.card_written = true;
        true
    }

    /// When something next happens.
    pub fn due(&self) -> u64 {
        self.rx_at.unwrap_or(u64::MAX).min(self.ack_at.unwrap_or(u64::MAX))
    }

    /// Cycles a byte takes: eight bits at the baud rate (JOY_BAUD times
    /// JOY_MODE's factor).
    fn byte_time(&self) -> u64 {
        let factor = match self.mode & 3 {
            2 => 16,
            3 => 64,
            _ => 1,
        };
        u64::from(self.baud.max(1)) * factor * 8
    }

    pub fn read(&mut self, offset: u32) -> u32 {
        match offset {
            0x0 => u32::from(self.rx.take().unwrap_or(0xff)),
            0x4 => {
                // Ready to send; the byte sent is out once its answer came.
                let mut s = 1 | u32::from(self.rx_at.is_none()) << 2;
                s |= u32::from(self.rx.is_some()) << 1;
                s |= u32::from(self.ack) << 7;
                s |= u32::from(self.irq) << 9;
                s
            }
            0x8 => u32::from(self.mode),
            0xa => u32::from(self.ctrl),
            0xe => u32::from(self.baud),
            _ => 0,
        }
    }

    /// A byte sent; whether the device will pull /ACK.
    fn transfer(&mut self, value: u8) -> bool {
        let selected = self.ctrl & 2 != 0;
        let port = usize::from(self.ctrl >> 13 & 1);
        let (reply, ack) = if !selected {
            (0xff, false)
        } else {
            match (self.device, self.step) {
                // A Multitap: 01h is slot A's (or the long access), 02h to 04h slots B to D's.
                (Device::None, _) if self.tap.port == Some(port) && matches!(value, 0x01..=0x04) => match value {
                    0x01 => {
                        self.tap.begin();
                        self.device = Device::Tap;
                        (0xff, true)
                    }
                    n if self.tap.plugged[usize::from(n) - 2] => {
                        self.device = Device::TapSlot(n - 1);
                        (0xff, true)
                    }
                    _ => {
                        self.device = Device::Nothing;
                        (0xff, false)
                    }
                },
                (Device::Tap, step) => {
                    let tap = &mut self.tap;
                    if step == 2 {
                        tap.request(value);
                    }
                    let a = (&mut self.dual_shocks[port], self.pads[port]);
                    match (tap.kind, step) {
                        (crate::tap::SLOT_A, _) => a.0.transfer(step, value, a.1),
                        (_, 1) => (0x80, true),
                        (_, 2) => (0x5a, true),
                        // The garbage one: slot A's ID's low byte, and no more.
                        (crate::tap::GARBAGE, _) => (if a.0.analog() { 0x73 } else { 0x41 }, false),
                        (_, 3..=34) => {
                            let at = step - 3;
                            (tap.segment(usize::from(at / 8), at % 8, value, a), step < 34)
                        }
                        _ => (0xff, false),
                    }
                }
                (Device::TapSlot(n), step) => {
                    let n = usize::from(n) - 1;
                    self.tap.pads[n].transfer(step, value, self.tap.buttons[n])
                }
                (Device::None, _) => {
                    if value == 0x01 && (port == 0 || self.second_pad) {
                        self.device = Device::Pad;
                        (0xff, true)
                    } else if value == 0x81 && self.cards[port].is_some() {
                        self.device = Device::Card(0);
                        (0xff, true)
                    } else {
                        // An empty port: nothing answers.
                        self.device = Device::Nothing;
                        (0xff, false)
                    }
                }
                (Device::Card(command), step) => {
                    let command = if step == 1 { value } else { command };
                    self.device = Device::Card(command);
                    let card = self.cards[port].as_mut().unwrap();
                    let flag = &mut self.card_flags[port];
                    let (reply, ack, written) = card.transfer(flag, command, u32::from(step) - 1, value);
                    self.card_written |= written;
                    (reply, ack)
                }
                (Device::Pad, step) => self.dual_shocks[port].transfer(step, value, self.pads[port]),
                _ => (0xff, false),
            }
        };
        self.step = self.step.saturating_add(1);
        if !ack {
            self.device = Device::Nothing;
        }
        self.incoming = Some(reply);
        self.ack = false;
        ack
    }

    fn write_ctrl(&mut self, value: u16) {
        if value & 0x40 != 0 {
            let cards = std::mem::take(&mut self.cards);
            // The pads are not reset with the port: a DualShock stays in its mode.
            let (dual_shocks, tap) = (std::mem::take(&mut self.dual_shocks), std::mem::take(&mut self.tap));
            let (pads, second_pad, card_written, card_flags) = (self.pads, self.second_pad, self.card_written, self.card_flags);
            *self = Sio { pads, second_pad, dual_shocks, tap, cards, card_written, card_flags, ..Sio::default() };
            return;
        }
        // Acknowledging clears the interrupt.
        if value & 0x10 != 0 {
            self.irq = false;
        }
        self.ctrl = value & !0x50;
        if value & 2 == 0 {
            self.device = Device::None;
            self.step = 0;
            self.ack_at = None;
        }
    }
}

impl Psx {
    pub(crate) fn sio_write(&mut self, offset: u32, value: u32) {
        match offset {
            0x0 => {
                let ack = self.sio.transfer(value as u8);
                let done = self.cycles + self.sio.byte_time();
                self.sio.rx_at = Some(done);
                self.sio.ack_at = if ack { Some(done + ACK_DELAY) } else { None };
                self.schedule();
            }
            0x8 => self.sio.mode = value as u16,
            0xa => {
                self.sio.write_ctrl(value as u16);
                self.schedule();
            }
            0xe => self.sio.baud = value as u16,
            _ => {}
        }
    }

    /// The answer comes in; the device pulls /ACK: IRQ7 if JOY_CTRL lets
    /// it through.
    pub(crate) fn sio_update(&mut self) {
        if self.sio.rx_at.is_some_and(|at| at <= self.cycles) {
            self.sio.rx_at = None;
            self.sio.rx = self.sio.incoming.take();
        }
        if self.sio.ack_at.is_some_and(|at| at <= self.cycles) && self.sio.ack {
            // The pulse ends.
            self.sio.ack_at = None;
            self.sio.ack = false;
        } else if self.sio.ack_at.is_some_and(|at| at <= self.cycles) {
            self.sio.ack_at = Some(self.cycles + ACK_PULSE);
            self.sio.ack = true;
            if self.sio.ctrl & 1 << 12 != 0 {
                self.sio.irq = true;
                self.request(crate::PAD);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes sent to port 1, and what came back for each.
    fn exchange(sio: &mut Sio, bytes: &[u8]) -> Vec<u8> {
        sio.write_ctrl(0x1003);
        let replies = bytes.iter().map(|&b| {
            sio.transfer(b);
            sio.incoming.take().unwrap()
        });
        let replies = replies.collect();
        sio.write_ctrl(0);
        replies
    }

    #[test]
    fn card_writes_and_reads_a_sector() {
        let mut sio = Sio::new();
        let data: Vec<u8> = (0..128).map(|i| (i * 3) as u8).collect();
        let checksum = data.iter().fold(0x01 ^ 0x23, |a, b| a ^ b);
        let mut write = vec![0x81, b'W', 0, 0, 0x01, 0x23];
        write.extend(&data);
        write.extend([checksum, 0, 0, 0]);
        let replies = exchange(&mut sio, &write);
        assert_eq!(replies[1..4], [0x08, 0x5a, 0x5d]);
        assert_eq!(replies[replies.len() - 3..], [0x5c, 0x5d, 0x47]);
        assert!(sio.card_written);

        let mut read = vec![0x81, b'R', 0, 0, 0x01, 0x23, 0, 0, 0, 0];
        read.extend([0; 130]);
        let replies = exchange(&mut sio, &read);
        // The write cleared the flag's bit 3.
        assert_eq!(replies[1], 0x00);
        assert_eq!(replies[6..10], [0x5c, 0x5d, 0x01, 0x23]);
        assert_eq!(replies[10..138], data[..]);
        assert_eq!(replies[138..140], [checksum, 0x47]);
    }

    /// The second port answers as a pad only while one is plugged in, with
    /// its own buttons.
    #[test]
    fn a_pad_in_the_second_port() {
        let mut sio = Sio::new();
        let poll = |sio: &mut Sio| {
            sio.write_ctrl(0x3003);
            let replies: Vec<(u8, bool)> = [0x01, 0x42, 0, 0, 0].iter().map(|&b| {
                let ack = sio.transfer(b);
                (sio.incoming.take().unwrap(), ack)
            }).collect();
            sio.write_ctrl(0);
            replies
        };
        // Empty: nothing acknowledges the first byte.
        assert!(!poll(&mut sio)[0].1);
        sio.second_pad = true;
        sio.pads[1] = 0x0040;
        let replies: Vec<u8> = poll(&mut sio).iter().map(|r| r.0).collect();
        assert_eq!(replies[1..], [0x41, 0x5a, !0x40u8, 0xff]);
    }

    /// Bytes sent to port 1, and what came back for each with its /ACK.
    fn acks(sio: &mut Sio, bytes: &[u8]) -> Vec<(u8, bool)> {
        sio.write_ctrl(0x1003);
        let replies = bytes.iter().map(|&b| {
            let ack = sio.transfer(b);
            (sio.incoming.take().unwrap(), ack)
        });
        let replies = replies.collect();
        sio.write_ctrl(0);
        replies
    }

    /// A Multitap in port 1 with pads in slots A to C, digital, each with
    /// its own buttons held.
    fn tapped() -> Sio {
        let mut sio = Sio::new();
        sio.tap.port = Some(0);
        sio.tap.plugged = [true, true, false];
        sio.pads[0] = 0x0001;
        sio.tap.buttons = [0x0002, 0x0004, 0x0008];
        sio
    }

    /// A long access: 01h, 42h with REQ, then each slot's 42h, 00h and six
    /// more bytes.
    fn long(req: u8) -> Vec<u8> {
        let mut bytes = vec![0x01, 0x42, req];
        for _ in 0..4 {
            bytes.extend([0x42, 0x00, 0, 0, 0, 0, 0, 0]);
        }
        bytes
    }

    /// psx-spx's table: REQ in an access to 01h makes the next one long,
    /// asked again from a long one it is garbage, and without it slot A's.
    #[test]
    fn multitap_toggles_slot_a_long_and_garbage() {
        let mut sio = tapped();
        let slot_a = acks(&mut sio, &[0x01, 0x42, 0x01, 0, 0]);
        assert_eq!(slot_a.iter().map(|r| r.0).collect::<Vec<_>>(), [0xff, 0x41, 0x5a, 0xfe, 0xff]);
        let replies = acks(&mut sio, &long(0x00));
        let bytes: Vec<u8> = replies.iter().map(|r| r.0).collect();
        let pad = |held: u16| [0x41, 0x5a, !(held as u8), !((held >> 8) as u8), 0xff, 0xff, 0xff, 0xff];
        assert_eq!(bytes[..3], [0xff, 0x80, 0x5a]);
        assert_eq!(bytes[3..11], pad(0x0001));
        assert_eq!(bytes[11..19], pad(0x0002));
        assert_eq!(bytes[19..27], pad(0x0004));
        assert_eq!(bytes[27..35], [0xff; 8]);
        assert!(replies[..34].iter().all(|r| r.1) && !replies[34].1);
        // REQ was 0: slot A's again.
        assert_eq!(acks(&mut sio, &[0x01, 0x42, 0x01, 0, 0])[1].0, 0x41);
        // REQ every time: long, garbage, long.
        assert_eq!(acks(&mut sio, &long(0x01))[1].0, 0x80);
        let garbage = acks(&mut sio, &long(0x01));
        assert_eq!(garbage[..4], [(0xff, true), (0x80, true), (0x5a, true), (0x41, false)]);
        assert_eq!(acks(&mut sio, &long(0x01))[1].0, 0x80);
    }

    /// Slots B to D by their own addresses, an empty one not answering; a
    /// slot's DualShock put in analog mode there gives its sticks in the
    /// long access.
    #[test]
    fn multitap_slots_by_their_addresses() {
        let mut sio = tapped();
        let b = acks(&mut sio, &[0x02, 0x42, 0x00, 0, 0]);
        assert_eq!(b.iter().map(|r| r.0).collect::<Vec<_>>(), [0xff, 0x41, 0x5a, 0xfd, 0xff]);
        assert!(!acks(&mut sio, &[0x04, 0x42])[0].1);
        // Configuration mode, analog on, out again: slot B's pad alone.
        acks(&mut sio, &[0x02, 0x43, 0x00, 0x01, 0, 0, 0]);
        acks(&mut sio, &[0x02, 0x44, 0x00, 0x01, 0x03, 0, 0, 0, 0]);
        acks(&mut sio, &[0x02, 0x43, 0x00, 0x00, 0, 0, 0, 0, 0]);
        assert!(sio.tap.pads[0].analog() && !sio.dual_shocks[0].analog());
        sio.tap.pads[0].sticks = [1, 2, 3, 4];
        acks(&mut sio, &[0x01, 0x42, 0x01, 0, 0]);
        let bytes: Vec<u8> = acks(&mut sio, &long(0x00)).iter().map(|r| r.0).collect();
        assert_eq!(bytes[11..19], [0x73, 0x5a, 0xfd, 0xff, 1, 2, 3, 4]);
    }

    #[test]
    fn formatted_card_has_its_header() {
        let card = Card::formatted();
        assert_eq!(&card.data[..2], b"MC");
        assert_eq!(card.data[127], 0x0e);
        assert_eq!(card.data[128], 0xa0);
    }
}
