//! The Game Boy Advance: one player, L and R on the pad's bumpers.

use super::{Clock, Core, buttons, rgb555};
use crate::pad::*;

pub struct Gba {
    gba: gba::Gba,
    /// The cartridge's clock has been set since the machine powered on or a
    /// state went in: from then on it runs with the emulated time, and the
    /// page's later times (given before each save) leave it alone.
    clock_set: bool,
}

const MAP: [(u32, u16); 10] = [
    (EAST, gba::A),
    (SOUTH, gba::B),
    (SELECT, gba::SELECT),
    (START, gba::START),
    (UP, gba::UP),
    (DOWN, gba::DOWN),
    (LEFT, gba::LEFT),
    (RIGHT, gba::RIGHT),
    (L1, gba::L),
    (R1, gba::R),
];

impl Gba {
    pub fn new(rom: &[u8]) -> Result<Gba, String> {
        let gba = gba::Gba::new(rom).map_err(|e| e.to_string())?;
        Ok(Gba { gba, clock_set: false })
    }
}

impl Core for Gba {
    fn size(&self) -> (usize, usize) {
        (gba::WIDTH, gba::HEIGHT)
    }

    /// 16.78 MHz over the 280 896 cycles of a frame.
    fn frame_hz(&self) -> f64 {
        16_777_216.0 / 280_896.0
    }

    fn run_frame(&mut self, pads: [u32; 4]) {
        self.gba.run_frame(buttons(pads[0], &MAP));
    }

    fn picture(&self, out: &mut [u8]) {
        for (o, &p) in out.chunks_exact_mut(4).zip(self.gba.frame().iter()) {
            o.copy_from_slice(&rgb555(p));
        }
    }

    fn audio(&self) -> (&[i16], bool) {
        (self.gba.audio(), true)
    }

    fn sample_hz(&self) -> u32 {
        gba::SAMPLE_HZ
    }

    fn save_state(&self) -> Vec<u8> {
        self.gba.save_state()
    }

    /// A state holds the cartridge's clock as it was when it was taken: the
    /// next time the page gives sets it again.
    fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        self.gba.load_state(data).map_err(|e| e.to_string())?;
        self.clock_set = false;
        Ok(())
    }

    fn sram(&self) -> Option<Vec<u8>> {
        self.gba.save_data()
    }

    fn load_sram(&mut self, data: &[u8]) {
        self.gba.load_save_data(data);
    }

    /// Pokémon Ruby, Sapphire and Emerald keep a clock in the cartridge, set
    /// to the player's own time of day once, and not pulled back to it later
    /// (fast forward runs it ahead).
    fn set_clock(&mut self, now: Clock) {
        if !self.clock_set {
            self.gba.set_clock(now.local);
            self.clock_set = true;
        }
    }
}
