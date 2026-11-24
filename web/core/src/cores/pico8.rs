//! PICO-8: a cartridge's Lua on the fantasy console, 128×128 in sixteen
//! colours (and the sixteen secret ones), one channel of sound at 22 050 Hz,
//! the 256 bytes of cartdata as the cartridge's save.
//!
//! A cartridge runs at 30 or 60 frames a second, and which it is only shows
//! once its code has run: so the page always runs it at 60, and a 30 Hz game
//! makes a frame every other refresh, its sound split between the two.

use std::ops::Range;

use super::{Clock, Core, buttons, rgba};
use crate::pad::*;

pub struct Pico8 {
    p8: pico8::Pico8,
    /// The palette, as RGBA.
    colours: [[u8; 4]; 32],
    /// The sound of the last frame of the game, the part of it this refresh
    /// gives, and how much of it has gone out.
    sound: Vec<i16>,
    out: Range<usize>,
    sent: usize,
    /// The second refresh of a 30 Hz game's frame is next.
    half: bool,
    /// rnd() has been seeded from the page's clock.
    seeded: bool,
    /// The error that stopped the game has gone to the browser's console.
    told: bool,
}

/// 🅾️ on the bottom face button and the top one, ❎ on the right and the
/// left, as PICO-8 on a pad (Celeste: 🅾️ jumps, ❎ dashes); Start and Select
/// open PICO-8's pause menu, with the cartridge's own items.
const MAP: [(u32, u8); 10] = [
    (SOUTH, pico8::O),
    (NORTH, pico8::O),
    (EAST, pico8::X),
    (WEST, pico8::X),
    (LEFT, pico8::LEFT),
    (RIGHT, pico8::RIGHT),
    (UP, pico8::UP),
    (DOWN, pico8::DOWN),
    (START, pico8::PAUSE),
    (SELECT, pico8::PAUSE),
];

impl Pico8 {
    pub fn new(cart: &[u8]) -> Result<Pico8, String> {
        let p8 = pico8::Pico8::new(cart).map_err(|e| e.to_string())?;
        Ok(Pico8 {
            p8,
            colours: pico8::PALETTE.map(rgba),
            sound: Vec::new(),
            out: 0..0,
            sent: 0,
            half: false,
            seeded: false,
            told: false,
        })
    }
}

impl Core for Pico8 {
    fn size(&self) -> (usize, usize) {
        (pico8::WIDTH, pico8::HEIGHT)
    }

    fn frame_hz(&self) -> f64 {
        60.0
    }

    /// A player a pad, up to the page's four.
    fn run_frame(&mut self, pads: [u32; 4]) {
        if self.half {
            // The second refresh of a 30 Hz frame: the rest of its sound.
            self.half = false;
            self.out = self.sent..self.sound.len();
            self.sent = self.sound.len();
            return;
        }
        let mut held = [0u8; 8];
        for (h, &pad) in held.iter_mut().zip(&pads) {
            *h = buttons(pad, &MAP);
        }
        if let Err(e) = self.p8.run_frame(held)
            && !self.told
        {
            // The error is on the picture too, as PICO-8 shows it.
            crate::log(&format!("solweb: pico8: {e}"));
            self.told = true;
        }
        self.sound.clear();
        self.sound.extend_from_slice(self.p8.audio());
        self.sent = if self.p8.fps() == 30 { self.sound.len() / 2 } else { self.sound.len() };
        self.half = self.p8.fps() == 30;
        self.out = 0..self.sent;
    }

    fn picture(&self, out: &mut [u8]) {
        for (o, &c) in out.chunks_exact_mut(4).zip(self.p8.frame().iter()) {
            o.copy_from_slice(&self.colours[usize::from(c & 31)]);
        }
    }

    fn audio(&self) -> (&[i16], bool) {
        (&self.sound[self.out.clone()], false)
    }

    fn sample_hz(&self) -> u32 {
        pico8::SAMPLE_HZ as u32
    }

    fn save_state(&self) -> Vec<u8> {
        self.p8.save_state()
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        self.p8.load_state(data).map_err(|e| e.to_string())?;
        (self.half, self.sent, self.out) = (false, 0, 0..0);
        self.sound.clear();
        Ok(())
    }

    /// The cartdata, once the cartridge has asked for it.
    fn sram(&self) -> Option<Vec<u8>> {
        self.p8.cartdata_id().map(|_| self.p8.persistent().to_vec())
    }

    fn load_sram(&mut self, data: &[u8]) {
        self.p8.set_persistent(data);
    }

    /// The time of day for stat(80) and on, in the player's time zone. The
    /// first time, before the cartridge starts, it seeds rnd() too.
    fn set_clock(&mut self, now: Clock) {
        if !self.seeded {
            self.p8.seed(now.utc as u32);
            self.seeded = true;
        }
        self.p8.set_clock(now.local);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cart(code: &str) -> Vec<u8> {
        format!("pico-8 cartridge // http://www.pico-8.com\nversion 42\n__lua__\n{code}\n").into_bytes()
    }

    #[test]
    fn a_30_hz_game_shares_its_frame_and_sound_between_two_refreshes() {
        let mut core = Pico8::new(&cart("n=0 function _update() n+=1 end function _draw() cls(n%16) end")).unwrap();
        let mut picture = vec![0u8; 128 * 128 * 4];
        let mut samples = 0;
        for _ in 0..60 {
            core.run_frame([0; 4]);
            samples += core.audio().0.len();
        }
        core.picture(&mut picture);
        // 30 frames of the game in 60 refreshes, and a second of sound.
        assert_eq!(picture[..4], core.colours[14]);
        assert_eq!(samples, 22_050);
    }

    #[test]
    fn the_bottom_button_is_o_and_each_pad_a_player() {
        let mut core = Pico8::new(&cart("function _update60() a=btn(4,0) b=btn(5,1) end")).unwrap();
        core.run_frame([SOUTH, EAST, 0, 0]);
        assert_eq!(core.p8.global_text("a"), "true");
        assert_eq!(core.p8.global_text("b"), "true");
    }

    #[test]
    fn start_opens_the_pause_menu() {
        let mut core = Pico8::new(&cart("function _update60() end")).unwrap();
        core.run_frame([0; 4]);
        core.run_frame([START, 0, 0, 0]);
        assert!(core.p8.paused());
    }
}
