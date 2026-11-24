//! The Mega Drive. Its picture changes size as the game switches mode: 256
//! or 320 across, 224 or (on a European console) 240 down, its pixels the
//! shape an NTSC TV gave them. A European game runs at 50 frames a second.

use super::{Core, buttons, rgb565};
use crate::pad::*;

pub struct Md(md::Md);

/// By place, not by letter: A, B and C are the pad's bottom row (left,
/// bottom and right face buttons), X, Y and Z the row above (the left
/// bumper, the top face button, the right bumper), Mode on Select.
const MAP: [(u32, u16); 12] = [
    (WEST, md::A),
    (SOUTH, md::B),
    (EAST, md::C),
    (L1, md::X),
    (NORTH, md::Y),
    (R1, md::Z),
    (START, md::START),
    (SELECT, md::MODE),
    (UP, md::UP),
    (DOWN, md::DOWN),
    (LEFT, md::LEFT),
    (RIGHT, md::RIGHT),
];

impl Md {
    pub fn new(rom: &[u8]) -> Result<Md, String> {
        md::Md::new(rom).map(Md).map_err(|e| e.to_string())
    }
}

impl Core for Md {
    fn size(&self) -> (usize, usize) {
        self.0.dimensions()
    }

    /// 8:7 at 256 across, the NES's pixel clock; 320 across the clock is a
    /// quarter faster and the pixels a fifth narrower.
    fn pixel_aspect(&self) -> f64 {
        if self.0.dimensions().0 == 320 { 32.0 / 35.0 } else { 8.0 / 7.0 }
    }

    /// The master clock over a frame's lines of 3420 clocks each: 262 on an
    /// American or Japanese console, 313 on a European one.
    fn frame_hz(&self) -> f64 {
        if self.0.region().pal() { 53_203_424.0 / (3420.0 * 313.0) } else { 53_693_175.0 / (3420.0 * 262.0) }
    }

    /// The first two players' pads, on the console's two ports.
    fn run_frame(&mut self, pads: [u32; 4]) {
        self.0.run_frame([buttons(pads[0], &MAP), buttons(pads[1], &MAP)]);
    }

    fn picture(&self, out: &mut [u8]) {
        for (o, &p) in out.chunks_exact_mut(4).zip(self.0.frame()) {
            o.copy_from_slice(&rgb565(p));
        }
    }

    fn audio(&self) -> (&[i16], bool) {
        (self.0.audio(), true)
    }

    fn sample_hz(&self) -> u32 {
        md::SAMPLE_HZ
    }

    fn save_state(&self) -> Vec<u8> {
        self.0.save_state()
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        self.0.load_state(data).map_err(|e| e.to_string())
    }

    fn sram(&self) -> Option<Vec<u8>> {
        self.0.save_data().map(<[u8]>::to_vec)
    }

    fn load_sram(&mut self, data: &[u8]) {
        if let Err(e) = self.0.load_save_data(data) {
            crate::log(&format!("solweb: the cartridge's save: {e}"));
        }
    }
}
