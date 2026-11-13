//! Physical keyboard/controller mappings and the machine's exact frame clock.
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Keyboard {
    down: [bool; 512],
    latched: [u16; 2],
}
impl Default for Keyboard {
    fn default() -> Self {
        Self {
            down: [false; 512],
            latched: [0; 2],
        }
    }
}
impl Keyboard {
    pub fn key(&mut self, scancode: i32, pressed: bool, repeat: bool) {
        if repeat {
            return;
        }
        let Ok(i) = usize::try_from(scancode) else {
            return;
        };
        if i >= self.down.len() {
            return;
        }
        self.down[i] = pressed;
        if pressed {
            for (port, key, button) in MAPPING {
                if key == i {
                    self.latched[port] |= button;
                }
            }
        }
    }
    pub fn clear(&mut self) {
        self.down.fill(false);
        self.latched = [0; 2];
    }
    /// Press/release events between frames still reach one input sample.
    pub fn take_frame(&mut self) -> [u16; 2] {
        let mut pads = self.latched;
        self.latched = [0; 2];
        for (port, key, button) in MAPPING {
            if self.down[key] {
                pads[port] |= button;
            }
        }
        pads.map(clean_directions)
    }
}

const MAPPING: [(usize, usize, u16); 24] = [
    (0, 82, md::UP),
    (0, 81, md::DOWN),
    (0, 80, md::LEFT),
    (0, 79, md::RIGHT),
    (0, 29, md::A),
    (0, 27, md::B),
    (0, 6, md::C),
    (0, 4, md::X),
    (0, 22, md::Y),
    (0, 7, md::Z),
    (0, 40, md::START),
    (0, 229, md::MODE),
    (1, 96, md::UP),
    (1, 90, md::DOWN),
    (1, 92, md::LEFT),
    (1, 94, md::RIGHT),
    (1, 95, md::A),
    (1, 97, md::B),
    (1, 98, md::C),
    (1, 89, md::X),
    (1, 91, md::Y),
    (1, 99, md::Z),
    (1, 88, md::START),
    (1, 87, md::MODE),
];

pub fn clean_directions(mut pad: u16) -> u16 {
    if pad & (md::LEFT | md::RIGHT) == md::LEFT | md::RIGHT {
        pad &= !(md::LEFT | md::RIGHT);
    }
    if pad & (md::UP | md::DOWN) == md::UP | md::DOWN {
        pad &= !(md::UP | md::DOWN);
    }
    pad
}

/// SDL2's standard controller layout, regardless of the physical brand.
pub fn controller(buttons: u32, x: i16, y: i16) -> u16 {
    let mut pad = 0;
    for (button, bit) in [
        (0, md::B),
        (1, md::C),
        (2, md::A),
        (3, md::Y),
        (4, md::MODE),
        (6, md::START),
        (9, md::X),
        (10, md::Z),
        (11, md::UP),
        (12, md::DOWN),
        (13, md::LEFT),
        (14, md::RIGHT),
    ] {
        if buttons & (1 << button) != 0 {
            pad |= bit;
        }
    }
    if x < -12000 {
        pad |= md::LEFT;
    }
    if x > 12000 {
        pad |= md::RIGHT;
    }
    if y < -12000 {
        pad |= md::UP;
    }
    if y > 12000 {
        pad |= md::DOWN;
    }
    clean_directions(pad)
}

pub struct Pacer {
    pal: bool,
    frames: u64,
    origin: Instant,
}
impl Pacer {
    pub fn new(pal: bool) -> Self {
        Self {
            pal,
            frames: 0,
            origin: Instant::now(),
        }
    }
    pub fn rebase(&mut self) {
        self.frames = 0;
        self.origin = Instant::now();
    }
    pub fn next(&mut self) -> Instant {
        self.frames += 1;
        self.origin + frame_time(self.frames, self.pal)
    }
}
pub fn frame_time(frames: u64, pal: bool) -> Duration {
    let (clocks, hz): (u128, u128) = if pal {
        (3420 * 313, 53_203_424)
    } else {
        (3420 * 262, 53_693_175)
    };
    let nanos = u128::from(frames) * clocks * 1_000_000_000 / hz;
    Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mappings_preserve_taps_and_release_on_focus_loss() {
        let mut k = Keyboard::default();
        k.key(27, true, false);
        k.key(27, false, false);
        assert_eq!(k.take_frame(), [md::B, 0]);
        assert_eq!(k.take_frame(), [0, 0]);
        k.key(95, true, false);
        assert_eq!(k.take_frame(), [0, md::A]);
        k.clear();
        assert_eq!(k.take_frame(), [0, 0]);
        k.key(27, true, true);
        assert_eq!(k.take_frame(), [0, 0]);
    }
    #[test]
    fn opposing_directions_cancel_after_combining_sources() {
        assert_eq!(clean_directions(md::LEFT | md::RIGHT | md::A), md::A);
        assert_eq!(
            controller(1 | (1 << 6), -20000, 20000),
            md::B | md::START | md::LEFT | md::DOWN
        );
        assert_eq!(controller(0, 11999, -11999), 0);
    }
    #[test]
    fn exact_master_clock_pacing_has_no_integer_fps_drift() {
        assert_eq!(frame_time(60, false).as_nanos(), 1_001_289_270);
        assert_eq!(frame_time(50, true).as_nanos(), 1_006_006_681);
        let a = frame_time(1_000_000, false).as_nanos();
        let b = 1_000_000u128 * 3420 * 262 * 1_000_000_000 / 53_693_175;
        assert_eq!(a, b);
    }
}
