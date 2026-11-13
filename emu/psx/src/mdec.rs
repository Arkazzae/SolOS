//! The MDEC: decodes macroblocks of a JPEG-like format for movies. A
//! command word, then its parameters (by MDEC0 or DMA channel 0): quant
//! tables, the IDCT's scale table, or compressed macroblocks, which come
//! out (by DMA channel 1) as 16x16 pixels of 15- or 24-bit colour, or 8x8
//! of 4- or 8-bit grey. A block is run-length coded coefficients in zigzag
//! order, scaled by the quant table, through an 8x8 IDCT; colour blocks
//! (Cr, Cb at half resolution) and four luminance blocks make a macroblock.

use savestate::State;
use std::collections::VecDeque;

/// Where each coefficient of the zigzag order goes in a block.
const ZAGZIG: [usize; 64] = {
    const ZIGZAG: [usize; 64] = [
        0, 1, 5, 6, 14, 15, 27, 28, 2, 4, 7, 13, 16, 26, 29, 42, 3, 8, 12, 17, 25, 30, 41, 43, 9, 11, 18, 24, 31, 40,
        44, 53, 10, 19, 23, 32, 39, 45, 52, 54, 20, 22, 33, 38, 46, 51, 55, 60, 21, 34, 37, 47, 50, 56, 59, 61, 35,
        36, 48, 49, 57, 58, 62, 63,
    ];
    let mut t = [0; 64];
    let mut i = 0;
    while i < 64 {
        t[ZIGZAG[i]] = i;
        i += 1;
    }
    t
};

#[derive(Clone)]
pub struct Mdec {
    /// The command running, and the parameter words it still wants.
    command: u32,
    remaining: u32,
    params: Vec<u32>,
    pub output: VecDeque<u32>,
    iq_y: [u8; 64],
    iq_uv: [u8; 64],
    scale: [i16; 64],
    enable_in: bool,
    enable_out: bool,
}

impl Default for Mdec {
    fn default() -> Mdec {
        Mdec {
            command: 0,
            remaining: 0,
            params: Vec::new(),
            output: VecDeque::new(),
            iq_y: [0; 64],
            iq_uv: [0; 64],
            scale: [0; 64],
            enable_in: false,
            enable_out: false,
        }
    }
}

impl State for Mdec {
    fn save(&self, out: &mut Vec<u8>) {
        (self.command, self.remaining).save(out);
        (self.iq_y, self.iq_uv).save(out);
        (self.scale, (self.enable_in, self.enable_out)).save(out);
        for words in [&self.params.clone(), &self.output.iter().copied().collect()] {
            words.len().save(out);
            words.iter().for_each(|w| w.save(out));
        }
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), savestate::BadState> {
        let mut a = (0u32, 0u32);
        a.load(input)?;
        (self.command, self.remaining) = a;
        let mut q = ([0u8; 64], [0u8; 64]);
        q.load(input)?;
        (self.iq_y, self.iq_uv) = q;
        let mut s = ([0i16; 64], (false, false));
        s.load(input)?;
        (self.scale, (self.enable_in, self.enable_out)) = s;
        let words = |input: &mut &[u8]| -> Result<Vec<u32>, savestate::BadState> {
            let mut n = 0usize;
            n.load(input)?;
            (0..n).map(|_| {
                let mut w = 0u32;
                w.load(input).map(|_| w)
            }).collect()
        };
        self.params = words(input)?;
        self.output = words(input)?.into();
        Ok(())
    }
}

impl Mdec {
    pub fn status(&self) -> u32 {
        let mut s = u32::from(self.output.is_empty()) << 31;
        s |= u32::from(self.remaining > 0) << 29;
        s |= u32::from(self.enable_in && self.remaining > 0) << 28;
        s |= u32::from(self.enable_out && !self.output.is_empty()) << 27;
        // Depth, signed and bit 15 as the command gave them.
        s |= (self.command >> 25 & 0xf) << 23;
        s |= 4 << 16;
        s | (self.remaining.wrapping_sub(1) & 0xffff)
    }

    /// A command still takes parameters.
    pub fn busy(&self) -> bool {
        self.remaining > 0
    }

    pub fn control(&mut self, value: u32) {
        if value & 1 << 31 != 0 {
            *self = Mdec { iq_y: self.iq_y, iq_uv: self.iq_uv, scale: self.scale, ..Mdec::default() };
        }
        self.enable_in = value & 1 << 30 != 0;
        self.enable_out = value & 1 << 29 != 0;
    }

    /// A word to MDEC0: a command, or its next parameter.
    pub fn write(&mut self, value: u32) {
        if self.remaining == 0 {
            self.command = value;
            self.params.clear();
            self.remaining = match value >> 29 {
                1 => value & 0xffff,
                2 => if value & 1 != 0 { 32 } else { 16 },
                3 => 32,
                _ => 0,
            };
            return;
        }
        self.params.push(value);
        self.remaining -= 1;
        if self.remaining == 0 {
            self.run();
        }
    }

    pub fn read(&mut self) -> u32 {
        self.output.pop_front().unwrap_or(0)
    }

    fn run(&mut self) {
        let bytes: Vec<u8> = self.params.iter().flat_map(|w| w.to_le_bytes()).collect();
        match self.command >> 29 {
            1 => self.decode(),
            2 => {
                self.iq_y.copy_from_slice(&bytes[..64]);
                if self.command & 1 != 0 {
                    self.iq_uv.copy_from_slice(&bytes[64..128]);
                }
            }
            3 => {
                for (s, pair) in self.scale.iter_mut().zip(bytes.chunks_exact(2)) {
                    *s = i16::from_le_bytes([pair[0], pair[1]]);
                }
            }
            _ => {}
        }
    }

    /// One block's run-length coded coefficients, from `at`: scaled by
    /// the quant table and through the IDCT. None if the data ends first.
    fn block(&self, data: &[u16], at: &mut usize, qt: &[u8; 64]) -> Option<[i16; 64]> {
        let mut blk = [0i32; 64];
        let mut n;
        loop {
            n = *data.get(*at)?;
            *at += 1;
            if n != 0xfe00 {
                break;
            }
        }
        let q_scale = i32::from(n >> 10 & 0x3f);
        let value = |n: u16| ((n as i32) << 22) >> 22;
        let mut k = 0usize;
        let mut val = value(n) * i32::from(qt[0]);
        loop {
            if q_scale == 0 {
                val = value(n) * 2;
            }
            let val_c = val.clamp(-0x400, 0x3ff);
            if q_scale > 0 {
                blk[ZAGZIG[k]] = val_c;
            } else {
                blk[k] = val_c;
            }
            // The last coefficient ends the block, end code or not.
            if k >= 63 {
                break;
            }
            n = *data.get(*at)?;
            *at += 1;
            k += usize::from(n >> 10 & 0x3f) + 1;
            if k > 63 {
                break;
            }
            val = (value(n) * i32::from(qt[k]) * q_scale + 4) >> 3;
        }
        Some(self.idct(&blk))
    }

    /// The IDCT by the scale table, in two passes kept in full precision,
    /// rounded and cut to signed 9 bits, then 8.
    fn idct(&self, blk: &[i32; 64]) -> [i16; 64] {
        let scale = self.scale.map(i64::from);
        let mut temp = [0i64; 64];
        for x in 0..8 {
            for y in 0..8 {
                temp[x + y * 8] = (0..8).map(|u| i64::from(blk[u * 8 + x]) * scale[u * 8 + y]).sum();
            }
        }
        let mut out = [0i16; 64];
        for x in 0..8 {
            for y in 0..8 {
                let sum: i64 = (0..8).map(|u| temp[u + y * 8] * scale[u * 8 + x]).sum();
                let v = ((sum >> 32) + ((sum >> 31) & 1)) as i32;
                let v = (v << 23) >> 23;
                out[x + y * 8] = v.clamp(-128, 127) as i16;
            }
        }
        out
    }

    /// The command's macroblocks, decoded into the output.
    fn decode(&mut self) {
        let data: Vec<u16> = self.params.iter().flat_map(|&w| [w as u16, (w >> 16) as u16]).collect();
        let depth = self.command >> 27 & 3;
        let signed = self.command & 1 << 26 != 0;
        let bit15 = (self.command >> 25 & 1) << 15;
        let add = if signed { 0 } else { 0x80 };
        let mut at = 0;
        if depth < 2 {
            // Grey 8x8 blocks.
            while let Some(y) = self.block(&data, &mut at, &self.iq_y) {
                let grey: Vec<u8> = y.iter().map(|&v| (i32::from(v) + add) as u8).collect();
                if depth == 1 {
                    for w in grey.chunks_exact(4) {
                        self.output.push_back(u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
                    }
                } else {
                    for w in grey.chunks_exact(8) {
                        let word = w.iter().enumerate().fold(0, |acc, (i, &g)| acc | u32::from(g >> 4) << (4 * i));
                        self.output.push_back(word);
                    }
                }
            }
            return;
        }
        while let Some(cr) = self.block(&data, &mut at, &self.iq_uv) {
            let Some(cb) = self.block(&data, &mut at, &self.iq_uv) else { break };
            let mut rgb = [[0u8; 3]; 256];
            for (xx, yy) in [(0, 0), (8, 0), (0, 8), (8, 8)] {
                let Some(y) = self.block(&data, &mut at, &self.iq_y) else { return };
                for py in 0..8 {
                    for px in 0..8 {
                        let c = (px + xx) / 2 + (py + yy) / 2 * 8;
                        let (r, b) = (f32::from(cr[c]), f32::from(cb[c]));
                        let g = (-0.3437 * b + -0.7143 * r) as i32;
                        let (r, b) = ((1.402 * r) as i32, (1.772 * b) as i32);
                        let l = i32::from(y[px + py * 8]);
                        let channel = |v: i32| ((l + v).clamp(-128, 127) + add) as u8;
                        rgb[(px + xx) + (py + yy) * 16] = [channel(r), channel(g), channel(b)];
                    }
                }
            }
            if depth == 2 {
                let bytes: Vec<u8> = rgb.iter().flatten().copied().collect();
                for w in bytes.chunks_exact(4) {
                    self.output.push_back(u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
                }
            } else {
                let pixel = |p: [u8; 3]| {
                    // Rounded to five bits, as the console's pictures show.
                    let [r, g, b] = p.map(|c| ((u32::from(c) + 4) >> 3).min(31));
                    r | g << 5 | b << 10 | bit15
                };
                for pair in rgb.chunks_exact(2) {
                    self.output.push_back(pixel(pair[0]) | pixel(pair[1]) << 16);
                }
            }
        }
    }
}
