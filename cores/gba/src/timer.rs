//! The four timers: 16-bit counters running at the CPU's clock divided by
//! 1, 64, 256 or 1024, or (count-up) counting the overflows of the timer
//! before. An overflow loads the reload value, and can request an
//! interrupt and feed the sound FIFOs.
//!
//! A running timer is not stepped: its count is worked out from when it
//! last had a known value. Its overflows are events only when something
//! sees them happen.

use crate::Gba;
use crate::bus::set_byte;
use savestate::state;

/// The prescalers, as shifts of the cycle count.
const SHIFTS: [u32; 4] = [0, 6, 8, 10];

#[derive(Clone, Copy, Default)]
pub struct Timer {
    reload: u16,
    control: u16,
    /// The count, as it was at `since` (a whole number of ticks back).
    counter: u16,
    since: u64,
}

state!(Timer { reload, control, counter, since });

impl Timer {
    fn running(&self) -> bool {
        self.control & 0x80 != 0
    }

    fn shift(&self) -> u32 {
        SHIFTS[usize::from(self.control & 3)]
    }
}

impl Gba {
    /// Whether a timer counts overflows (timer 0 cannot).
    fn counts_up(&self, n: usize) -> bool {
        n > 0 && self.timers[n].control & 4 != 0
    }

    /// Whether a timer runs on the clock.
    fn clocked(&self, n: usize) -> bool {
        self.timers[n].running() && !self.counts_up(n)
    }

    /// Brings a clocked timer's count up to now, keeping its prescaler's
    /// phase.
    fn settle(&mut self, n: usize) {
        if !self.clocked(n) {
            return;
        }
        let now = self.cycles;
        let t = &mut self.timers[n];
        let ticks = now.saturating_sub(t.since) >> t.shift();
        t.since += ticks << t.shift();
        let count = u64::from(t.counter) + ticks;
        t.counter = if count < 0x10000 {
            count as u16
        } else {
            (u64::from(t.reload) + (count - 0x10000) % (0x10000 - u64::from(t.reload))) as u16
        };
    }

    /// Whether anything sees a timer's overflows: its interrupt, a timer
    /// counting them, or the sound.
    fn overflows_matter(&self, n: usize) -> bool {
        self.timers[n].control & 0x40 != 0
            || n < 3 && self.timers[n + 1].running() && self.counts_up(n + 1)
            || n < 2 && self.apu.uses_timer(n)
    }

    /// Schedules a timer's next overflow, if it is one anything sees.
    pub(crate) fn reschedule_timer(&mut self, n: usize) {
        let at = if self.clocked(n) && self.overflows_matter(n) {
            self.settle(n);
            let t = &self.timers[n];
            t.since + ((0x10000 - u64::from(t.counter)) << t.shift())
        } else {
            u64::MAX
        };
        self.schedule(crate::TIMER0 + n, at);
    }

    pub(crate) fn timer_overflow(&mut self, n: usize, at: u64) {
        let t = &mut self.timers[n];
        t.counter = t.reload;
        t.since = at;
        self.overflowed(n);
        self.reschedule_timer(n);
    }

    fn overflowed(&mut self, n: usize) {
        if self.timers[n].control & 0x40 != 0 {
            self.request(crate::TIMER_IRQ << n);
        }
        if n < 2 {
            self.apu_timer_overflow(n);
        }
        if n < 3 && self.timers[n + 1].running() && self.counts_up(n + 1) {
            let next = &mut self.timers[n + 1];
            next.counter = next.counter.wrapping_add(1);
            if next.counter == 0 {
                next.counter = next.reload;
                self.overflowed(n + 1);
            }
        }
    }

    pub(crate) fn timer_read_register(&mut self, reg: u32) -> u16 {
        let n = (reg as usize - 0x100) / 4;
        if reg & 2 != 0 {
            return self.timers[n].control;
        }
        // A count changes with time, not at events.
        self.idle_loop.touched = true;
        self.settle(n);
        self.timers[n].counter
    }

    pub(crate) fn timer_write_register(&mut self, reg: u32, value: u8) {
        let n = (reg as usize - 0x100) / 4;
        match reg & 3 {
            0 | 1 => set_byte(&mut self.timers[n].reload, reg, value),
            2 => {
                self.settle(n);
                let t = &mut self.timers[n];
                let starting = !t.running() && value & 0x80 != 0;
                t.control = u16::from(value) & 0xc7;
                if starting {
                    t.counter = t.reload;
                    t.since = self.cycles;
                }
                // The timer before may now have one counting its overflows.
                for m in n.saturating_sub(1)..=n {
                    self.reschedule_timer(m);
                }
            }
            _ => {}
        }
    }
}
