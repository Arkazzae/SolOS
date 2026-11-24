//! The Game Boy and the Game Boy Color, one core: the cartridge's header says
//! which console it runs on. The original's picture is four shades, drawn in
//! its green; the Color's is RGB555.

use super::{Clock, Core, buttons, rgb555, rgba};
use crate::pad::*;

pub struct Gb {
    gb: gb::Gb,
    /// The time the page last gave, for the clock an MBC3 cartridge keeps
    /// beside its save (the save holds when it was taken, so the clock moves
    /// on by the time between).
    now: u64,
}

/// The original Game Boy's four shades, lightest first: its green.
const SHADES: [u32; 4] = [0xe0f8d0, 0x88c070, 0x346856, 0x081820];

const MAP: [(u32, u8); 8] = [
    (EAST, gb::A),
    (SOUTH, gb::B),
    (SELECT, gb::SELECT),
    (START, gb::START),
    (UP, gb::UP),
    (DOWN, gb::DOWN),
    (LEFT, gb::LEFT),
    (RIGHT, gb::RIGHT),
];

impl Gb {
    pub fn new(rom: &[u8]) -> Result<Gb, String> {
        let gb = gb::Gb::new(rom, gb::Model::detect(rom)).map_err(|e| e.to_string())?;
        Ok(Gb { gb, now: 0 })
    }
}

impl Core for Gb {
    fn size(&self) -> (usize, usize) {
        (gb::WIDTH, gb::HEIGHT)
    }

    /// 4 194 304 Hz over the 70 224 clocks of a frame.
    fn frame_hz(&self) -> f64 {
        4_194_304.0 / 70_224.0
    }

    fn run_frame(&mut self, pads: [u32; 4]) {
        self.gb.run_frame(buttons(pads[0], &MAP));
    }

    fn picture(&self, out: &mut [u8]) {
        let colour = self.gb.color();
        let shades = SHADES.map(rgba);
        for (o, &p) in out.chunks_exact_mut(4).zip(self.gb.frame().iter()) {
            o.copy_from_slice(&if colour { rgb555(p) } else { shades[usize::from(p & 3)] });
        }
    }

    fn audio(&self) -> (&[i16], bool) {
        (self.gb.audio(), true)
    }

    fn sample_hz(&self) -> u32 {
        gb::SAMPLE_HZ
    }

    fn save_state(&self) -> Vec<u8> {
        self.gb.save_state()
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        self.gb.load_state(data).map_err(|e| e.to_string())
    }

    fn sram(&self) -> Option<Vec<u8>> {
        self.gb.save_data(self.now)
    }

    fn load_sram(&mut self, data: &[u8]) {
        self.gb.load_save_data(data, self.now);
    }

    fn set_clock(&mut self, now: Clock) {
        self.now = now.utc;
    }
}
