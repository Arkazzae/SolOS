//! Master System and Game Gear share a machine. The handheld uses square
//! pixels in its LCD window and TV pixels in Master System compatibility mode.

use super::{Core, buttons, rgb565};
use crate::pad::*;
use sms::System;

pub struct Sms(sms::Sms);

const MAP: [(u32, u16); 7] = [
    (SOUTH, sms::BUTTON_1),
    (EAST, sms::BUTTON_2),
    (START, sms::START),
    (UP, sms::UP),
    (DOWN, sms::DOWN),
    (LEFT, sms::LEFT),
    (RIGHT, sms::RIGHT),
];

impl Sms {
    pub fn new(rom: &[u8], system: System) -> Result<Self, String> {
        sms::Sms::new(rom, system).map(Self).map_err(|e| e.to_string())
    }
}

impl Core for Sms {
    fn size(&self) -> (usize, usize) {
        self.0.dimensions()
    }

    fn pixel_aspect(&self) -> f64 {
        if self.0.system() == System::GameGear && !self.0.sms_mode() { 1.0 } else { 8.0 / 7.0 }
    }

    fn frame_hz(&self) -> f64 {
        self.0.frame_hz()
    }

    fn run_frame(&mut self, pads: [u32; 4]) {
        let second = if self.0.system() == System::MasterSystem { buttons(pads[1], &MAP) } else { 0 };
        self.0.run_frame([buttons(pads[0], &MAP), second]);
    }

    fn picture(&self, out: &mut [u8]) {
        for (pixel, &color) in out.chunks_exact_mut(4).zip(self.0.frame()) {
            pixel.copy_from_slice(&rgb565(color));
        }
    }

    fn audio(&self) -> (&[i16], bool) {
        (self.0.audio(), true)
    }

    fn sample_hz(&self) -> u32 {
        sms::SAMPLE_HZ
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
