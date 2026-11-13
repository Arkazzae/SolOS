//! A DualShock in a controller port (psx-spx, Controllers and Memory Cards):
//! a digital pad (ID 41h) until a game puts it in analog mode (73h), which
//! adds the two sticks to each poll, through its configuration mode (F3h),
//! where the game also maps its two motors to bytes it sends with the poll.
//! A game made for the digital pad never leaves digital mode, so it sees the
//! pad it knows.
//!
//! An exchange after the address byte (01h): the command, then 5Ah, then
//! the data, two bytes in digital mode and six otherwise, while the game's
//! bytes going the other way are the command's parameters.

use savestate::state;

#[derive(Clone)]
pub struct Pad {
    /// The sticks: right x, right y, left x, left y, 80h at rest, 00h left
    /// and up (the host's).
    pub sticks: [u8; 4],
    analog: bool,
    config: bool,
    /// Which motor each data byte of a poll drives (4Dh): 00h the small
    /// one, 01h the large, anything else none.
    map: [u8; 6],
    /// The small motor runs, and the large one's strength (the host's to
    /// feel).
    pub small: bool,
    pub large: u8,
    /// The exchange going on: its command, its data bytes and how many.
    command: u8,
    data: [u8; 6],
    len: u8,
}

// The mode and the motors' map: a game sets them once, at its start.
state!(Pad { analog, config, map });

impl Default for Pad {
    fn default() -> Pad {
        Pad {
            sticks: [0x80; 4],
            analog: false,
            config: false,
            map: [0xff; 6],
            small: false,
            large: 0,
            command: 0,
            data: [0; 6],
            len: 0,
        }
    }
}

impl Pad {
    pub fn analog(&self) -> bool {
        self.analog
    }

    /// The byte answering `value`, the exchange's `step`th (from 1, after
    /// the address byte), with these buttons held (bit set: held); and
    /// whether the pad pulls /ACK after it, as it does but after the last.
    pub fn transfer(&mut self, step: u8, value: u8, buttons: u16) -> (u8, bool) {
        match step {
            1 => self.command(value, buttons),
            2 => (0x5a, true),
            _ => {
                let i = usize::from(step - 3);
                if i >= usize::from(self.len) {
                    return (0xff, false);
                }
                self.parameter(i, value);
                (self.data[i], i + 1 < usize::from(self.len))
            }
        }
    }

    /// A command: its ID answers it, and what its data will be is known.
    fn command(&mut self, command: u8, buttons: u16) -> (u8, bool) {
        let valid = match command {
            0x42 | 0x43 => true,
            0x40..=0x4f => self.config,
            _ => false,
        };
        if !valid {
            return (0xff, false);
        }
        self.command = command;
        let [low, high] = (!buttons).to_le_bytes();
        let [rx, ry, lx, ly] = self.sticks;
        self.data = match command {
            0x42 => [low, high, rx, ry, lx, ly],
            0x43 if !self.config => [low, high, rx, ry, lx, ly],
            // The mode: whether analog, and the pad's kind.
            0x45 => [0x01, 0x02, u8::from(self.analog), 0x02, 0x01, 0x00],
            0x47 => [0x00, 0x00, 0x02, 0x00, 0x01, 0x00],
            // The motors' map until now.
            0x4d => self.map,
            _ => [0; 6],
        };
        let id = match (self.config, self.analog) {
            (true, _) => 0xf3,
            (false, true) => 0x73,
            (false, false) => 0x41,
        };
        self.len = if id == 0x41 { 2 } else { 6 };
        (id, true)
    }

    /// The game's byte `i` of the data, a parameter of the command.
    fn parameter(&mut self, i: usize, value: u8) {
        match (self.command, i) {
            (0x42, _) => match self.map[i] {
                0x00 => self.small = value & 1 != 0,
                0x01 => self.large = value,
                _ => {}
            },
            (0x43, 0) => self.config = value == 1,
            (0x44, 0) if value <= 1 => self.analog = value == 1,
            // The actuators: what each of the two is, by the parameter.
            (0x46, 0) => {
                self.data[2..].copy_from_slice(match value {
                    0 => &[0x01, 0x02, 0x00, 0x0a],
                    1 => &[0x01, 0x01, 0x01, 0x14],
                    _ => &[0; 4],
                })
            }
            (0x4c, 0) => self.data[3] = [0x04, 0x07].get(usize::from(value)).copied().unwrap_or(0),
            (0x4d, _) => self.map[i] = value,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An exchange after the address byte: what came back for each byte.
    fn exchange(pad: &mut Pad, bytes: &[u8], buttons: u16) -> Vec<(u8, bool)> {
        bytes.iter().enumerate().map(|(i, &b)| pad.transfer(i as u8 + 1, b, buttons)).collect()
    }

    /// Digital until a game asks: a poll is two bytes of buttons, held low.
    #[test]
    fn digital_at_first() {
        let mut pad = Pad::default();
        let replies = exchange(&mut pad, &[0x42, 0x00, 0x00, 0x00], 0x0040);
        assert_eq!(replies, [(0x41, true), (0x5a, true), (0xbf, true), (0xff, false)]);
        // Configuration commands need its mode.
        assert_eq!(pad.transfer(1, 0x44, 0), (0xff, false));
    }

    /// A DualShock game: into configuration mode, analog on, the small motor
    /// mapped to the first byte and the large to the second, out again; its
    /// polls then carry the sticks and drive the motors.
    #[test]
    fn a_dualshock_game() {
        let mut pad = Pad::default();
        pad.sticks = [0x80, 0x80, 0x00, 0xff];
        exchange(&mut pad, &[0x43, 0x00, 0x01, 0x00], 0);
        assert_eq!(exchange(&mut pad, &[0x44, 0x00, 0x01, 0x03, 0, 0, 0, 0], 0)[0], (0xf3, true));
        let map = exchange(&mut pad, &[0x4d, 0x00, 0x00, 0x01, 0xff, 0xff, 0xff, 0xff], 0);
        assert_eq!(map[2..].iter().map(|r| r.0).collect::<Vec<_>>(), [0xff; 6]);
        assert_eq!(exchange(&mut pad, &[0x45, 0x00, 0, 0, 0, 0, 0, 0], 0)[4].0, 0x01);
        exchange(&mut pad, &[0x43, 0x00, 0x00, 0, 0, 0, 0, 0], 0);
        let poll = exchange(&mut pad, &[0x42, 0x00, 0x01, 0xc0, 0, 0, 0, 0], 0x0008);
        assert_eq!(poll.iter().map(|r| r.0).collect::<Vec<_>>(), [0x73, 0x5a, 0xf7, 0xff, 0x80, 0x80, 0x00, 0xff]);
        assert!(!poll[7].1 && poll[6].1);
        assert!(pad.small && pad.large == 0xc0);
        exchange(&mut pad, &[0x42, 0x00, 0x00, 0x00, 0, 0, 0, 0], 0);
        assert!(!pad.small && pad.large == 0);
    }

    /// The actuators' and modes' queries answer as a DualShock's do.
    #[test]
    fn queries() {
        let mut pad = Pad::default();
        exchange(&mut pad, &[0x43, 0x00, 0x01, 0x00], 0);
        let data = |pad: &mut Pad, command: u8, parameter: u8| {
            exchange(pad, &[command, 0x00, parameter, 0, 0, 0, 0, 0], 0)[2..].iter().map(|r| r.0).collect::<Vec<_>>()
        };
        assert_eq!(data(&mut pad, 0x46, 0), [0x00, 0x00, 0x01, 0x02, 0x00, 0x0a]);
        assert_eq!(data(&mut pad, 0x46, 1), [0x00, 0x00, 0x01, 0x01, 0x01, 0x14]);
        assert_eq!(data(&mut pad, 0x47, 0), [0x00, 0x00, 0x02, 0x00, 0x01, 0x00]);
        assert_eq!(data(&mut pad, 0x4c, 1), [0x00, 0x00, 0x00, 0x07, 0x00, 0x00]);
    }
}
