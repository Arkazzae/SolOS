//! The NES: two pads, the picture without the edges a TV's overscan hid,
//! its pixels 8:7 as an NTSC TV showed them.

use super::{Core, buttons};
use crate::pad::*;

pub struct Nes {
    nes: nes::Nes,
    /// Each palette entry with its emphasis bits, as RGBA.
    colours: Vec<[u8; 4]>,
}

/// The pixels a TV's overscan hid at each edge of the picture: games keep
/// there what was never meant to be seen, as the tiles a scroll is about to
/// bring in.
const OVERSCAN: usize = 8;
const WIDTH: usize = nes::WIDTH - 2 * OVERSCAN;
const HEIGHT: usize = nes::HEIGHT - 2 * OVERSCAN;

/// The pad's buttons by where they sit: the right face button is A, the
/// bottom one B.
const MAP: [(u32, u8); 8] = [
    (EAST, nes::A),
    (SOUTH, nes::B),
    (SELECT, nes::SELECT),
    (START, nes::START),
    (UP, nes::UP),
    (DOWN, nes::DOWN),
    (LEFT, nes::LEFT),
    (RIGHT, nes::RIGHT),
];

impl Nes {
    pub fn new(rom: &[u8]) -> Result<Nes, String> {
        let mut nes = nes::Nes::new(rom).map_err(|e| e.to_string())?;
        // All the sprites on a line: no flicker where a game has more than
        // the console's eight.
        nes.set_sprite_limit(false);
        let colours = (0..512u16)
            .map(|i| {
                let [r, g, b] = nes::palette::rgb(i);
                [r, g, b, 255]
            })
            .collect();
        Ok(Nes { nes, colours })
    }
}

impl Core for Nes {
    fn size(&self) -> (usize, usize) {
        (WIDTH, HEIGHT)
    }

    fn pixel_aspect(&self) -> f64 {
        8.0 / 7.0
    }

    /// The CPU's 1 789 772.7 Hz over the 29 780.5 cycles of a frame (a dot
    /// shorter every other one).
    fn frame_hz(&self) -> f64 {
        236_250_000.0 / 11.0 / 12.0 / 29_780.5
    }

    fn run_frame(&mut self, pads: [u32; 4]) {
        self.nes.run_frame([buttons(pads[0], &MAP), buttons(pads[1], &MAP)]);
    }

    fn picture(&self, out: &mut [u8]) {
        let lines = self.nes.frame().chunks_exact(nes::WIDTH).skip(OVERSCAN);
        for (row, line) in out.chunks_exact_mut(WIDTH * 4).zip(lines) {
            for (o, &p) in row.chunks_exact_mut(4).zip(&line[OVERSCAN..]) {
                o.copy_from_slice(&self.colours[usize::from(p & 0x1ff)]);
            }
        }
    }

    fn audio(&self) -> (&[i16], bool) {
        (self.nes.audio(), false)
    }

    fn sample_hz(&self) -> u32 {
        nes::SAMPLE_HZ
    }

    fn save_state(&self) -> Vec<u8> {
        self.nes.save_state()
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        self.nes.load_state(data).map_err(|e| e.to_string())
    }

    fn sram(&self) -> Option<Vec<u8>> {
        self.nes.battery_ram().map(<[u8]>::to_vec)
    }

    fn load_sram(&mut self, data: &[u8]) {
        self.nes.load_battery_ram(data);
    }
}
