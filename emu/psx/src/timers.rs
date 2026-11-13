//! The three root counters: 16-bit counters on the system clock, or timer 0
//! on the GPU's dot clock, timer 1 on scanlines, timer 2 on an eighth of
//! the system clock. Each counts to its target or to FFFF, where it can
//! start over and raise its interrupt. They are brought up to date when
//! read or written and at every scanline, which is as often as anything
//! here needs their interrupts.

use crate::Psx;
use savestate::state;

#[derive(Clone, Copy, Default)]
pub struct Timer {
    counter: u32,
    mode: u32,
    target: u32,
    /// The cycle counted up to, and the clocks of a divided source over.
    since: u64,
    rest: u64,
    /// The one-shot interrupt has gone off since the mode was written.
    fired: bool,
}

state!(Timer { counter, mode, target, since, rest, fired });

#[derive(Clone, Default)]
pub struct Timers {
    timers: [Timer; 3],
    /// Interrupts the timers raised, for the controller.
    pub raised: u32,
}

state!(Timers { timers, raised });

impl Timers {
    /// The divider of the system clock a timer counts by, or 0 when it
    /// counts scanlines (or is stopped).
    fn divider(&self, n: usize) -> u64 {
        let t = &self.timers[n];
        let source = t.mode >> 8 & 3;
        match n {
            // The dot clock, as for 320 pixels across: 53.69 MHz / 8.
            0 if source & 1 != 0 => 0,
            1 if source & 1 != 0 => 0,
            // Timer 2 stops in sync modes 0 and 3.
            2 if t.mode & 1 != 0 && matches!(t.mode >> 1 & 3, 0 | 3) => 0,
            2 if source >= 2 => 8,
            _ => 1,
        }
    }

    fn sync(&mut self, n: usize, now: u64) {
        let divider = self.divider(n);
        let t = &mut self.timers[n];
        let elapsed = now.saturating_sub(t.since);
        t.since = now;
        let ticks = match divider {
            0 if n == 0 && t.mode >> 8 & 1 != 0 => {
                // The dot clock: 11/56 of the system clock.
                let total = t.rest + elapsed * 11;
                t.rest = total % 56;
                total / 56
            }
            0 => 0,
            d => {
                let total = t.rest + elapsed;
                t.rest = total % d;
                total / d
            }
        };
        if ticks > 0 {
            self.advance(n, ticks);
        }
    }

    /// Counts ticks on, through the target and FFFF as many times as they
    /// pass.
    fn advance(&mut self, n: usize, mut ticks: u64) {
        let t = &mut self.timers[n];
        let mut irq = false;
        while ticks > 0 {
            let at_target = t.mode & 8 != 0;
            let end = if at_target && t.target > 0 { t.target } else { 0xffff };
            let to_target = if t.counter < t.target { u64::from(t.target - t.counter) } else { u64::MAX };
            let to_end = u64::from(end.saturating_sub(t.counter)) + 1;
            if ticks < to_target.min(to_end) {
                t.counter += ticks as u32;
                break;
            }
            if to_target <= to_end && to_target <= ticks {
                ticks -= to_target;
                t.counter = t.target;
                t.mode |= 1 << 11;
                irq |= t.mode & 1 << 4 != 0;
                if !at_target {
                    continue;
                }
                // Reset at the target, a tick after it.
                if ticks == 0 {
                    break;
                }
                ticks -= 1;
                t.counter = 0;
                continue;
            }
            ticks -= to_end;
            t.counter = 0;
            if end == 0xffff {
                t.mode |= 1 << 12;
                irq |= t.mode & 1 << 5 != 0;
            }
        }
        if irq && (t.mode & 1 << 6 != 0 || !t.fired) {
            t.fired = true;
            self.raised |= 1 << (4 + n);
        }
    }

    pub fn read(&mut self, addr: u32, now: u64) -> u32 {
        let n = (addr >> 4 & 3) as usize;
        if n == 3 {
            return 0;
        }
        self.sync(n, now);
        let t = &mut self.timers[n];
        match addr & 0xf {
            0x0 => t.counter,
            0x4 => {
                // Reading the mode clears the reached flags.
                let mode = t.mode;
                t.mode &= !(3 << 11);
                mode
            }
            0x8 => t.target,
            _ => 0,
        }
    }

    pub fn write(&mut self, addr: u32, value: u32, now: u64) {
        let n = (addr >> 4 & 3) as usize;
        if n == 3 {
            return;
        }
        self.sync(n, now);
        let t = &mut self.timers[n];
        match addr & 0xf {
            0x0 => t.counter = value & 0xffff,
            0x4 => {
                t.mode = value & 0x3ff | 1 << 10;
                t.counter = 0;
                t.fired = false;
                t.rest = 0;
            }
            0x8 => t.target = value & 0xffff,
            _ => {}
        }
    }

    /// A scanline's horizontal blank: timer 1 on scanlines counts it, and
    /// the others are brought up to date.
    pub fn hblank(&mut self, now: u64) {
        for n in 0..3 {
            self.sync(n, now);
        }
        if self.timers[1].mode >> 8 & 1 != 0 {
            self.advance(1, 1);
        }
    }
}

impl Psx {
    /// Passes the timers' interrupts to the controller.
    pub(crate) fn timers_irq(&mut self) {
        let raised = std::mem::take(&mut self.timers.raised);
        if raised != 0 {
            self.request(raised);
        }
    }
}
