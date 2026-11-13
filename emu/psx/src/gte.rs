//! The GTE (coprocessor 2): fixed-point vector and matrix arithmetic for
//! 3D, in 32 data and 32 control registers. A command multiplies and adds
//! into the 44-bit accumulators MAC1-3 (and the 32-bit MAC0), each step's
//! overflow noted in FLAG, and saturates what it keeps: IR0-3, the screen
//! coordinates (SXY), depths (SZ) and colours (RGB), the last three in
//! small FIFOs.

use savestate::state;

#[derive(Clone, Default)]
pub struct Gte {
    data: [u32; 32],
    control: [u32; 32],
}

state!(Gte { data, control });

// Data registers.
const VXY0: usize = 0;
const RGBC: usize = 6;
const OTZ: usize = 7;
const IR0: usize = 8;
const SXY0: usize = 12;
const SXY2: usize = 14;
const SZ0: usize = 16;
const SZ3: usize = 19;
const RGB0: usize = 20;
const RGB2: usize = 22;
const MAC0: usize = 24;
const LZCS: usize = 30;
const LZCR: usize = 31;

// Control registers: the rotation, light and light colour matrices (five
// words each, from these), their translation vectors and the rest.
const RT: usize = 0;
const TR: usize = 5;
const LLM: usize = 8;
const BK: usize = 13;
const LCM: usize = 16;
const FC: usize = 21;
const OFX: usize = 24;
const OFY: usize = 25;
const H: usize = 26;
const DQA: usize = 27;
const DQB: usize = 28;
const ZSF3: usize = 29;
const ZSF4: usize = 30;
const FLAG: usize = 31;

/// The ends of the reciprocal table the division starts from.
const UNR: [u8; 257] = {
    let mut table = [0; 257];
    let mut i = 0;
    while i < 257 {
        let v = (0x40000 / (i as i32 + 0x100) + 1) / 2 - 0x101;
        table[i] = if v > 0 { v as u8 } else { 0 };
        i += 1;
    }
    table
};

type Matrix = [[i64; 3]; 3];

impl Gte {
    /// A data register as MFC2 reads it: 16-bit ones sign- or
    /// zero-extended, IRGB and ORGB the colour IR1-3 make.
    pub fn read_data(&self, n: usize) -> u32 {
        match n {
            1 | 3 | 5 | 8..=11 => self.data[n] as i16 as u32,
            7 | 16..=19 => self.data[n] & 0xffff,
            15 => self.data[SXY2],
            28 | 29 => {
                let c = |ir: usize| ((self.data[ir] as i16 >> 7).clamp(0, 0x1f)) as u32;
                c(9) | c(10) << 5 | c(11) << 10
            }
            _ => self.data[n],
        }
    }

    pub fn write_data(&mut self, n: usize, value: u32) {
        match n {
            // SXYP pushes onto the screen coordinates' FIFO.
            15 => {
                self.data[SXY0] = self.data[SXY0 + 1];
                self.data[SXY0 + 1] = self.data[SXY2];
                self.data[SXY2] = value;
            }
            // IRGB sets IR1-3 from five bits each.
            28 => {
                for i in 0..3 {
                    self.data[9 + i] = (value >> (5 * i) & 0x1f) << 7;
                }
            }
            29 | LZCR => {}
            LZCS => {
                self.data[LZCS] = value;
                let lead = if (value as i32) < 0 { value.leading_ones() } else { value.leading_zeros() };
                self.data[LZCR] = lead;
            }
            _ => self.data[n] = value,
        }
    }

    pub fn read_control(&self, n: usize) -> u32 {
        match n {
            // H reads sign-extended too, though it is unsigned.
            4 | 12 | 20 | 26 | 27 | 29 | 30 => self.control[n] as i16 as u32,
            _ => self.control[n],
        }
    }

    pub fn write_control(&mut self, n: usize, value: u32) {
        if n == FLAG {
            self.control[FLAG] = value & 0x7fff_f000;
            self.update_error();
        } else {
            self.control[n] = value;
        }
    }

    /// Runs a command; the cycles it takes.
    pub fn command(&mut self, op: u32) -> u64 {
        self.control[FLAG] = 0;
        let shift = if op & 1 << 19 != 0 { 12 } else { 0 };
        let lm = op & 1 << 10 != 0;
        let cycles = match op & 0x3f {
            0x01 => {
                self.rtp(0, shift, lm, true);
                15
            }
            0x06 => {
                self.nclip();
                8
            }
            0x0c => {
                self.op(shift, lm);
                6
            }
            0x10 => {
                self.dpc(self.data[RGBC], shift, lm);
                8
            }
            0x11 => {
                let mac = [9, 10, 11].map(|i| i64::from(self.ir(i)) << 12);
                self.interpolate(mac, shift, lm);
                8
            }
            0x12 => {
                self.mvmva(op, shift, lm);
                8
            }
            0x13 => {
                self.normal_colour(0, shift, lm, Light::Depth);
                19
            }
            0x14 => {
                self.colour(shift, lm, Light::Depth);
                13
            }
            0x16 => {
                for v in 0..3 {
                    self.normal_colour(v, shift, lm, Light::Depth);
                }
                44
            }
            0x1b => {
                self.normal_colour(0, shift, lm, Light::Colour);
                17
            }
            0x1c => {
                self.colour(shift, lm, Light::Colour);
                11
            }
            0x1e => {
                self.normal_colour(0, shift, lm, Light::Plain);
                14
            }
            0x20 => {
                for v in 0..3 {
                    self.normal_colour(v, shift, lm, Light::Plain);
                }
                30
            }
            0x28 => {
                for i in 1..4 {
                    let ir = i64::from(self.ir(8 + i));
                    self.set_mac_ir(i, ir * ir, shift, lm);
                }
                5
            }
            0x29 => {
                let rgbc = self.data[RGBC];
                let mac = [0, 1, 2].map(|i| (i64::from(rgbc >> (8 * i) & 0xff) * i64::from(self.ir(9 + i))) << 4);
                self.interpolate(mac, shift, lm);
                8
            }
            0x2a => {
                for _ in 0..3 {
                    self.dpc(self.data[RGB0], shift, lm);
                }
                17
            }
            0x2d => {
                let sum: i64 = (17..20).map(|i| i64::from(self.data[i] & 0xffff)).sum();
                self.average_z(i64::from(self.control[ZSF3] as i16) * sum);
                5
            }
            0x2e => {
                let sum: i64 = (16..20).map(|i| i64::from(self.data[i] & 0xffff)).sum();
                self.average_z(i64::from(self.control[ZSF4] as i16) * sum);
                6
            }
            0x30 => {
                for v in 0..3 {
                    self.rtp(v, shift, lm, v == 2);
                }
                23
            }
            0x3d => {
                let ir0 = i64::from(self.ir(IR0));
                for i in 1..4 {
                    self.set_mac_ir(i, ir0 * i64::from(self.ir(8 + i)), shift, lm);
                }
                self.push_colour();
                5
            }
            0x3e => {
                let ir0 = i64::from(self.ir(IR0));
                for i in 1..4 {
                    let mac = self.check_mac(i, i64::from(self.data[MAC0 + i] as i32) << shift);
                    self.set_mac_ir(i, ir0 * i64::from(self.ir(8 + i)) + mac, shift, lm);
                }
                self.push_colour();
                5
            }
            0x3f => {
                for v in 0..3 {
                    self.normal_colour(v, shift, lm, Light::Colour);
                }
                39
            }
            _ => 2,
        };
        self.update_error();
        cycles
    }

    fn flag(&mut self, bit: u32) {
        self.control[FLAG] |= 1 << bit;
    }

    /// Bit 31 is up when any of bits 30-23 or 18-13 is.
    fn update_error(&mut self) {
        let f = self.control[FLAG] & 0x7fff_ffff;
        self.control[FLAG] = f | u32::from(f & 0x7f87_e000 != 0) << 31;
    }

    fn ir(&self, n: usize) -> i32 {
        i32::from(self.data[n] as i16)
    }

    /// A vector of the data registers: V0-V2, or 3 for IR1-3.
    fn vector(&self, v: usize) -> [i64; 3] {
        if v == 3 {
            return [9, 10, 11].map(|i| i64::from(self.ir(i)));
        }
        let (xy, z) = (self.data[VXY0 + 2 * v], self.data[VXY0 + 2 * v + 1]);
        [xy as i16, (xy >> 16) as i16, z as i16].map(i64::from)
    }

    /// A matrix of the control registers, from its first word.
    fn matrix(&self, at: usize) -> Matrix {
        let half = |i: usize| {
            let word = self.control[at + i / 2];
            i64::from(if i & 1 == 0 { word as i16 } else { (word >> 16) as i16 })
        };
        [[half(0), half(1), half(2)], [half(3), half(4), half(5)], [half(6), half(7), half(8)]]
    }

    fn translation(&self, at: usize) -> [i64; 3] {
        [0, 1, 2].map(|i| i64::from(self.control[at + i] as i32))
    }

    /// Notes a MAC1-3 sum outside 44 bits, and wraps it to them.
    fn check_mac(&mut self, i: usize, value: i64) -> i64 {
        if value >= 1 << 43 {
            self.flag(31 - i as u32);
        } else if value < -(1 << 43) {
            self.flag(28 - i as u32);
        }
        value << 20 >> 20
    }

    fn set_mac(&mut self, i: usize, value: i64, shift: u32) {
        self.check_mac(i, value);
        self.data[MAC0 + i] = (value >> shift) as u32;
    }

    /// IR1-3 saturated to -8000h (0 with lm) to 7FFFh.
    fn set_ir(&mut self, i: usize, value: i32, lm: bool) {
        let min = if lm { 0 } else { -0x8000 };
        let clamped = value.clamp(min, 0x7fff);
        if clamped != value {
            self.flag(25 - i as u32);
        }
        self.data[8 + i] = clamped as u32;
    }

    fn set_mac_ir(&mut self, i: usize, value: i64, shift: u32, lm: bool) {
        self.set_mac(i, value, shift);
        self.set_ir(i, self.data[MAC0 + i] as i32, lm);
    }

    fn set_mac0(&mut self, value: i64) {
        if value > i64::from(i32::MAX) {
            self.flag(16);
        } else if value < i64::from(i32::MIN) {
            self.flag(15);
        }
        self.data[MAC0] = value as u32;
    }

    /// T SHL 12 + M * V into MAC1-3, each sum checked as it is made.
    fn transform(&mut self, m: &Matrix, t: [i64; 3], v: [i64; 3], shift: u32, lm: bool) {
        for i in 0..3 {
            let mut sum = self.check_mac(i + 1, (t[i] << 12) + m[i][0] * v[0]);
            sum = self.check_mac(i + 1, sum + m[i][1] * v[1]);
            self.set_mac_ir(i + 1, sum + m[i][2] * v[2], shift, lm);
        }
    }

    /// RTPS and RTPT: a vertex rotated, moved and projected onto the
    /// screen, with its depth; the last one also gives the depth cue.
    fn rtp(&mut self, v: usize, shift: u32, lm: bool, last: bool) {
        let (m, t, vec) = (self.matrix(RT), self.translation(TR), self.vector(v));
        let mut z = 0;
        for i in 0..3 {
            let mut sum = self.check_mac(i + 1, (t[i] << 12) + m[i][0] * vec[0]);
            sum = self.check_mac(i + 1, sum + m[i][1] * vec[1]);
            sum = self.check_mac(i + 1, sum + m[i][2] * vec[2]);
            self.set_mac(i + 1, sum, shift);
            z = sum;
        }
        self.set_ir(1, self.data[MAC0 + 1] as i32, lm);
        self.set_ir(2, self.data[MAC0 + 2] as i32, lm);
        // IR3 saturates on MAC3, but with sf=0 its flag looks at MAC3 SAR 12
        // (and its signed range, lm or not).
        let mac3 = self.data[MAC0 + 3] as i32;
        let min = if lm { 0 } else { -0x8000 };
        self.data[11] = mac3.clamp(min, 0x7fff) as u32;
        let (seen, flag_min) = if shift == 0 { ((z >> 12) as i32, -0x8000) } else { (mac3, min) };
        if seen != seen.clamp(flag_min, 0x7fff) {
            self.flag(22);
        }
        // The depth FIFO.
        for i in SZ0..SZ3 {
            self.data[i] = self.data[i + 1];
        }
        let sz = z >> 12;
        let sz3 = sz.clamp(0, 0xffff);
        if sz3 != sz {
            self.flag(18);
        }
        self.data[SZ3] = sz3 as u32;
        let n = self.divide();
        let (ofx, ofy) = (i64::from(self.control[OFX] as i32), i64::from(self.control[OFY] as i32));
        let sx = n * i64::from(self.ir(9)) + ofx;
        let sy = n * i64::from(self.ir(10)) + ofy;
        self.set_mac0(sx);
        self.set_mac0(sy);
        let screen = |gte: &mut Gte, value: i64, bit: u32| {
            let v = (value >> 16) as i32;
            let c = v.clamp(-0x400, 0x3ff);
            if c != v {
                gte.flag(bit);
            }
            c as u32 & 0xffff
        };
        let (x, y) = (screen(self, sx, 14), screen(self, sy, 13));
        self.data[SXY0] = self.data[SXY0 + 1];
        self.data[SXY0 + 1] = self.data[SXY2];
        self.data[SXY2] = x | y << 16;
        if last {
            let dq = n * i64::from(self.control[DQA] as i16) + i64::from(self.control[DQB] as i32);
            self.set_mac0(dq);
            self.set_ir0(dq >> 12);
        }
    }

    fn set_ir0(&mut self, value: i64) {
        let c = value.clamp(0, 0x1000);
        if c != value {
            self.flag(12);
        }
        self.data[IR0] = c as u32;
    }

    /// H / SZ3, as the GTE works it out: from a table of reciprocals and
    /// a step of Newton-Raphson, to 17 bits; 1FFFFh (and FLAG bit 17)
    /// when it would not fit.
    fn divide(&mut self) -> i64 {
        let h = u64::from(self.control[H] & 0xffff);
        let sz3 = u64::from(self.data[SZ3] & 0xffff);
        if h >= sz3 * 2 {
            self.flag(17);
            return 0x1ffff;
        }
        let z = (sz3 as u16).leading_zeros();
        let n = h << z;
        let d = sz3 << z;
        let u = u64::from(UNR[((d - 0x7fc0) >> 7) as usize]) + 0x101;
        let d = (0x200_0080 - d * u) >> 8;
        let d = (0x80 + d * u) >> 8;
        ((n * d + 0x8000) >> 16).min(0x1ffff) as i64
    }

    /// NCLIP: the screen triangle's doubled area, its sign the winding.
    fn nclip(&mut self) {
        let xy = |i: usize| {
            let w = self.data[SXY0 + i];
            (i64::from(w as i16), i64::from((w >> 16) as i16))
        };
        let ((x0, y0), (x1, y1), (x2, y2)) = (xy(0), xy(1), xy(2));
        self.set_mac0(x0 * y1 + x1 * y2 + x2 * y0 - x0 * y2 - x1 * y0 - x2 * y1);
    }

    /// OP: the cross product of IR1-3 and the rotation matrix's diagonal.
    fn op(&mut self, shift: u32, lm: bool) {
        let m = self.matrix(RT);
        let (d1, d2, d3) = (m[0][0], m[1][1], m[2][2]);
        let [ir1, ir2, ir3] = self.vector(3);
        self.set_mac_ir(1, ir3 * d2 - ir2 * d3, shift, lm);
        self.set_mac_ir(2, ir1 * d3 - ir3 * d1, shift, lm);
        self.set_mac_ir(3, ir2 * d1 - ir1 * d2, shift, lm);
    }

    /// MVMVA: a matrix times a vector plus a translation, each picked by
    /// the command. The far colour as translation is buggy: only its first
    /// column's sum sets flags, the result leaves both out.
    fn mvmva(&mut self, op: u32, shift: u32, lm: bool) {
        let m = match op >> 17 & 3 {
            0 => self.matrix(RT),
            1 => self.matrix(LLM),
            2 => self.matrix(LCM),
            _ => {
                let r = i64::from(((self.data[RGBC] & 0xff) << 4) as i16);
                let rt = self.matrix(RT);
                [[-r, r, i64::from(self.ir(IR0))], [rt[0][2]; 3], [rt[1][1]; 3]]
            }
        };
        let v = self.vector((op >> 15 & 3) as usize);
        match op >> 13 & 3 {
            2 => {
                let t = self.translation(FC);
                for i in 0..3 {
                    let first = self.check_mac(i + 1, (t[i] << 12) + m[i][0] * v[0]);
                    self.set_ir(i + 1, (first >> shift) as i32, false);
                    let sum = self.check_mac(i + 1, m[i][1] * v[1]);
                    self.set_mac_ir(i + 1, sum + m[i][2] * v[2], shift, lm);
                }
            }
            tv => {
                let t = match tv {
                    0 => self.translation(TR),
                    1 => self.translation(BK),
                    _ => [0; 3],
                };
                self.transform(&m, t, v, shift, lm);
            }
        }
    }

    /// The colour FIFO takes MAC1-3 / 16, saturated, with RGBC's code.
    fn push_colour(&mut self) {
        let mut rgb = self.data[RGBC] & 0xff00_0000;
        for i in 0..3 {
            let v = (self.data[MAC0 + 1 + i] as i32) >> 4;
            let c = v.clamp(0, 0xff);
            if c != v {
                self.flag(21 - i as u32);
            }
            rgb |= (c as u32) << (8 * i);
        }
        self.data[RGB0] = self.data[RGB0 + 1];
        self.data[RGB0 + 1] = self.data[RGB2];
        self.data[RGB2] = rgb;
    }

    /// Moves MAC1-3 towards the far colour by IR0, and pushes the colour.
    fn interpolate(&mut self, mac: [i64; 3], shift: u32, lm: bool) {
        let fc = self.translation(FC);
        for i in 0..3 {
            self.set_mac_ir(i + 1, (fc[i] << 12) - mac[i], shift, false);
        }
        let ir0 = i64::from(self.ir(IR0));
        for (i, mac) in mac.into_iter().enumerate() {
            let ir = i64::from(self.ir(9 + i));
            self.set_mac_ir(i + 1, ir * ir0 + mac, shift, lm);
        }
        self.push_colour();
    }

    /// DPCS (and each of DPCT's): a colour depth-cued towards the far
    /// colour.
    fn dpc(&mut self, rgb: u32, shift: u32, lm: bool) {
        let mac = [0, 1, 2].map(|i| i64::from(rgb >> (8 * i) & 0xff) << 16);
        self.interpolate(mac, shift, lm);
    }

    /// NCS, NCCS, NCDS and their triple forms: a normal lit by the light
    /// matrix, coloured by the light colour matrix, then as `colour`.
    fn normal_colour(&mut self, v: usize, shift: u32, lm: bool, light: Light) {
        let (llm, normal) = (self.matrix(LLM), self.vector(v));
        self.transform(&llm, [0; 3], normal, shift, lm);
        self.colour(shift, lm, light);
    }

    /// CC and CDP (and the end of the normal colour commands): the
    /// background colour plus the light colour matrix times IR1-3, then
    /// times RGBC's colour (and depth-cued for CDP and NCDS).
    fn colour(&mut self, shift: u32, lm: bool, light: Light) {
        let (lcm, bk, ir) = (self.matrix(LCM), self.translation(BK), self.vector(3));
        self.transform(&lcm, bk, ir, shift, lm);
        if light == Light::Plain {
            return self.push_colour();
        }
        let rgbc = self.data[RGBC];
        let mac = [0, 1, 2].map(|i| (i64::from(rgbc >> (8 * i) & 0xff) * i64::from(self.ir(9 + i))) << 4);
        if light == Light::Depth {
            return self.interpolate(mac, shift, lm);
        }
        for (i, mac) in mac.into_iter().enumerate() {
            self.set_mac_ir(i + 1, mac, shift, lm);
        }
        self.push_colour();
    }

    /// AVSZ3 and AVSZ4: the depths' weighted sum, as the ordering table's
    /// index.
    fn average_z(&mut self, sum: i64) {
        self.set_mac0(sum);
        let otz = sum >> 12;
        let c = otz.clamp(0, 0xffff);
        if c != otz {
            self.flag(18);
        }
        self.data[OTZ] = c as u32;
    }
}

/// What a lighting command does with the light's colour.
#[derive(Clone, Copy, PartialEq)]
enum Light {
    /// NCS: the light's colour as it is.
    Plain,
    /// NCCS, CC: times RGBC's colour.
    Colour,
    /// NCDS, CDP: times RGBC's colour, depth-cued.
    Depth,
}
