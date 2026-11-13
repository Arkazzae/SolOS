//! S2DEX's G_BG_COPY: a background copied to the screen in copy mode, as the microcode's own steps: the image
//! is loaded as many lines at a time as TMEM holds (by LOAD_BLOCK or LOAD_TILE, as the uObjBg says), each load
//! drawn as a texture rectangle, the frame clipped to the scissor first. Where the image's bottom falls in a
//! load, its last lines and the first of its top are loaded in two halves of TMEM.
//!
//! DMEM: the uObjBg at 0x3b0.

use super::super::Gfx;
use super::*;

impl N64 {
    /// G_BG_COPY (0x0a): the uObjBg at w1.
    pub(super) fn bg_copy(&mut self, gfx: &mut Gfx, w1: u32) {
        gfx.s2dex_shared();
        let t8 = gfx.address(w1);
        self.s2dex_dma(gfx, 0x3b0, t8, 39);
        let segments = gfx.segments;
        let x = &mut gfx.s2;
        let s8 = 0x1c0u32;
        // 0x1220-0x1264
        x.llv(2, 4, s8 + 68);
        x.llv(2, 12, s8 + 72);
        x.lqv(8, 0, 0x3b0);
        let flip = u32::from(x.byte(0x3cb));
        x.vmudm(11, 8, 30, E6);
        x.vsub(9, 8, 2, ALL);
        x.vmadn(10, 1, 8, H2);
        x.vadd(6, 0, 8, H1);
        x.vand(11, 11, 28, E0);
        x.vlt(9, 0, 9, H2);
        x.vge(10, 0, 10, H3);
        x.vsub(3, 8, 9, ALL);
        x.vsub(11, 11, 9, ALL);
        x.vmadn(5, 6, 31, E3);
        x.vadd(7, 8, 9, ALL);
        x.vmadn(7, 10, 31, E3);
        x.vge(27, 0, 7, ALL);
        // 0x1268-0x1284: nothing left of the frame within the scissor.
        let mut t3 = x.cfc2(1);
        x.vadd(4, 11, 10, ALL);
        if flip == 0 {
            x.vmov(4, 8, 11, E0);
        }
        t3 &= 0x88;
        x.vge(27, 5, 29, E3);
        x.vmrg(5, 5, 11, ALL);
        if t3 != 0 {
            return;
        }
        // 0x1288-0x12e4: SET_TILE of the load tile (its line for LOAD_TILE) and SET_TILE_SIZE, then SET_TILE of the
        // render tile.
        let mut v0 = u32::from(x.half(0x3cc)) << 9;
        let a1 = x.byte(0x3c4) as i8 as i32 as u32;
        x.out_sw(0, 0x3510_0000 | v0 & a1);
        x.out_store(4, 29, 8, 4);
        x.out_store(4, 29, 4, 8);
        x.out_sw(0xc, 0);
        let (format, size, palette) = (u32::from(x.byte(0x3c6)), u32::from(x.byte(0x3c7)), u32::from(x.byte(0x3c9)));
        let a0 = x.word(0x264) | palette << 20;
        x.out_sw(0x10, (format << 2 | size) << 19 | v0);
        x.out_store(1, 29, 12, 0x10);
        x.out_sw(0x14, a0);
        x.advance(24);
        // 0x12e8-0x1350: the texel size's table; the first line's address.
        x.ldv(2, 0, (size << 3).wrapping_add(628));
        x.lsv(6, 2, 0x3b0 + 36);
        x.vmudm(5, 5, 31, E4);
        x.vand(8, 2, 4, E0);
        x.vmadn(10, 1, 7, E3);
        x.vmudl(9, 2, 4, E0);
        x.vmadn(11, 6, 5, E4);
        x.vmadh(12, 0, 29, E3);
        if flip != 0 {
            x.vsub(8, 1, 10, ALL);
        }
        x.vmudl(10, 2, 10, E0);
        x.vmudm(8, 8, 31, E6);
        x.vmadn(8, 31, 29, E3);
        x.vmov(8, 9, 0, E0);
        x.ssv(11, 2, s8 + 2);
        x.ssv(12, 2, s8);
        v0 = x.word(s8) >> 1;
        let image = x.word(0x3c0);
        let image = segments[(image >> 24 & 15) as usize].wrapping_add(image & 0x00ff_ffff);
        v0 = (v0 << 3).wrapping_add(image);
        let v1 = image;
        x.vadd(2, 3, 7, E3);
        x.vmadn(2, 1, 31, E3);
        // 0x1354-0x13e4
        let a1 = u32::from(x.half(0x3ba)) & 0xfffc;
        let a2 = x.mfc2(5, 8) << 2;
        let mut a3 = x.mfc2(9, 2);
        let t0 = x.mfc2(3, 4) << 12;
        let mut t1 = x.mfc2(3, 12);
        let mut t2 = x.mfc2(2, 4) << 12;
        let mut at = x.mfc2(7, 14);
        let mut a0 = a1.wrapping_sub(a2);
        if (a0 as i32) <= 0 {
            return;
        }
        if (a3 as i32) > 0 {
            a0 = a0.wrapping_sub(4);
        }
        t2 |= 0xe400_0000;
        if (a0.wrapping_sub(at) as i32) > 0 {
            a0 = at;
        }
        let load = x.byte(0x3c5) as i8 as i32 as u32;
        let t5 = u32::from(x.half(0x3d4));
        let a2 = u32::from(x.half(0x3d2));
        let mut a1 = u32::from(x.half(0x3d0));
        if (load as i32) < 0 {
            a1 = (x.mfc2(10, 4) << 2).wrapping_sub(1);
        }
        let t6 = 0x3d10_0000 | (t5 << 1).wrapping_sub(1);
        let t7 = load << 24;
        a1 = (a1 | 0x7000) << 12;
        let mut first = (a0 as i32) > 0;
        at = at.wrapping_sub(a0);
        // A load at least a line each time (but for what would hang the microcode: a TMEM height of 0), at most
        // a line's height in 16 bits.
        for _ in 0..0x1_0000 {
            if first {
                // 0x13e8-0x1400: a load's lines and bytes (and for LOAD_TILE its width).
                let mut sp = u32::from(x.half(0x3d6));
                let mut s5 = u32::from(x.half(0x3ce));
                if (t7 as i32) < 0 {
                    sp = sp << 16 | u32::from(x.half(0x3d0));
                }
                let mut s0 = a1 | a2;
                for _ in 0..0x1_0000 {
                    // 0x1404-0x1448: the last load of fewer lines.
                    a0 = a0.wrapping_sub(s5);
                    x.mtc2(2, 2, a0);
                    if (a0 as i32) < 0 {
                        x.vmudn(2, 6, 2, E1);
                        x.vmadh(3, 0, 29, E3);
                        x.ssv(2, 2, s8 + 2);
                        x.ssv(3, 2, s8);
                        t3 = x.word(s8);
                        s5 = s5.wrapping_add(a0);
                        sp = sp.wrapping_add(t3);
                        s0 = if (t7 as i32) < 0 {
                            a1 | s5.wrapping_sub(1)
                        } else {
                            ((sp.wrapping_sub(2) | 0xe000) << 11) | a2
                        };
                    }
                    // 0x144c-0x1494: a load sync, SET_TEXTURE_IMAGE, the load, a pipe sync, TEXTURE_RECTANGLE.
                    let gp = t1.wrapping_add(s5).wrapping_sub(1);
                    x.out_store(1, 29, 14, 0);
                    x.out_sw(8, t6);
                    x.out_sw(0xc, v0);
                    x.out_sw(0x10, t7);
                    x.out_sw(0x14, s0);
                    x.out_store(1, 29, 8, 24);
                    x.out_sw(0x20, t2 | gp);
                    x.out_sw(0x24, t0 | t1);
                    x.out_store(4, 8, 0, 40);
                    x.out_store(4, 30, 8, 44);
                    x.advance(48);
                    self.s2dex_emit_from(x);
                    t1 = gp.wrapping_add(1);
                    v0 = v0.wrapping_add(sp);
                    if (a0 as i32) <= 0 {
                        break;
                    }
                }
                if (at as i32) <= 0 {
                    return;
                }
            }
            // 0x149c-0x1540: the image's bottom: its last line and its top's first, each half of TMEM.
            x.out_store(1, 29, 14, 0);
            x.out_sw(8, t6);
            if (a3 as i32) > 0 {
                x.out_sw(0xc, v0);
                x.out_sw(0x10, 0x3510_0000);
                x.out_sw(0x14, 0x0600_0000);
                x.out_sw(0x18, 0x3300_0000);
                a3 >>= 1;
                let s5 = (t5 >> 1).wrapping_sub(a3);
                x.out_sw(0x1c, (s5 << 14).wrapping_sub(4096) | 0x0600_0000);
                x.out_store(1, 29, 8, 32);
                x.out_sw(0x28, t6);
                x.out_sw(0x2c, v1);
                x.out_sw(0x30, 0x3510_0000);
                x.out_sh(0x32, s5);
                x.out_sw(0x34, 0x0600_0000);
                x.out_sw(0x38, 0x3300_0000);
                x.out_sw(0x3c, (a3 << 14).wrapping_sub(4096) | 0x0600_0000);
                x.advance(88);
                x.out_store(1, 29, 8, 0u32.wrapping_sub(24));
                x.out_sw(0u32.wrapping_sub(16), t2 | t1);
                x.out_sw(0u32.wrapping_sub(12), t0 | t1);
                x.out_store(4, 8, 0, 0u32.wrapping_sub(8));
                x.out_store(4, 30, 8, 0u32.wrapping_sub(4));
                self.s2dex_emit_from(x);
                t1 = t1.wrapping_add(4);
                at = at.wrapping_sub(4);
                if (at as i32) <= 0 {
                    return;
                }
            }
            // 0x1544-0x1554: on from the image's top.
            v0 = v1.wrapping_add(a3 << 3);
            a0 = at;
            at = 0;
            first = true;
        }
    }
}
