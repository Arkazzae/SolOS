//! Cloudflare keeps seat metadata across hibernation. A waking room starts a new
//! epoch and obtains the current emulator state and frame from its host.
use super::{Game, MAX_MESSAGE, hex};
use sol_netplay::{Command, Event, Hello, Inputs, VERSION};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct RoomEngine(Game);

#[wasm_bindgen]
impl RoomEngine {
    #[wasm_bindgen(constructor)]
    pub fn new(game: String, epoch: u32, ready: u8) -> Self {
        let mut state = Game::new(game);
        state.ready = [ready & 1 != 0, ready & 2 != 0];
        state.epoch = epoch;
        if epoch > 0 {
            state.inputs = Inputs::from_frame(u32::MAX);
        }
        Self(state)
    }

    pub fn ready(&self) -> u8 {
        u8::from(self.0.ready[0]) | (u8::from(self.0.ready[1]) << 1)
    }

    pub fn epoch(&self) -> u32 {
        self.0.epoch
    }

    pub fn wake(&mut self) -> Option<String> {
        (self.0.ready == [true; 2]).then(|| encode(&self.0.sync()))
    }

    pub fn command(&mut self, seat: usize, message: &str) -> Result<Option<String>, JsValue> {
        if message.len() > MAX_MESSAGE {
            return Err("protocol".into());
        }
        let cmd: Command = serde_json::from_str(message).map_err(|_| "protocol")?;
        Ok(self.0.apply(seat, cmd)?.as_ref().map(encode))
    }
}

fn encode(event: &Event) -> String {
    serde_json::to_string(event).unwrap()
}

/// Validate the same v1 wire format as the native handshake before assigning a seat.
#[wasm_bindgen]
pub fn hello(message: &str) -> Result<String, JsValue> {
    if message.len() > 2048 {
        return Err("protocol".into());
    }
    let h: Hello = serde_json::from_str(message).map_err(|_| "version")?;
    match &h {
        Hello::Create {
            version: VERSION,
            core,
            game,
            name,
        } => {
            if !hex(core, 64) || !hex(game, 40) || name.len() > 512 {
                return Err("game".into());
            }
        }
        Hello::Join {
            version: VERSION, ..
        }
        | Hello::Resume {
            version: VERSION, ..
        } => {}
        _ => return Err("version".into()),
    }
    Ok(serde_json::to_string(&h).unwrap())
}
