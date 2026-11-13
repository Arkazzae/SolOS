//! The MBC3's real-time clock: seconds, minutes, hours and a 9-bit day
//! counter, run from the cartridge's own crystal. It moves with the
//! emulated time and is brought up to date whenever the game touches it;
//! the host moves it on by the time the console spent off.

use savestate::state;

/// The clock's crystal ticks once a second; time is counted here in the
/// Game Boy's clocks (4 MiHz).
const SECOND: u64 = 1 << 22;

// The registers, as the game selects them at A000 (from 08).
const SECONDS: usize = 0;
const MINUTES: usize = 1;
const HOURS: usize = 2;
const DAYS: usize = 3;
const CONTROL: usize = 4;

/// Bits of each register that exist: the control register has the days'
/// ninth bit, the halt bit (6) and the days' overflow (7).
const MASKS: [u8; 5] = [0x3f, 0x3f, 0x1f, 0xff, 0xc1];
const HALT: u8 = 0x40;
const OVERFLOW: u8 = 0x80;

#[derive(Clone, Copy, Default)]
pub struct Rtc {
    registers: [u8; 5],
    /// What the game reads: the registers as last latched.
    latched: [u8; 5],
    /// The last value written to the latch register; 0 then 1 latches.
    latch: u8,
    /// Clocks into the current second, and the time it was brought up to.
    sub: u64,
    now: u64,
}

state!(Rtc { registers, latched, latch, sub, now });

impl Rtc {
    /// Brings the clock up to the given time (in clocks).
    fn sync(&mut self, now: u64) {
        let elapsed = now.saturating_sub(self.now);
        self.now = now;
        if self.registers[CONTROL] & HALT != 0 {
            return;
        }
        self.sub += elapsed;
        let seconds = self.sub / SECOND;
        self.sub %= SECOND;
        self.pass(seconds);
    }

    /// Moves the clock on by some seconds, unless it is halted.
    pub fn pass(&mut self, mut seconds: u64) {
        if self.registers[CONTROL] & HALT != 0 {
            return;
        }
        // Out-of-range values (a game can write 28:63:60) tick one by one
        // until they wrap; then the rest is arithmetic.
        while seconds > 0 && !self.in_range() {
            self.tick();
            seconds -= 1;
        }
        if seconds == 0 {
            return;
        }
        let [s, m, h] = [SECONDS, MINUTES, HOURS].map(|i| u64::from(self.registers[i]));
        let time = h * 3600 + m * 60 + s + seconds;
        let days = self.days() + time / 86400;
        let time = time % 86400;
        self.registers[HOURS] = (time / 3600) as u8;
        self.registers[MINUTES] = (time / 60 % 60) as u8;
        self.registers[SECONDS] = (time % 60) as u8;
        self.set_days(days);
    }

    fn in_range(&self) -> bool {
        self.registers[SECONDS] < 60 && self.registers[MINUTES] < 60 && self.registers[HOURS] < 24
    }

    fn days(&self) -> u64 {
        u64::from(self.registers[DAYS]) | u64::from(self.registers[CONTROL] & 1) << 8
    }

    /// Past 511 days, the counter wraps and sets the overflow bit, which
    /// stays set until the game clears it.
    fn set_days(&mut self, days: u64) {
        if days > 511 {
            self.registers[CONTROL] |= OVERFLOW;
        }
        let days = days % 512;
        self.registers[DAYS] = days as u8;
        self.registers[CONTROL] = self.registers[CONTROL] & !1 | (days >> 8) as u8;
    }

    /// One second. Each counter carries only from its last proper value
    /// (59, 59, 23); past the top of its bits it wraps to 0 without one.
    fn tick(&mut self) {
        for (i, last) in [(SECONDS, 59), (MINUTES, 59), (HOURS, 23)] {
            if self.registers[i] != last {
                self.registers[i] = (self.registers[i] + 1) & MASKS[i];
                return;
            }
            self.registers[i] = 0;
        }
        self.set_days(self.days() + 1);
    }

    /// A write to the latch register: 0 then 1 copies the time into what
    /// the game reads.
    pub fn write_latch(&mut self, value: u8, now: u64) {
        if self.latch == 0 && value == 1 {
            self.sync(now);
            self.latched = self.registers;
        }
        self.latch = value;
    }

    pub fn read(&self, register: u8) -> u8 {
        self.latched[usize::from(register - 8)]
    }

    /// Writing the seconds starts the second over.
    pub fn write(&mut self, register: u8, value: u8, now: u64) {
        self.sync(now);
        let i = usize::from(register - 8);
        self.registers[i] = value & MASKS[i];
        if i == SECONDS {
            self.sub = 0;
        }
    }

    /// The registers and the latched ones as BGB and VBA-M keep them after
    /// the RAM in a save file: ten little-endian 32-bit words and the UNIX
    /// time of the save.
    pub fn save(&self, unix_time: u64, out: &mut Vec<u8>) {
        for value in self.registers.iter().chain(&self.latched) {
            out.extend_from_slice(&u32::from(*value).to_le_bytes());
        }
        out.extend_from_slice(&unix_time.to_le_bytes());
    }

    /// The same back (also with a 32-bit time, as some emulators write it),
    /// moved on by the time since the save.
    pub fn load(&mut self, data: &[u8], unix_time: u64) {
        let word = |i: usize| data[i * 4..i * 4 + 4].try_into().map(u32::from_le_bytes).unwrap_or(0);
        for i in 0..5 {
            self.registers[i] = word(i) as u8 & MASKS[i];
            self.latched[i] = word(i + 5) as u8 & MASKS[i];
        }
        let saved = match data.len() {
            48 => u64::from_le_bytes(data[40..48].try_into().unwrap()),
            _ => u64::from(word(10)),
        };
        self.pass(unix_time.saturating_sub(saved));
    }
}
