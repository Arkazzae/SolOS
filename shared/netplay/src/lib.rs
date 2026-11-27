//! The room protocol for browser and native clients. Pad bits in v1 are NES
//! A, B, Select, Start, Up, Down, Left, Right, in ascending bit order.
mod timeline;
pub use timeline::Timeline;

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const VERSION: u8 = 2;
pub const WINDOW: usize = 12;
pub const CHECK_INTERVAL: u32 = 300;

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Hello {
    Create {
        version: u8,
        core: String,
        game: String,
        name: String,
    },
    Join {
        version: u8,
        core: String,
        code: String,
    },
    Resume {
        version: u8,
        code: String,
        token: String,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Command {
    Ping,
    Performance {
        epoch: u32,
        slow: bool,
    },
    Ready {
        game: String,
    },
    Pause,
    Leave,
    Snapshot {
        epoch: u32,
        frame: u32,
        data: String,
    },
    Loaded {
        epoch: u32,
    },
    Input {
        epoch: u32,
        frame: u32,
        pad: u8,
    },
    Check {
        epoch: u32,
        frame: u32,
        hash: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Event {
    Pong {
        seat: usize,
    },
    Performance {
        epoch: u32,
        seat: usize,
        slow: bool,
    },
    Seat {
        code: String,
        seat: usize,
        token: String,
        game: String,
        name: String,
    },
    Wait,
    Sync {
        epoch: u32,
    },
    Snapshot {
        epoch: u32,
        frame: u32,
        data: String,
    },
    Go {
        epoch: u32,
        frame: u32,
    },
    Frame {
        frame: u32,
        pads: [u8; 2],
    },
    Error {
        reason: String,
    },
}

/// Only complete frames can leave this queue. Each seat submits its inputs in
/// order over a reliable stream, within a bounded lead of the other seat.
#[derive(Default)]
pub struct Inputs {
    pub frame: u32,
    pending: [VecDeque<u8>; 2],
}

impl Inputs {
    pub fn from_frame(frame: u32) -> Self {
        Self {
            frame,
            ..Self::default()
        }
    }

    pub fn push(
        &mut self,
        seat: usize,
        frame: u32,
        pad: u8,
    ) -> Result<Option<[u8; 2]>, &'static str> {
        let queue = self.pending.get_mut(seat).ok_or("seat")?;
        if queue.len() >= WINDOW || self.frame.checked_add(queue.len() as u32) != Some(frame) {
            return Err("input");
        }
        queue.push_back(pad);
        if self.pending.iter().any(VecDeque::is_empty) {
            return Ok(None);
        }
        self.frame = self.frame.checked_add(1).ok_or("input")?;
        Ok(Some(
            self.pending
                .each_mut()
                .map(|pads| pads.pop_front().unwrap()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seats_never_skip_or_overwrite_each_other() {
        let mut inputs = Inputs::from_frame(81);
        assert!(inputs.push(1, 82, 4).is_err());
        assert_eq!(inputs.push(1, 81, 4), Ok(None));
        assert!(inputs.push(1, 81, 8).is_err());
        assert_eq!(inputs.push(1, 82, 8), Ok(None));
        assert_eq!(inputs.push(0, 81, 1), Ok(Some([1, 4])));
        assert_eq!(inputs.push(0, 82, 2), Ok(Some([2, 8])));
    }
}
