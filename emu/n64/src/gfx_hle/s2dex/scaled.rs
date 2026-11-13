//! S2DEX's G_BG_1CYC: a background scaled to the screen in one cycle mode, as the microcode's own steps. The
//! image is loaded a strip of lines at a time into TMEM (wrapping round its bottom to its top as it scrolls),
//! each strip drawn as a texture rectangle; the edges are clipped to the scissor first.
//!
//! DMEM: the uObjScaleBg at 0x3b0; the microcode's work from 0x3d8 on (the strips' place, sizes and steps,
//! SET_TEXTURE_IMAGE's first word at 0x3f8, SET_TILE's at 0x3fc).

use super::super::Gfx;
use super::*;

/// What the microcode's load routines read: the image's address (its s1), the lines (s2), the width (s3)
/// and TMEM (s4).
#[derive(Clone, Copy, Default)]
struct Load {
    s1: u32,
    s2: u32,
    s3: u32,
    s4: u32,
}

impl S2dex {
    /// The routine that sets up the load tile (a tile sync, SET_TILE of tile 7 at TMEM s4), then `load_image`.
    fn load_tile(&mut self, r: Load) {
        let t3 = self.word(0x3fc);
        self.out_store(1, 29, 15, 0);
        self.advance(16);
        self.out_sw(0u32.wrapping_sub(8), t3 | r.s4);
        self.out_store(4, 29, 8, 0u32.wrapping_sub(4));
        self.load_image(r);
    }

    /// The routine that loads lines of the image: SET_TEXTURE_IMAGE (at s1), a load sync, LOAD_TILE of tile 7
    /// (s3 wide, s2 lines).
    fn load_image(&mut self, r: Load) {
        let t3 = self.word(0x3f8);
        self.out_sw(4, r.s1);
        self.advance(24);
        self.out_sw(0u32.wrapping_sub(24), t3);
        self.out_store(1, 29, 14, 0u32.wrapping_sub(16));
        self.out_sw(0u32.wrapping_sub(8), 0xf400_0000);
        self.out_sh(0u32.wrapping_sub(4), r.s3.wrapping_add(0x6ff));
        self.out_sh(0u32.wrapping_sub(2), (r.s2 << 2).wrapping_sub(1));
    }
}

/// Where the loop goes next (the microcode's labels, by their address).
#[derive(Clone, Copy)]
enum Step {
    /// 0x1cf8: the next strip's lines, by the scale's sum.
    Strip,
    /// 0x1d10: back to the image's top.
    Wrap,
    /// 0x1d4c: the lines loaded for the strip.
    Lines,
    /// 0x1a4c: the lines below the image's bottom first.
    Split,
    /// 0x1a90: then those from its top.
    Second,
    /// 0x1b08
    Last,
    /// 0x1b40: past the image's bottom, back to its top.
    Rows,
    /// 0x1d98: the strip's rectangle.
    Rectangle,
    /// 0x1dd8
    Next,
}

impl N64 {
    /// G_BG_1CYC (0x09): the uObjScaleBg at w1.
    pub(super) fn bg_1cyc(&mut self, gfx: &mut Gfx, w1: u32) {
        gfx.s2dex_shared();
        let t8 = gfx.address(w1);
        self.s2dex_dma(gfx, 0x3b0, t8, 39);
        let segments = gfx.segments;
        let x = &mut gfx.s2;
        let (at, s8) = (0x3d8u32, 0x1c0u32);
        // What the DMA's wait leaves in t3.
        let mut t3 = 0u32;
        // 0x16fc-0x1748
        x.llv(2, 4, s8 + 68);
        x.llv(2, 12, s8 + 72);
        x.lqv(3, 0, 0x3b0);
        x.lsv(4, 0, 0x3b0 + 28);
        x.lsv(4, 8, 0x3b0 + 30);
        x.vmudm(5, 4, 31, E1);
        x.vmadn(6, 31, 29, E3);
        x.vrcp(8, 8, 4, E0);
        x.vrcph(7, 8, 29, E3);
        x.vrcp(8, 12, 4, E4);
        x.vrcph(7, 12, 29, E3);
        x.vmudn(27, 1, 29, E1);
        x.vmadl(27, 8, 31, E0);
        x.vmadm(7, 7, 31, E0);
        x.vmadn(8, 31, 29, E3);
        x.vmudn(27, 3, 31, E4);
        x.vmadh(10, 31, 29, E3);
        let flip = u32::from(x.byte(0x3cb)) & 1;
        // 0x174c-0x1780
        x.vmudn(27, 1, 31, E3);
        x.vmadl(27, 8, 3, H1);
        x.vmadm(12, 7, 3, H1);
        x.vmadn(11, 31, 29, E3);
        x.vand(11, 11, 28, E0);
        x.vsubc(11, 3, 11, H0);
        x.vsub(12, 0, 12, H0);
        x.vge(12, 12, 29, E3);
        x.vmrg(11, 11, 29, E3);
        x.vsub(13, 3, 11, H3);
        if flip != 0 {
            x.vadd(27, 3, 11, E3);
            x.vmov(3, 10, 27, E2);
        }
        // 0x1784-0x17b4: the microcode's test of an empty frame is CTC2, not CFC2: it writes VCC from t3 (0)
        // and never ends the command here.
        x.vsub(9, 2, 3, ALL);
        x.vsub(11, 3, 2, H3);
        x.vmadn(11, 1, 13, H3);
        x.vge(9, 9, 29, E3);
        x.vge(11, 11, 29, E3);
        x.vsub(13, 13, 9, H2);
        x.vsub(13, 13, 11, H2);
        x.vadd(2, 3, 9, H2);
        x.vge(27, 13, 29, E1);
        x.ctc2(1, t3);
        t3 &= 0x88;
        x.vadd(12, 2, 13, H3);
        if t3 != 0 {
            return;
        }
        // 0x17b8-0x17d0
        x.vmov(2, 15, 13, E7);
        x.vmudm(27, 2, 31, E4);
        x.ssv(2, 4, at);
        x.ssv(12, 4, at + 2);
        x.slv(27, 12, at + 4);
        if flip != 0 {
            x.vmov(9, 10, 11, E2);
        }
        // 0x17d4-0x17fc
        x.vmudn(2, 3, 31, E6);
        x.vmudn(12, 4, 9, H2);
        x.vmadh(11, 31, 29, E3);
        x.vmudn(12, 12, 30, E7);
        x.vmadh(11, 11, 30, E7);
        x.vmadh(11, 1, 3, H0);
        let mut v1 = x.mfc2(11, 0);
        let a1 = x.mfc2(2, 2);
        let mut a0 = x.mfc2(11, 8);
        let mut a3 = x.word(0x3d0);
        let a2 = x.mfc2(2, 10);
        // 0x1800-0x1830: the image's x and y within it (a line on for each width x passes).
        for _ in 0..0x1_0000 {
            t3 = v1.wrapping_sub(a1);
            x.mtc2(11, 0, v1);
            if (t3 as i32) < 0 {
                break;
            }
            v1 = v1.wrapping_sub(a1);
            a0 = a0.wrapping_add(32);
            a3 = a3.wrapping_add(32);
        }
        for _ in 0..0x1_0000 {
            t3 = a0.wrapping_sub(a2);
            x.mtc2(11, 8, a0);
            if (t3 as i32) < 0 {
                break;
            }
            a0 = a0.wrapping_sub(a2);
            a3 = a3.wrapping_sub(a2);
        }
        // 0x1834-0x187c
        x.set_word(0x3f0, a3);
        x.set_word(0x3e4, a0.wrapping_sub(a3) << 5);
        x.vmudn(12, 4, 13, H3);
        x.vmadh(9, 31, 29, E3);
        x.vmudn(12, 12, 30, E7);
        x.vmadh(9, 9, 30, E7);
        let bilinear = (u32::from(x.byte(0x268)) & 8) >> 3;
        let mut a0 = x.mfc2(9, 0);
        x.set_byte(0x3e0, bilinear as u8);
        a0 = a0.wrapping_add(11);
        let t4 = v1.wrapping_add(a0);
        x.set_byte(0x3e1, u8::from((t4 as i32) >= (a1 as i32)));
        // 0x1880-0x18d4
        t3 = (u32::from(x.byte(0x3c6)) << 1).wrapping_add(0x2a4);
        let t4 = (u32::from(x.byte(0x3c7)) << 2).wrapping_add(0x294);
        x.lsv(12, 4, t3);
        x.llv(12, 0, t4);
        x.mtc2(13, 0, bilinear);
        x.vmudn(14, 4, 3, E3);
        x.vmadh(15, 31, 29, E3);
        x.vmudn(27, 13, 28, E2);
        x.vmadl(27, 14, 30, E7);
        x.vmadm(15, 15, 30, E7);
        x.vmadn(14, 31, 29, E3);
        x.vsubc(27, 14, 2, E1);
        x.vlt(27, 15, 29, E3);
        x.vmrg(27, 14, 2, E1);
        x.vmadn(3, 1, 12, E0);
        x.vmudh(27, 1, 29, E1);
        x.vmadm(3, 3, 12, E1);
        x.ssv(3, 0, at + 10);
        // 0x18d8-0x1924
        x.vrcp(15, 8, 3, E0);
        x.vrcph(14, 8, 29, E3);
        x.vmudh(27, 13, 31, E3);
        x.vmadl(27, 15, 12, E2);
        x.vmadm(16, 14, 12, E2);
        x.vmudn(13, 16, 28, E1);
        x.vmadh(3, 31, 29, E3);
        x.vmudl(27, 13, 8, E4);
        x.vmadm(27, 3, 8, E4);
        x.vmadn(13, 13, 7, E4);
        x.vmadh(3, 3, 7, E4);
        x.ssv(3, 0, at + 16);
        x.ssv(13, 0, at + 18);
        x.lsv(14, 0, at + 14);
        x.lsv(15, 0, at + 12);
        x.vmudl(27, 14, 8, E4);
        x.vmadm(27, 15, 8, E4);
        x.vmadn(14, 14, 7, E4);
        x.vmadh(15, 15, 7, E4);
        x.vand(14, 14, 28, E4);
        // 0x1928-0x1970
        x.vrcph(27, 8, 3, E0);
        x.vrcpl(7, 8, 13, E0);
        x.vrcph(8, 8, 29, E3);
        x.vmudl(27, 14, 7, ALL);
        x.vmadm(27, 15, 7, ALL);
        x.vmadn(17, 14, 8, ALL);
        x.vmadh(18, 15, 8, ALL);
        x.vmudn(17, 17, 31, E2);
        x.vmadh(18, 18, 31, E2);
        x.vmadh(17, 1, 29, E1);
        x.vmudn(7, 13, 17, ALL);
        x.vmadh(8, 3, 17, ALL);
        x.vsubc(27, 14, 7, ALL);
        x.vge(27, 15, 8, ALL);
        x.vmrg(18, 17, 18, ALL);
        x.vmudn(7, 13, 18, E0);
        x.vmadh(8, 3, 18, E0);
        x.vand(17, 7, 28, E4);
        x.vsubc(19, 14, 17, ALL);
        // 0x1974-0x19c8
        x.vsub(20, 15, 8, ALL);
        x.vmudl(27, 19, 31, E1);
        x.vmadm(15, 20, 31, E1);
        x.vmadn(14, 31, 29, E3);
        x.vmudn(14, 14, 4, E4);
        x.vmadh(15, 15, 4, E4);
        x.vmudh(27, 3, 29, E1);
        x.vand(27, 7, 28, E5);
        x.vmadn(27, 13, 29, E1);
        x.vmadn(7, 19, 31, E3);
        x.vmadh(8, 20, 31, E3);
        x.ssv(8, 0, at + 20);
        x.ssv(7, 0, at + 22);
        x.vmudm(3, 14, 31, E0);
        x.vand(3, 3, 28, E6);
        x.vmudl(14, 14, 31, E1);
        x.vmadm(15, 15, 31, E1);
        x.vmadn(14, 31, 29, E3);
        x.vsub(7, 16, 14, E0);
        x.vmov(7, 9, 16, E0);
        x.slv(7, 0, at + 40);
        // 0x1bb4-0x1bf8
        x.lsv(13, 0, at + 26);
        x.lsv(17, 0, at + 24);
        x.vmudn(27, 16, 18, ALL);
        x.vmadn(27, 14, 29, E1);
        x.vmadh(27, 15, 29, E1);
        x.vmadm(27, 17, 31, E0);
        x.vmadl(8, 13, 31, E0);
        x.vsub(13, 8, 10, E5);
        x.vadd(14, 8, 10, E5);
        x.vge(27, 8, 29, E3);
        x.vmrg(8, 8, 14, ALL);
        x.vge(27, 13, 29, E3);
        x.vmrg(8, 13, 8, ALL);
        x.vmov(11, 9, 2, E1);
        x.vmudm(10, 11, 12, E1);
        x.vmudn(10, 10, 31, E6);
        x.slv(10, 0, at + 28);
        x.ssv(8, 0, at + 54);
        // 0x1bfc-0x1c48: the image's address made physical (DMEM 0x3c0).
        let image = x.word(0x3c0);
        x.vmudn(27, 10, 29, E1);
        x.vmadn(2, 8, 10, E1);
        let t8 = segments[(image >> 24 & 15) as usize].wrapping_add(image & 0x00ff_ffff);
        x.set_word(0x3c0, t8);
        x.vmadh(13, 31, 29, E3);
        x.ssv(2, 0, s8 + 2);
        x.ssv(13, 0, s8);
        let mut a1 = x.word(s8).wrapping_add(t8);
        x.vmudm(8, 7, 10, E1);
        x.vmadn(2, 31, 29, E3);
        x.ssv(8, 0, at + 44);
        x.ssv(8, 2, at + 48);
        x.ssv(2, 0, at + 46);
        x.ssv(2, 2, at + 50);
        x.vand(2, 11, 12, E0);
        if flip != 0 {
            x.vmudn(27, 2, 31, E3);
            x.vmadn(2, 9, 31, E3);
        }
        // 0x1c4c-0x1cb8: SET_TEXTURE_IMAGE's and SET_TILE's first words; SET_TILE of the render tile (0) and its
        // size.
        t3 = (u32::from(x.half(0x3f6)) >> 1).wrapping_sub(1);
        x.set_word(0x3f8, 0xfd10_0000 | t3);
        let settile = 0xf510_0000 | u32::from(x.half(0x3e2)) << 9;
        x.set_word(0x3fc, settile);
        x.out_sw(0, settile);
        x.out_store(4, 29, 8, 4);
        x.out_sw(8, settile);
        x.out_sb(9, u32::from(x.byte(0x3c6)) << 5 | u32::from(x.byte(0x3c7)) << 3);
        x.out_sw(0xc, u32::from(x.byte(0x3c9)) << 20 | x.word(0x264));
        x.out_store(4, 29, 4, 16);
        x.out_sw(0x14, 0);
        x.advance(24);
        // 0x1cbc-0x1cf4
        let mut v1 = u32::from(x.half(0x3ba)) >> 2;
        let mut a0 = x.half(0x400) as i16 as i32 as u32;
        let mut v0 = x.word(0x3ec);
        let t7 = x.word(0x3e8);
        let mut t6 = u32::from(x.half(0x3de));
        x.set_half(0x40c, v1 as u16);
        v1 = v1.wrapping_sub(u32::from(x.half(0x40e)));
        let mut a2 = x.word(0x404);
        let a3 = u32::from(x.half(0x3d8)) << 12;
        let t0 = u32::from(x.half(0x3da)) << 12;
        let mut t1 = u32::from(x.half(0x3dc));
        let (mut t2, mut t4, mut t5, mut t8, mut t9) = (0u32, 0u32, 0u32, 0u32, 0u32);
        let mut r = Load::default();
        let mut step = Step::Strip;
        // At most a strip a line of the frame (t6, 16 bits), a few steps each; and a stop for what would hang
        // the microcode (a scale of 0).
        for _ in 0..1 << 23 {
            match step {
                Step::Strip => {
                    t5 = v0 >> 10;
                    if (t5 as i32) > 0 {
                        step = Step::Lines;
                        continue;
                    }
                    v1 = v1.wrapping_sub(a0);
                    a1 = a1.wrapping_add(a2);
                    step = if (v1 as i32) > 0 { Step::Next } else { Step::Wrap };
                }
                Step::Wrap => {
                    x.lsv(7, 0, at + 30);
                    x.mtc2(7, 2, 0u32.wrapping_sub(v1));
                    a1 = x.word(0x3c0);
                    x.vmudn(7, 7, 7, E1);
                    x.vmadh(8, 31, 29, E3);
                    x.ssv(7, 0, s8 + 2);
                    x.ssv(8, 0, s8);
                    t3 = x.word(s8);
                    t4 = u32::from(x.half(0x3f4));
                    let ra = u32::from(x.half(0x40c));
                    a1 = a1.wrapping_add(t3).wrapping_add(t4);
                    v1 = v1.wrapping_add(ra);
                    step = Step::Next;
                }
                // 0x1d4c-0x1d90, then 0x1a18-0x1a48.
                Step::Lines => {
                    t6 = t6.wrapping_sub(t5);
                    v0 &= 0x3ff;
                    if (t6 as i32) < 0 {
                        x.mtc2(27, 0, t6);
                        x.vmudn(7, 4, 27, E0);
                        x.vmadh(8, 31, 29, E3);
                        t4 = u32::from(x.half(0x402));
                        x.ssv(8, 8, s8);
                        x.ssv(7, 8, s8 + 2);
                        t3 = ((x.word(s8) as i32) >> 10) as u32;
                        a0 = a0.wrapping_add(1).wrapping_add(t3);
                        t3 = a0.wrapping_sub(t4);
                        t5 = t5.wrapping_add(t6);
                        if (t3 as i32) >= 0 {
                            a0 = t4;
                        }
                    }
                    t2 = t1.wrapping_add(t5);
                    t9 = a0.wrapping_add(u32::from(x.byte(0x3e0)));
                    t4 = u32::from(x.byte(0x3e1));
                    t8 = v1.wrapping_sub(t4);
                    t3 = t8.wrapping_sub(t9);
                    r.s3 = u32::from(x.half(0x3e2));
                    if (t3 as i32) < 0 {
                        step = Step::Split;
                        continue;
                    }
                    // The strip's lines in one load.
                    r.s2 = t9;
                    r.s1 = a1;
                    x.load_image(r);
                    v1 = v1.wrapping_sub(a0);
                    a1 = a1.wrapping_add(a2);
                    step = Step::Rectangle;
                }
                Step::Split => {
                    x.mtc2(24, 0, r.s3);
                    r.s2 = t9.wrapping_sub(v1);
                    r.s4 = v1;
                    if (r.s2 as i32) > 0 {
                        r.s1 = x.word(0x3c0).wrapping_add(u32::from(x.half(0x3f4)));
                        if r.s4 & 1 != 0 {
                            t3 = u32::from(x.half(0x3f6));
                            r.s4 = r.s4.wrapping_sub(1);
                            r.s2 = r.s2.wrapping_add(1);
                            r.s1 = r.s1.wrapping_sub(t3);
                        }
                        x.mtc2(27, 0, r.s4);
                        x.vmudn(27, 27, 24, E0);
                        r.s4 = x.mfc2(27, 0);
                        x.load_tile(r);
                    }
                    step = Step::Second;
                }
                Step::Second => {
                    x.lsv(24, 2, at + 30);
                    if t4 == 0 {
                        step = Step::Last;
                        continue;
                    }
                    x.mtc2(26, 2, t8);
                    x.vmudn(27, 26, 24, E1);
                    x.vmadh(25, 31, 29, E3);
                    x.ssv(27, 2, s8 + 2);
                    x.ssv(25, 2, s8);
                    t3 = x.word(s8);
                    r.s1 = x.word(0x3c0);
                    r.s2 = t8 & 1;
                    t4 = a1.wrapping_add(t3);
                    if r.s2 != 0 {
                        t3 = u32::from(x.half(0x3f6));
                        x.vsub(26, 26, 29, E1);
                        t4 = t4.wrapping_sub(t3);
                        r.s1 = r.s1.wrapping_sub(t3);
                    }
                    x.lsv(24, 4, at + 28);
                    r.s2 = r.s2.wrapping_add(1);
                    x.vsub(27, 24, 24, E2);
                    x.vmudm(27, 27, 30, E6);
                    x.vsub(25, 24, 27, E1);
                    r.s3 = x.mfc2(25, 0);
                    x.vmudn(25, 26, 24, E0);
                    x.vmadn(26, 27, 29, E1);
                    r.s4 = x.mfc2(26, 2);
                    x.load_tile(r);
                    r.s1 = t4;
                    r.s4 = x.mfc2(25, 2);
                    r.s3 = x.mfc2(27, 2);
                    x.load_tile(r);
                    step = Step::Last;
                }
                Step::Last => {
                    v1 = v1.wrapping_sub(a0);
                    if (t8 as i32) > 0 {
                        r.s1 = a1;
                        r.s3 = u32::from(x.half(0x3e2));
                        r.s2 = t8;
                        r.s4 = 0;
                        x.load_tile(r);
                    } else {
                        t3 = x.word(0x3fc);
                        x.advance(8);
                        x.out_sw(0u32.wrapping_sub(8), t3);
                        x.out_store(4, 29, 8, 0u32.wrapping_sub(4));
                    }
                    a1 = a1.wrapping_add(a2);
                    step = if (v1 as i32) > 0 { Step::Rectangle } else { Step::Rows };
                }
                Step::Rows => {
                    x.mtc2(27, 0, 0u32.wrapping_sub(v1));
                    t3 = u32::from(x.half(0x3f4));
                    a1 = x.word(0x3c0);
                    x.vmudn(27, 27, 24, E1);
                    x.vmadh(25, 31, 29, E3);
                    a1 = a1.wrapping_add(t3);
                    x.ssv(27, 0, s8 + 2);
                    t4 = u32::from(x.half(0x40c));
                    x.ssv(25, 0, s8);
                    t3 = x.word(s8);
                    v1 = v1.wrapping_add(t4);
                    a1 = a1.wrapping_add(t3);
                    step = Step::Rectangle;
                }
                // A pipe sync, then TEXTURE_RECTANGLE.
                Step::Rectangle => {
                    x.out_store(1, 29, 8, 0);
                    x.out_sw(8, t2 << 2 | t0);
                    x.out_store(1, 29, 13, 8);
                    x.out_sw(0xc, t1 << 2 | a3);
                    x.out_store(2, 2, 0, 16);
                    x.out_store(2, 3, 0, 18);
                    x.out_store(2, 4, 0, 20);
                    x.out_store(2, 4, 8, 22);
                    x.advance(24);
                    self.s2dex_emit_from(x);
                    t1 = t2;
                    if (t6 as i32) <= 0 {
                        return;
                    }
                    step = Step::Next;
                }
                Step::Next => {
                    v0 = v0.wrapping_add(t7);
                    a0 = x.half(0x402) as i16 as i32 as u32;
                    a2 = x.word(0x408);
                    x.vxor(3, 3, 3, ALL);
                    step = Step::Strip;
                }
            }
        }
    }
}
