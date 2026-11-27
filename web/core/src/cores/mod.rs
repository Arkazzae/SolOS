//! The cores the page runs, behind one interface: the buttons where they
//! sit on the console's own pad, and the picture a TV (or the handheld's
//! screen) showed, as RGBA for the browser.

mod gb;
mod gba;
mod md;
mod nes;
mod pico8;
mod sms;

/// The time now, as the page gives it: seconds since 1970 in UTC, and the
/// same in the player's time zone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    pub utc: u64,
    pub local: u64,
}

pub trait Core {
    /// The picture's width and height, now: the Mega Drive's changes with
    /// the game's mode.
    fn size(&self) -> (usize, usize);
    /// The shape of the console's pixels on the screen it was made for:
    /// their width over their height.
    fn pixel_aspect(&self) -> f64 {
        1.0
    }
    /// Its frames a second, exactly: the page runs as many as the time that
    /// passed holds, whatever the display's refresh.
    fn frame_hz(&self) -> f64;
    /// Runs a frame with each player's buttons held (bits of crate::pad): a
    /// console with one pad takes the first player's.
    fn run_frame(&mut self, pads: [u32; 4]);
    /// The last frame as RGBA, row after row, into a buffer of the size's.
    fn picture(&self, out: &mut [u8]);
    /// The last frame's sound, and whether it is stereo (left and right in
    /// turns) or one channel.
    fn audio(&self) -> (&[i16], bool);
    /// The sound's samples a second.
    fn sample_hz(&self) -> u32;
    /// The whole machine, for Quick Resume, and back from it.
    fn save_state(&self) -> Vec<u8>;
    fn load_state(&mut self, data: &[u8]) -> Result<(), String>;
    /// The cartridge's own save (battery RAM, EEPROM, flash), when it has
    /// one, and back from it.
    fn sram(&self) -> Option<Vec<u8>>;
    fn load_sram(&mut self, data: &[u8]);
    /// The time now, for a cartridge with a clock: given at power-on, after a
    /// state goes in and before each save, so a clock that runs by itself
    /// takes only the first after each of the first two.
    fn set_clock(&mut self, _now: Clock) {}
}

/// A core for a console, by its id (as web/src/engine/systems.ts names
/// them), with its game's file; or why not. The
/// Game Boy and the Game Boy Color are one core: the cartridge says which
/// console it is for, whichever shelf the page put it on.
pub fn open(sys: &str, rom: &[u8]) -> Result<Box<dyn Core>, String> {
    Ok(match sys {
        "nes" => Box::new(nes::Nes::new(rom)?),
        "gb" | "gbc" => Box::new(gb::Gb::new(rom)?),
        "gba" => Box::new(gba::Gba::new(rom)?),
        "sms" => Box::new(sms::Sms::new(rom, ::sms::System::MasterSystem)?),
        "gg" => Box::new(sms::Sms::new(rom, ::sms::System::GameGear)?),
        "md" => Box::new(md::Md::new(rom)?),
        "pico8" => Box::new(pico8::Pico8::new(rom)?),
        _ => return Err(format!("no core for {sys}")),
    })
}

/// The console's buttons held, from the pad's: each pair is a pad bit and
/// the console's bit for it.
fn buttons<T: Copy + std::ops::BitOr<Output = T> + Default>(pad: u32, map: &[(u32, T)]) -> T {
    map.iter().filter(|(bit, _)| pad & bit != 0).fold(T::default(), |held, &(_, console)| held | console)
}

/// A colour channel of `bits` bits widened to 8, its top bits repeated
/// below, so that the brightest is 255.
fn widen(c: u16, bits: u32) -> u8 {
    let c = c & ((1 << bits) - 1);
    (c << (8 - bits) | c >> (2 * bits - 8)) as u8
}

/// 5-bit red, green and blue (red in the low bits) to RGBA.
fn rgb555(p: u16) -> [u8; 4] {
    [widen(p, 5), widen(p >> 5, 5), widen(p >> 10, 5), 255]
}

/// 5-bit red (in the high bits), 6-bit green and 5-bit blue to RGBA.
fn rgb565(p: u16) -> [u8; 4] {
    [widen(p >> 11, 5), widen(p >> 5, 6), widen(p, 5), 255]
}

/// A table of 0xRRGGBB colours as RGBA.
fn rgba(rgb: u32) -> [u8; 4] {
    [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_widen_to_full_white() {
        assert_eq!(rgb555(0x7fff), [255, 255, 255, 255]);
        assert_eq!(rgb555(0x001f), [255, 0, 0, 255]);
        assert_eq!(rgb555(0x7c00), [0, 0, 255, 255]);
        assert_eq!(rgb565(0xffff), [255, 255, 255, 255]);
        assert_eq!(rgb565(0xf800), [255, 0, 0, 255]);
        assert_eq!(rgb565(0x07e0), [0, 255, 0, 255]);
        assert_eq!(rgb565(0x001f), [0, 0, 255, 255]);
        assert_eq!(rgba(0xff7a45), [255, 122, 69, 255]);
    }

    #[test]
    fn buttons_map_by_bit() {
        use crate::pad::*;
        let map = [(SOUTH, 1u8), (EAST, 2), (START, 4)];
        assert_eq!(buttons(SOUTH | START | UP, &map), 5);
        assert_eq!(buttons(0, &map), 0);
    }

    #[test]
    fn unknown_consoles_and_files_are_refused() {
        assert!(open("snes", &[0; 1024]).is_err());
        for sys in ["sms", "gg"] {
            // Headerless homebrew cartridges can be just a few instructions.
            assert!(open(sys, &[]).is_err());
        }
        for sys in ["nes", "gb", "gbc", "gba", "md", "pico8"] {
            assert!(open(sys, &[0; 16]).is_err(), "{sys} took 16 zeros for a game");
        }
    }
}
