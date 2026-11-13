//! The colours of the NES's 64 palette entries (the common 2C02 table), and
//! the colour emphasis of PPUMASK, which dims the other channels.

const RGB: [[u8; 3]; 64] = [
    [84, 84, 84], [0, 30, 116], [8, 16, 144], [48, 0, 136], [68, 0, 100], [92, 0, 48], [84, 4, 0], [60, 24, 0],
    [32, 42, 0], [8, 58, 0], [0, 64, 0], [0, 60, 0], [0, 50, 60], [0, 0, 0], [0, 0, 0], [0, 0, 0],
    [152, 150, 152], [8, 76, 196], [48, 50, 236], [92, 30, 228], [136, 20, 176], [160, 20, 100], [152, 34, 32],
    [120, 60, 0], [84, 90, 0], [40, 114, 0], [8, 124, 0], [0, 118, 40], [0, 102, 120], [0, 0, 0], [0, 0, 0],
    [0, 0, 0],
    [236, 238, 236], [76, 154, 236], [120, 124, 236], [176, 98, 236], [228, 84, 236], [236, 88, 180],
    [236, 106, 100], [212, 136, 32], [160, 170, 0], [116, 196, 0], [76, 208, 32], [56, 204, 108], [56, 180, 204],
    [60, 60, 60], [0, 0, 0], [0, 0, 0],
    [236, 238, 236], [168, 204, 236], [188, 188, 236], [212, 178, 236], [236, 174, 236], [236, 174, 212],
    [236, 180, 176], [228, 196, 144], [204, 210, 120], [180, 222, 120], [168, 226, 144], [152, 226, 180],
    [160, 214, 228], [160, 162, 160], [0, 0, 0], [0, 0, 0],
];

/// The colour of a pixel of Nes::frame: a palette index with emphasis bits
/// (red, green, blue from bit 6). Emphasis dims the channels it does not
/// name to about 82%; the blacks of columns $E and $F stay black.
pub fn rgb(pixel: u16) -> [u8; 3] {
    let [r, g, b] = RGB[usize::from(pixel & 0x3f)];
    let emphasis = pixel >> 6 & 7;
    if emphasis == 0 || pixel & 0x0e == 0x0e {
        return [r, g, b];
    }
    let dim = |c: u8, bit: u16| if emphasis & bit != 0 { c } else { (u16::from(c) * 209 / 256) as u8 };
    [dim(r, 1), dim(g, 2), dim(b, 4)]
}
