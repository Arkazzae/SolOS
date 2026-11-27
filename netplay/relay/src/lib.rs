//! The two-seat room state machine used by both the WebSocket service and Cloudflare.
use sol_netplay::{CHECK_INTERVAL, Command, Event, Inputs};
pub type Failure = &'static str;
pub const MAX_MESSAGE: usize = 512 * 1024;

pub fn hex(s: &str, length: usize) -> bool {
    s.len() == length && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(PartialEq)]
enum Phase {
    Waiting,
    Snapshot,
    Loading,
    Playing,
}

pub struct Game {
    game: String,
    ready: [bool; 2],
    phase: Phase,
    epoch: u32,
    inputs: Inputs,
    checks: [Option<(u32, String)>; 2],
}

impl Game {
    pub fn new(game: String) -> Self {
        Self {
            game,
            ready: [false; 2],
            phase: Phase::Waiting,
            epoch: 0,
            inputs: Inputs::default(),
            checks: [None, None],
        }
    }

    fn sync(&mut self) -> Event {
        self.epoch += 1;
        self.phase = Phase::Snapshot;
        self.checks = [None, None];
        Event::Sync { epoch: self.epoch }
    }

    pub fn apply(&mut self, seat: usize, command: Command) -> Result<Option<Event>, Failure> {
        if seat > 1 {
            return Err("seat");
        }
        match command {
            Command::Ping => return Ok(Some(Event::Pong { seat })),
            Command::Performance { epoch, slow }
                if epoch == self.epoch && self.phase == Phase::Playing =>
            {
                return Ok(Some(Event::Performance { epoch, seat, slow }));
            }
            Command::Pause => {
                self.ready[seat] = false;
                self.phase = Phase::Waiting;
                return Ok(Some(Event::Wait));
            }
            Command::Ready { game } => {
                if game != self.game {
                    return Err("game");
                }
                self.ready[seat] = true;
                if self.phase == Phase::Waiting && self.ready == [true, true] {
                    return Ok(Some(self.sync()));
                }
            }
            Command::Snapshot { epoch, frame, data }
                if epoch == self.epoch && self.phase == Phase::Snapshot =>
            {
                if seat != 0
                    || frame > self.inputs.frame
                    || data.is_empty()
                    || data.len() > MAX_MESSAGE - 128
                {
                    return Err("snapshot");
                }
                self.inputs = Inputs::from_frame(frame);
                self.phase = Phase::Loading;
                return Ok(Some(Event::Snapshot { epoch, frame, data }));
            }
            Command::Loaded { epoch } if epoch == self.epoch && self.phase == Phase::Loading => {
                if seat != 1 {
                    return Err("seat");
                }
                self.phase = Phase::Playing;
                return Ok(Some(Event::Go {
                    epoch,
                    frame: self.inputs.frame,
                }));
            }
            Command::Input { epoch, frame, pad }
                if epoch == self.epoch && self.phase == Phase::Playing =>
            {
                let next = self.inputs.frame;
                if let Some(pads) = self.inputs.push(seat, frame, pad)? {
                    return Ok(Some(Event::Frame { frame: next, pads }));
                }
            }
            Command::Check { epoch, frame, hash }
                if epoch == self.epoch && self.phase == Phase::Playing =>
            {
                if !frame.is_multiple_of(CHECK_INTERVAL)
                    || frame > self.inputs.frame
                    || !hex(&hash, 64)
                {
                    return Err("check");
                }
                self.checks[seat] = Some((frame, hash));
                if let [Some((a, x)), Some((b, y))] = &self.checks
                    && a == b
                {
                    if x != y {
                        return Ok(Some(self.sync()));
                    } else {
                        self.checks = [None, None];
                    }
                }
            }
            Command::Leave => return Err("ended"),
            // Packets from a previous connection or epoch can arrive during recovery.
            _ => {}
        }
        Ok(None)
    }
}

#[cfg(all(target_arch = "wasm32", feature = "edge"))]
mod edge;
