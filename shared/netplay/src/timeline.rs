//! Bounded input history for a deterministic client. All positions name the
//! next frame. Only retired frames may be used as a synchronization anchor.
use crate::WINDOW;
use std::collections::VecDeque;

pub const PREDICTION: u32 = 6;

#[derive(Default)]
struct Entry {
    local: u8,
    actual: Option<[u8; 2]>,
    used: Option<[u8; 2]>,
}

#[derive(Default)]
pub struct Timeline {
    base: u32,
    frame: u32,
    received: u32,
    last: [u8; 2],
    entries: VecDeque<Entry>,
}

impl Timeline {
    pub fn from_frame(frame: u32) -> Self {
        Self {
            base: frame,
            frame,
            received: frame,
            ..Self::default()
        }
    }

    pub fn frame(&self) -> u32 {
        self.frame
    }
    pub fn stable(&self) -> u32 {
        self.base
    }

    pub fn reserve(&mut self, lead: u32, local: u8) -> Option<u32> {
        let n = self.base.checked_add(self.entries.len() as u32)?;
        // The relay's queue is bounded independently of speculative progress.
        if n == u32::MAX
            || n - self.frame >= lead.min(WINDOW as u32)
            || n - self.received >= WINDOW as u32
        {
            return None;
        }
        self.entries.push_back(Entry {
            local,
            ..Entry::default()
        });
        Some(n)
    }

    pub fn confirm(&mut self, frame: u32, pads: [u8; 2]) -> bool {
        if frame != self.received {
            return false;
        }
        let Some(entry) = self.entries.get_mut((frame - self.base) as usize) else {
            return false;
        };
        entry.actual = Some(pads);
        self.received += 1;
        true
    }

    /// Later predictions must also be replayed when an earlier input changes.
    pub fn rewind(&mut self, stalled: bool) -> Option<u32> {
        let index = self
            .entries
            .iter()
            .take((self.received.min(self.frame) - self.base) as usize)
            .position(|e| e.actual != e.used);
        if let Some(index) = index {
            self.frame = self.base + index as u32;
        } else if stalled && self.frame > self.received {
            // Preserve submitted input, but display the last agreed state while waiting.
            self.frame = self.received;
        } else {
            return None;
        }
        Some(self.frame)
    }

    pub fn advance(&mut self, seat: usize, predict: bool) -> Option<[u8; 2]> {
        if seat > 1 {
            return None;
        }
        let i = (self.frame - self.base) as usize;
        let previous = if i == 0 {
            self.last
        } else {
            self.entries[i - 1].used?
        };
        let entry = self.entries.get_mut(i)?;
        let pads = match entry.actual {
            Some(pads) => pads,
            None if predict && self.frame - self.received < PREDICTION => {
                let mut pads = previous;
                pads[seat] = entry.local;
                pads
            }
            None => return None,
        };
        entry.used = Some(pads);
        self.frame += 1;
        Some(pads)
    }

    pub fn retire(&mut self) -> u32 {
        while self.base < self.received.min(self.frame) {
            let entry = self.entries.front().unwrap();
            if entry.used != entry.actual {
                break;
            }
            self.last = entry.actual.unwrap();
            self.entries.pop_front();
            self.base += 1;
        }
        self.base
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use super::Timeline;
    use std::cell::RefCell;
    thread_local! { static CLIENT: RefCell<Timeline> = RefCell::new(Timeline::default()); }

    #[unsafe(no_mangle)]
    pub extern "C" fn reset(frame: u32) {
        CLIENT.with_borrow_mut(|c| *c = Timeline::from_frame(frame));
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn check_interval() -> u32 {
        crate::CHECK_INTERVAL
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn frame() -> u32 {
        CLIENT.with_borrow(Timeline::frame)
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn stable() -> u32 {
        CLIENT.with_borrow(Timeline::stable)
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn reserve(lead: u32, pad: u8) -> f64 {
        CLIENT.with_borrow_mut(|c| c.reserve(lead, pad).map_or(-1.0, f64::from))
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn confirm(frame: u32, p1: u8, p2: u8) -> bool {
        CLIENT.with_borrow_mut(|c| c.confirm(frame, [p1, p2]))
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn rewind(stalled: bool) -> f64 {
        CLIENT.with_borrow_mut(|c| c.rewind(stalled).map_or(-1.0, f64::from))
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn advance(seat: usize, predict: bool) -> i32 {
        CLIENT.with_borrow_mut(|c| {
            c.advance(seat, predict)
                .map_or(-1, |pads| u16::from_le_bytes(pads).into())
        })
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn retire() -> u32 {
        CLIENT.with_borrow_mut(Timeline::retire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_changes_the_entire_predicted_suffix_and_preserves_local_inputs() {
        let mut t = Timeline::from_frame(299);
        for pad in [2, 4, 8] {
            t.reserve(3, pad).unwrap();
            assert_eq!(t.advance(1, true), Some([0, pad]));
        }
        assert!(!t.confirm(300, [1, 4]));
        assert!(t.confirm(299, [1, 2]));
        assert_eq!(t.retire(), 299);
        assert_eq!(t.rewind(false), Some(299));
        for pad in [2, 4, 8] {
            assert_eq!(t.advance(1, true), Some([1, pad]));
        }
        assert_eq!(t.retire(), 300);
        assert!(t.confirm(300, [1, 4]));
        assert_eq!(t.rewind(false), None);
        assert_eq!(t.retire(), 301);
    }

    #[test]
    fn a_stall_discards_unconfirmed_execution_but_keeps_the_submitted_inputs() {
        let mut t = Timeline::default();
        t.reserve(2, 1);
        t.reserve(2, 2);
        t.advance(0, true);
        t.advance(0, true);
        assert!(t.confirm(0, [1, 0]));
        assert_eq!(t.retire(), 1);
        assert_eq!(t.rewind(true), Some(1));
        assert_eq!(t.advance(0, false), None);
        assert!(t.confirm(1, [2, 8]));
        assert_eq!(t.advance(0, false), Some([2, 8]));
        assert_eq!(t.retire(), 2);
    }

    #[test]
    fn predictions_and_wire_queue_both_have_hard_limits() {
        let mut t = Timeline::default();
        while t.reserve(WINDOW as u32, 3).is_some() {}
        assert_eq!(t.advance(0, false), None);
        for _ in 0..PREDICTION {
            assert_eq!(t.advance(0, true), Some([3, 0]));
        }
        assert_eq!(t.advance(0, true), None);
        assert_eq!(t.reserve(WINDOW as u32, 3), None);
        assert!(t.confirm(0, [3, 0]));
        assert_eq!(t.retire(), 1);
        assert_eq!(t.advance(0, true), Some([3, 0]));
    }
}
