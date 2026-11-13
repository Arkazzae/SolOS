//! The real-time clock some cartridges carry (Seiko's S-3511A: Pokémon
//! Ruby, Sapphire and Emerald, Boktai), reached through the cartridge's
//! general-purpose port at 0800_00C4: three pins, a clock (SCK), a data
//! line (SIO) and a select (CS), which the game drives bit by bit.
//!
//! A transfer starts with CS going high, and each bit moves as SCK rises,
//! lowest bit first. The first byte is a command: 0110 in its low four
//! bits, then which register (reset, status, date and time, or time), and
//! in the top bit whether the game reads it. The date and time are in
//! BCD. The clock shows the time the host gives it, moved on with the
//! emulated time.

use savestate::state;

/// Cycles in a second.
const CYCLE_HZ: u64 = 1 << 24;

// The registers, by the command's number, and the bytes each has.
const RESET: u8 = 0;
const DATE_TIME: u8 = 2;
const STATUS: u8 = 4;
const TIME: u8 = 6;
const LENGTHS: [u8; 8] = [0, 0, 7, 0, 1, 0, 3, 0];

// The pins, as bits of the port.
const SCK: u8 = 1;
const SIO: u8 = 2;
const CS: u8 = 4;

#[derive(Clone, Default)]
pub struct Rtc {
    /// The port: the pins' levels, which pins the game drives (the
    /// others the clock does), and whether the game can read the port.
    pins: u8,
    direction: u8,
    readable: bool,
    /// The transfer: the command (once its byte is in), the bits of the
    /// byte under way, the bytes of the register left.
    command: Option<u8>,
    byte: u8,
    bits: u8,
    left: u8,
    /// The register being read, its bytes as taken when the command came.
    output: [u8; 7],
    /// The status register: bit 6 is the 24-hour clock.
    status: u8,
    /// The time the clock shows, as seconds since 1970, at the cycle
    /// `since`.
    seconds: u64,
    since: u64,
}

state!(Rtc { pins, direction, readable, command, byte, bits, left, output, status, seconds, since });

fn bcd(n: u64) -> u8 {
    (n / 10 % 10 * 16 + n % 10) as u8
}

/// The date of a day counted from 1970-01-01: year, month (1-12), day.
fn civil(days: i64) -> (i64, u64, u64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u64;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 } as u64;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

impl Rtc {
    pub fn new() -> Rtc {
        Rtc { status: 0x40, ..Rtc::default() }
    }

    /// Sets the time the clock shows now.
    pub fn set(&mut self, seconds: u64, now: u64) {
        self.seconds = seconds;
        self.since = now;
    }

    pub fn pass_time(&mut self, seconds: u64) {
        self.seconds += seconds;
    }

    /// What the game reads at 0800_00C4-C9, if the port is readable.
    pub fn read(&self, addr: u32) -> Option<u16> {
        if !self.readable {
            return None;
        }
        Some(match addr & 0xe {
            0x4 => u16::from(self.pins),
            0x6 => u16::from(self.direction),
            _ => u16::from(self.readable),
        })
    }

    pub fn write(&mut self, addr: u32, value: u16, now: u64) {
        match addr & 0xe {
            0x4 => {
                let before = self.pins;
                self.pins = self.pins & !self.direction | value as u8 & self.direction & 7;
                self.clock_pins(before, now);
            }
            0x6 => self.direction = value as u8 & 7,
            _ => self.readable = value & 1 != 0,
        }
    }

    /// The pins changed: with CS low the transfer is over; as SCK rises
    /// a bit goes in, or out on SIO.
    fn clock_pins(&mut self, before: u8, now: u64) {
        if self.pins & CS == 0 {
            self.command = None;
            self.bits = 0;
            self.byte = 0;
            return;
        }
        if before & SCK != 0 || self.pins & SCK == 0 {
            return;
        }
        let reading = self.command.is_some_and(|c| c & 0x80 != 0);
        if reading {
            // The clock drives SIO with the next bit of its register.
            let index = usize::from(self.output_length() - self.left);
            let bit = self.output[index.min(6)] >> self.bits & 1;
            self.pins = self.pins & !SIO | bit << 1;
        } else {
            self.byte |= (self.pins & SIO) >> 1 << self.bits;
        }
        self.bits += 1;
        if self.bits < 8 {
            return;
        }
        let byte = self.byte;
        self.bits = 0;
        self.byte = 0;
        match self.command {
            None => self.start(byte, now),
            Some(command) => {
                if command & 0x80 == 0 && command >> 4 & 7 == STATUS {
                    self.status = byte & 0x6a;
                }
                self.left = self.left.saturating_sub(1);
                if self.left == 0 {
                    self.command = None;
                }
            }
        }
    }

    /// A command byte: 0110 then the register and the direction.
    fn start(&mut self, byte: u8, now: u64) {
        if byte & 15 != 6 {
            return;
        }
        let register = byte >> 4 & 7;
        self.left = LENGTHS[usize::from(register)];
        match register {
            RESET => self.status = 0,
            STATUS => self.output[0] = self.status,
            DATE_TIME | TIME => {
                let time = self.time(now);
                if register == TIME {
                    self.output[..3].copy_from_slice(&time[4..]);
                } else {
                    self.output = time;
                }
            }
            _ => {}
        }
        self.command = (self.left > 0).then_some(byte);
    }

    fn output_length(&self) -> u8 {
        self.command.map_or(0, |c| LENGTHS[usize::from(c >> 4 & 7)])
    }

    /// The date and time now, as the clock gives them: year (00-99),
    /// month, day, day of the week (0 for Sunday), hour, minute, second,
    /// in BCD; in 12-hour mode bit 7 of the hour is the afternoon.
    fn time(&self, now: u64) -> [u8; 7] {
        let seconds = self.seconds + now.saturating_sub(self.since) / CYCLE_HZ;
        let days = (seconds / 86_400) as i64;
        let (year, month, day) = civil(days);
        let in_day = seconds % 86_400;
        let hour = in_day / 3600;
        let hour = if self.status & 0x40 != 0 { bcd(hour) } else { bcd(hour % 12) | u8::from(hour >= 12) << 7 };
        // 1970-01-01 was a Thursday.
        let weekday = ((days + 4).rem_euclid(7)) as u8;
        [bcd(year as u64 % 100), bcd(month), bcd(day), weekday, hour, bcd(in_day / 60 % 60), bcd(in_day % 60)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to the clock as Nintendo's library does: SIO set while SCK
    /// is low, taken as it rises.
    fn send(rtc: &mut Rtc, byte: u8, now: u64) {
        for i in 0..8 {
            let bit = u16::from(byte >> i & 1) << 1;
            rtc.write(0xc4, u16::from(CS) | bit, now);
            rtc.write(0xc4, u16::from(CS | SCK) | bit, now);
        }
    }

    fn receive(rtc: &mut Rtc, now: u64) -> u8 {
        (0..8).fold(0, |byte, i| {
            rtc.write(0xc4, u16::from(CS), now);
            rtc.write(0xc4, u16::from(CS | SCK), now);
            byte | (rtc.read(0xc4).unwrap() as u8 >> 1 & 1) << i
        })
    }

    /// Reads a register: the command, then its bytes with SIO the clock's.
    fn read(rtc: &mut Rtc, register: u8, now: u64) -> Vec<u8> {
        rtc.write(0xc8, 1, now);
        rtc.write(0xc6, 7, now);
        rtc.write(0xc4, u16::from(SCK), now);
        rtc.write(0xc4, u16::from(CS | SCK), now);
        send(rtc, 6 | register << 4 | 0x80, now);
        rtc.write(0xc6, 5, now);
        let bytes = (0..LENGTHS[usize::from(register)]).map(|_| receive(rtc, now)).collect();
        rtc.write(0xc4, 0, now);
        bytes
    }

    #[test]
    fn date_and_time() {
        let mut rtc = Rtc::new();
        // 2026-10-29, a Thursday, 15:20:07.
        rtc.set(1_793_287_207, 0);
        assert_eq!(read(&mut rtc, DATE_TIME, 0), [0x26, 0x10, 0x29, 4, 0x15, 0x20, 0x07]);
        // A minute and a half of emulated time on.
        assert_eq!(read(&mut rtc, TIME, 90 * CYCLE_HZ), [0x15, 0x21, 0x37]);
        assert_eq!(read(&mut rtc, STATUS, 0), [0x40]);
        // In 12-hour mode the afternoon is bit 7 of the hour.
        rtc.write(0xc6, 7, 0);
        rtc.write(0xc4, u16::from(SCK), 0);
        rtc.write(0xc4, u16::from(CS | SCK), 0);
        send(&mut rtc, 6 | STATUS << 4, 0);
        send(&mut rtc, 0, 0);
        rtc.write(0xc4, 0, 0);
        assert_eq!(read(&mut rtc, TIME, 0), [0x83, 0x20, 0x07]);
        // A leap day, a Tuesday.
        rtc.set(951_868_799, 0);
        assert_eq!(read(&mut rtc, DATE_TIME, 0)[..4], [0x00, 0x02, 0x29, 2]);
    }
}
