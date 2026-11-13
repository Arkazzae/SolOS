//! S2DEX's objects: G_OBJ_LOADTXTR (and the commands that load a texture, then draw), G_OBJ_RECTANGLE,
//! G_OBJ_RECTANGLE_R and G_OBJ_SPRITE, as the microcode's own steps.
//!
//! DMEM: the uObjTxtr at 0x3a8, the uObjSprite at 0x3c0 (after it); the render mode's corrections at 0x1f8
//! and 0x1fc and its tables' addresses at 0x244 and 0x246; the tile last set up at 0x2ae (0x81: a load's),
//! the render tile (0 or 2, in turn) at 0x2af; the status words at 0x2b0; the object matrix at 0x1e0.

use super::super::Gfx;
use super::*;

impl N64 {
    /// G_OBJ_LOADTXTR (05), and G_OBJ_LDTX_SPRITE, _RECT and _RECT_R (06-08), which then draw the uObjSprite
    /// after the texture's structure: a uObjTxtr at w1 (TLUT, Block or Tile load, by its type's bytes), loaded
    /// unless the status word at its sid already holds its flag under its mask; the word takes it otherwise.
    /// The load sets the texture image and the load tile (7), with a tile sync before when the last tile set
    /// up was a load's, and a load sync after unless it was.
    pub(super) fn obj_load_txtr(&mut self, gfx: &mut Gfx, w0: u32, w1: u32) {
        let t8 = gfx.address(w1);
        self.s2dex_dma(gfx, 0x3a8, t8, w0 & 0xff);
        let s2 = &mut gfx.s2;
        let t3 = u32::from(s2.half(0x244));
        s2.ldv(4, 0, 0x1f8);
        let at = s2.byte(0x2ae) as i8 as i32;
        s2.lqv(3, 0, t3);
        let v1 = u32::from(s2.byte(0x3b7));
        let (mask, flag) = (s2.word(0x3bc), s2.word(0x3b8));
        let status = s2.word(0x2b0 + v1);
        if status & mask != flag {
            s2.set_word(0x2b0 + v1, status & !mask | flag & mask);
            if at == -127 {
                s2.out_store(1, 29, 15, 0);
                s2.advance(8);
            }
            s2.set_byte(0x2ae, 0x81);
            // SETTIMG (0x3d10: RGBA, 16 bits), the width from the structure's 0x0a.
            let a0 = u32::from(s2.half(0x3b2));
            s2.out_store(2, 29, 0, 0);
            s2.out_sh(2, a0);
            let image = s2.word(0x3ac);
            let tmem = u32::from(s2.half(0x3b0));
            let image = gfx.address(image);
            let s2 = &mut gfx.s2;
            s2.out_sw(4, image);
            // SETTILE (0x35) of tile 7: its line from the width where the type's second byte says (a tile).
            let line_mask = s2.byte(0x3a9) as i8 as i32 as u32;
            let size = u32::from(s2.byte(0x3aa));
            let load = u32::from(s2.byte(0x3ab));
            let line = (a0.wrapping_add(1) & line_mask) << 7;
            s2.out_sw(8, tmem | line | size << 16);
            s2.out_store(1, 29, 12, 8);
            s2.out_store(4, 29, 8, 12);
            if at >= 0 {
                s2.out_store(1, 29, 14, 16);
                s2.advance(8);
            }
            // The load (the type's last byte), up to the width and the structure's 0x0c, of tile 7.
            let th = u32::from(s2.half(0x3b4));
            s2.out_sw(0x10, load << 24);
            s2.out_sw(0x14, a0 << 14 | th);
            s2.out_store(1, 29, 8, 0x14);
            s2.advance(0x18);
            self.s2dex_emit(gfx);
        }
        match w0 >> 24 {
            0x06 => self.obj_sprite_drawn(gfx),
            0x07 => self.obj_rectangle_drawn(gfx, false),
            0x08 => self.obj_rectangle_drawn(gfx, true),
            _ => {}
        }
    }

    /// G_OBJ_RECTANGLE (01) and G_OBJ_RECTANGLE_R (0xda, through the object matrix): the uObjSprite at w1.
    pub(super) fn obj_rectangle(&mut self, gfx: &mut Gfx, w0: u32, w1: u32) {
        let t8 = gfx.address(w1);
        self.s2dex_dma(gfx, 0x3c0, t8, 23);
        let s2 = &mut gfx.s2;
        let t3 = u32::from(s2.half(0x244));
        s2.ldv(4, 0, 0x1f8);
        s2.ldv(3, 0, t3);
        self.obj_rectangle_drawn(gfx, w0 >> 24 == 0xda);
    }

    /// G_OBJ_SPRITE (02): the uObjSprite at w1, as two triangles through the object matrix.
    pub(super) fn obj_sprite(&mut self, gfx: &mut Gfx, w1: u32) {
        let t8 = gfx.address(w1);
        self.s2dex_dma(gfx, 0x3c0, t8, 23);
        let s2 = &mut gfx.s2;
        let t3 = u32::from(s2.half(0x244));
        s2.ldv(4, 0, 0x1f8);
        s2.ldv(3, 0, t3);
        self.obj_sprite_drawn(gfx);
    }

    /// The rectangle, its uObjSprite at 0x3c0, the corrections in v3 and v4.
    fn obj_rectangle_drawn(&mut self, gfx: &mut Gfx, rotated: bool) {
        gfx.s2dex_shared();
        let s2 = &mut gfx.s2;
        // 0x1bac
        s2.lqv(2, 0, 0x3c0);
        if rotated {
            // 0x1d88-0x1d94
            s2.lsv(7, 2, 0x1c0 + 52);
            s2.lsv(7, 10, 0x1c0 + 54);
            s2.lsv(8, 0, 0x1c0 + 48);
            s2.lsv(8, 8, 0x1c0 + 50);
            // 0x1d98-0x1dac
            s2.vmudm(9, 2, 31, E0);
            s2.vmadn(10, 31, 29, E3);
            s2.vrcp(12, 8, 7, E1);
            s2.vrcph(11, 8, 27, E0);
            s2.vrcp(12, 12, 7, E5);
            s2.vrcph(11, 12, 27, E0);
            // 0x1db0-0x1dc8
            s2.vmudn(27, 1, 11, ALL);
            s2.vmadn(12, 12, 29, E1);
            s2.vmadh(11, 11, 29, E1);
            s2.vmudm(7, 7, 31, E1);
            s2.vmadn(13, 31, 29, E3);
            s2.vmudn(27, 13, 2, ALL);
            s2.vmadh(7, 7, 2, ALL);
            // 0x1dcc-0x1df0
            s2.vmudl(27, 10, 12, H0);
            s2.vmadm(27, 9, 12, H0);
            s2.vmadn(6, 10, 11, H0);
            s2.vmadh(5, 9, 11, H0);
            s2.vadd(8, 8, 4, E2);
            s2.vand(8, 8, 3, E0);
            s2.vmadn(5, 5, 29, E1);
            s2.vmov(2, 9, 7, E1);
            s2.vmov(2, 13, 7, E5);
        } else {
            // 0x1bb0-0x1bb8
            s2.vadd(5, 2, 4, E2);
            s2.vand(5, 5, 3, E0);
            s2.vand(6, 0, 29, E3);
        }
        // 0x1bbc-0x1bf0
        s2.vsub(8, 2, 4, E1);
        s2.vadd(7, 0, 4, E0);
        s2.vrcp(9, 8, 2, E1);
        s2.vrcph(4, 8, 27, E0);
        s2.vrcp(9, 12, 2, E5);
        s2.vrcph(4, 12, 27, E0);
        s2.vaddc(9, 9, 4, ALL);
        s2.vadd(4, 4, 29, E3);
        s2.vmudn(11, 8, 30, E0);
        s2.vmadh(10, 0, 29, E3);
        s2.vmudl(27, 11, 9, H0);
        s2.vmadm(27, 10, 9, H0);
        s2.vmadn(11, 11, 4, H0);
        s2.vmadh(10, 10, 4, H0);
        // 0x1bf4-0x1c18
        let mut t3 = u32::from(s2.half(0x246));
        s2.ldv(4, 0, t3);
        s2.vadd(9, 7, 8, H2);
        s2.vmadn(9, 1, 31, E3);
        s2.vmudn(9, 9, 4, E0);
        s2.vmudn(12, 2, 4, E1);
        t3 = u32::from(s2.byte(0x3d7));
        t3 &= 0x11;
        t3 = t3.wrapping_add(119);
        s2.ctc2(1, t3);
        // 0x1c1c-0x1c48
        s2.vmrg(7, 7, 9, ALL);
        s2.vmrg(13, 2, 12, ALL);
        s2.vadd(7, 7, 3, E2);
        s2.vaddc(27, 6, 11, H2);
        s2.vadd(2, 5, 10, H2);
        s2.llv(8, 0, 0x1c0 + 68);
        s2.llv(8, 8, 0x1c0 + 72);
        s2.vlt(2, 2, 8, H1);
        s2.vsub(9, 5, 8, H0);
        s2.vge(5, 5, 8, H0);
        s2.vmrg(9, 0, 9, ALL);
        s2.vge(27, 5, 2, ALL);
        // 0x1c4c-0x1c60
        let mut at = s2.cfc2(1);
        at &= 0x11;
        s2.vmudh(27, 9, 13, H1);
        if at != 0 {
            return;
        }
        s2.vsar(11, 11, 11, E1);
        s2.vsar(10, 10, 10, E0);
        // 0x1c64-0x1c80
        s2.vmudl(27, 11, 30, E2);
        s2.vmadm(27, 10, 30, E2);
        s2.vmadn(11, 11, 31, E3);
        s2.vmadn(7, 1, 7, H2);
        s2.vand(8, 5, 3, E1);
        s2.vmudh(8, 8, 30, H3);
        s2.vmudm(8, 13, 8, H0);
        s2.vsub(7, 7, 8, H1);
        // 0x1c84-0x1ca8
        at = s2.mfc2(5, 0);
        let mut v0 = s2.mfc2(5, 8);
        let mut v1 = s2.mfc2(2, 0);
        let mut a0 = s2.mfc2(2, 8);
        v0 &= 0xfff;
        at <<= 12;
        at |= v0;
        a0 &= 0xfff;
        v1 <<= 12;
        v1 |= a0;
        // 0x1cac-0x1cd0
        let mut a1 = s2.mfc2(13, 2);
        v0 = u32::from(s2.byte(0xc9));
        v0 &= 0x20;
        if v0 != 0 {
            a1 <<= 2;
            v1 = v1.wrapping_add(-16388i32 as u32);
        }
        v0 = u32::from(s2.byte(0x2af));
        v0 ^= 0x2;
        s2.set_byte(0x2af, v0 as u8);
        // 0x1cd4-0x1ce4
        let t4 = s2.byte(0x2ae) as i8 as i32 as u32;
        s2.out_store(1, 29, 15, 0);
        s2.set_byte(0x2ae, v0 as u8);
        if v0 == t4 {
            s2.advance(8);
        }
        // 0x1ce8-0x1d00
        s2.slv(4, 4, 0x1c0);
        let t2 = s2.word(0x1c0);
        a0 = u32::from(s2.byte(0x3d4));
        let mut a2 = u32::from(s2.byte(0x3d5));
        let mut a3 = u32::from(s2.half(0x3d0));
        let mut t0 = u32::from(s2.half(0x3d2));
        let mut t1 = u32::from(s2.byte(0x3d6));
        // 0x1d04-0x1d30
        a0 <<= 21;
        a2 <<= 19;
        a0 |= a2;
        a3 <<= 9;
        a0 |= a3;
        a0 |= t0;
        s2.out_sw(0, a0);
        s2.out_store(1, 29, 12, 0);
        t1 <<= 20;
        t1 |= t2;
        s2.out_sw(4, t1);
        s2.out_sb(4, v0);
        // 0x1d34-0x1d58
        a3 = u32::from(s2.half(0x3c4));
        s2.out_store(4, 29, 4, 8);
        t0 = u32::from(s2.half(0x3cc));
        a3 &= 0x7ff8;
        a3 <<= 9;
        t0 = ((t0 as i32) >> 3) as u32;
        a2 = a3 | t0;
        a2 = a2.wrapping_add(-16388i32 as u32);
        s2.out_sw(0xc, a2);
        s2.out_sb(0xc, v0);
        // 0x1d5c-0x1d78
        s2.out_sw(0x10, v1);
        s2.out_store(1, 29, 13, 16);
        s2.out_sw(0x14, at);
        s2.out_sb(0x14, v0);
        s2.out_store(2, 7, 0, 24);
        s2.out_store(2, 7, 8, 26);
        s2.out_sh(0x1c, a1);
        s2.out_store(2, 13, 10, 30);
        // 0x1d80, the jump's delay slot
        s2.advance(32);
        self.s2dex_emit(gfx);
    }

    /// The sprite, its uObjSprite at 0x3c0, the corrections in v3 and v4.
    fn obj_sprite_drawn(&mut self, gfx: &mut Gfx) {
        gfx.s2dex_shared();
        let s2 = &mut gfx.s2;
        let mut t9 = 1u32;
        // 0x16e8-0x1700
        s2.lqv(2, 0, 0x3c0);
        let mut v0 = u32::from(s2.byte(0x1e8));
        let mut v1 = s2.byte(0x1ec) as i8 as i32 as u32;
        v0 ^= v1;
        v0 &= 0x180;
        v0 >>= 3;
        let mut at = v0.wrapping_add(704);
        // 0x1704-0x173c
        s2.vsub(7, 2, 4, E1);
        s2.vadd(6, 4, 3, E2);
        s2.vadd(5, 2, 4, E3);
        s2.vrcp(8, 8, 2, E1);
        s2.vrcph(4, 8, 27, E0);
        s2.vrcp(8, 12, 2, E5);
        s2.vrcph(4, 12, 27, E0);
        s2.vaddc(8, 8, 4, ALL);
        s2.vadd(4, 4, 29, E3);
        s2.vmudl(27, 7, 8, H0);
        s2.vmadm(2, 7, 4, H0);
        s2.vmadn(9, 27, 29, E3);
        s2.vmudm(27, 2, 30, E0);
        s2.vmadl(2, 9, 30, E0);
        s2.lqv(9, 0, at);
        // 0x1740-0x1760
        let mut t3 = u32::from(s2.byte(0x3d7));
        s2.ctc2(1, t3);
        s2.vmrg(10, 1, 0, ALL);
        s2.vxor(8, 9, 10, H0);
        s2.vmudh(27, 1, 6, E0);
        s2.vmadh(4, 8, 7, H2);
        s2.vmudh(27, 1, 5, H0);
        s2.vmadh(6, 9, 2, H2);
        s2.vmadh(7, 0, 29, E3);
        // 0x1764-0x177c
        s2.sqv(6, 0, 0x1c0);
        s2.ldv(6, 8, 0x1c0);
        s2.ldv(7, 0, 0x1c0 + 8);
        s2.lqv(2, 0, 0x1c0 + 32);
        s2.lsv(5, 0, 0x1c0 + 48);
        s2.lsv(5, 8, 0x1c0 + 50);
        s2.vadd(5, 5, 3, E3);
        // 0x1780-0x17a0
        s2.vand(5, 5, 3, E0);
        s2.vmudh(27, 1, 5, H0);
        s2.vmadm(27, 6, 2, H1);
        s2.vmadh(27, 6, 2, H0);
        s2.vmadm(27, 7, 2, H3);
        s2.vmadh(8, 7, 2, H2);
        s2.llv(2, 0, 0x1c0 + 68);
        s2.llv(2, 8, 0x1c0 + 72);
        s2.vge(27, 8, 2, H0);
        // 0x17a4-0x17b0
        t3 = s2.cfc2(1);
        let mut t4 = t3 & 0xf;
        let taken = t4 == 0;
        t4 = t3 & 0xf0;
        if taken {
            return;
        }
        // 0x17b4-0x17b8
        let taken = t4 == 0;
        s2.vlt(27, 8, 2, H1);
        if taken {
            return;
        }
        // 0x17bc-0x17c0
        t3 = s2.cfc2(1);
        t4 = t3 & 0xf;
        // 0x17c4-0x17c8
        let taken = t4 == 0;
        t4 = t3 & 0xf0;
        if taken {
            return;
        }
        // 0x17cc-0x17d0
        let taken = t4 == 0;
        s2.vadd(2, 8, 29, E3);
        if taken {
            return;
        }
        // 0x17d4-0x17f8
        s2.sdv(8, 8, 0x1c0);
        s2.vxor(5, 8, 8, Q0);
        s2.ldv(2, 0, 0x1c0);
        s2.vxor(6, 4, 4, Q0);
        s2.vlt(2, 2, 2, Q1);
        s2.vmrg(5, 5, 5, Q1);
        s2.vmrg(6, 6, 6, Q1);
        s2.vxor(8, 8, 5, Q0);
        s2.vxor(4, 4, 6, Q0);
        at = u32::from(s2.byte(0x2af));
        // 0x17fc-0x1814
        at ^= 0x2;
        s2.set_byte(0x2af, at as u8);
        t4 = s2.byte(0x2ae) as i8 as i32 as u32;
        s2.out_store(1, 29, 15, 0);
        let taken = at != t4;
        s2.set_byte(0x2ae, at as u8);
        if !taken {
            s2.advance(8);
        }
        // 0x1818-0x184c
        let mut a3 = u32::from(s2.half(0x246));
        v0 = u32::from(s2.byte(0x3d4));
        v1 = u32::from(s2.byte(0x3d5));
        let mut a0 = u32::from(s2.half(0x3d0));
        let mut a1 = u32::from(s2.half(0x3d2));
        let mut a2 = u32::from(s2.byte(0x3d6));
        a3 = s2.word(a3 + 4);
        v0 <<= 21;
        v1 <<= 19;
        v0 |= v1;
        a0 <<= 9;
        v0 |= a0;
        v0 |= a1;
        s2.out_sw(0, v0);
        // 0x1850-0x1890
        s2.out_store(1, 29, 12, 0);
        a2 <<= 20;
        a2 |= a3;
        s2.out_sw(4, a2);
        s2.out_sb(4, at);
        a0 = u32::from(s2.half(0x3c4));
        s2.out_store(4, 29, 4, 8);
        a1 = u32::from(s2.half(0x3cc));
        a0 &= 0x7ff8;
        a0 <<= 9;
        a1 = ((a1 as i32) >> 3) as u32;
        v1 = a0 | a1;
        v1 = v1.wrapping_add(-16388i32 as u32);
        s2.out_sw(0xc, v1);
        s2.out_sb(0xc, at);
        s2.out_store(1, 29, 8, 16);
        s2.advance(24);
        // 0x1894-0x18c4
        s2.vnor(7, 0, 30, E1);
        t3 = 32;
        s2.ctc2(1, t3);
        t4 = 231;
        s2.vmrg(2, 7, 8, H2);
        s2.vmrg(3, 7, 8, H0);
        s2.vmrg(5, 7, 8, H1);
        s2.vmrg(6, 7, 8, H3);
        s2.ctc2(1, t4);
        s2.vmrg(2, 2, 4, H2);
        s2.vmrg(3, 3, 4, H0);
        s2.vmrg(5, 5, 4, H1);
        s2.vmrg(6, 6, 4, H3);
        loop {
            // 0x18c8-0x18f0
            s2.vsub(4, 5, 2, ALL);
            s2.vsub(7, 3, 2, ALL);
            s2.vsub(8, 2, 3, ALL);
            s2.vsub(9, 5, 3, ALL);
            s2.vmudh(27, 4, 7, E6);
            s2.vmadh(27, 8, 4, E6);
            s2.vsar(10, 10, 10, E0);
            s2.vsar(11, 11, 11, E1);
            s2.vrcp(13, 8, 4, E6);
            s2.vrcph(12, 8, 29, E3);
            s2.vabs(27, 10, 10, ALL);
            s2.vrcp(13, 9, 7, E6);
            s2.vrcph(12, 9, 29, E3);
            s2.vlt(27, 27, 31, E6);
            // 0x1900-0x1924
            s2.vmrg(14, 30, 30, E0);
            s2.vrcp(13, 10, 9, E6);
            s2.vrcph(12, 10, 29, E3);
            s2.vmudl(27, 11, 14, E1);
            s2.vmadm(10, 10, 14, E1);
            s2.vmadn(11, 31, 29, E3);
            s2.vmov(9, 8, 4, E2);
            s2.vmov(9, 9, 7, E2);
            s2.vaddc(13, 13, 12, ALL);
            s2.vadd(12, 12, 29, E3);
            // 0x1928-0x1948
            s2.vmov(17, 8, 4, E6);
            s2.vmov(17, 9, 7, E6);
            s2.vmov(17, 10, 9, E6);
            s2.vadd(18, 0, 31, E2);
            s2.vmudn(13, 13, 31, E2);
            s2.vmadh(12, 12, 31, E2);
            s2.vmudl(27, 13, 17, ALL);
            s2.vmadm(16, 12, 17, ALL);
            s2.vmadn(15, 31, 29, E3);
            // 0x194c-0x196c
            s2.vsubc(15, 0, 15, ALL);
            s2.vsub(16, 18, 16, ALL);
            s2.vmudl(27, 13, 15, ALL);
            s2.vmadm(27, 12, 15, ALL);
            s2.vmadn(13, 13, 16, ALL);
            s2.vmadh(12, 12, 16, ALL);
            v0 = s2.mfc2(10, 3);
            v0 &= 0x80;
            v0 |= at;
            // 0x1970-0x198c
            v0 |= 0xa00;
            s2.vrcph(27, 8, 10, E2);
            s2.vrcpl(16, 10, 11, E2);
            s2.vrcph(15, 10, 29, E3);
            s2.vmudn(27, 13, 9, ALL);
            s2.vmadh(27, 12, 9, ALL);
            s2.vsar(10, 10, 10, E1);
            s2.vsar(11, 11, 11, E0);
            // 0x1990-0x19b8
            s2.vmudh(27, 4, 7, E6);
            s2.vmadh(27, 8, 4, E6);
            s2.vsar(12, 12, 12, E1);
            s2.vsar(13, 13, 13, E0);
            s2.vmudh(27, 7, 4, E2);
            s2.vmadh(27, 4, 8, E2);
            s2.vsar(17, 17, 17, E1);
            s2.vsar(18, 18, 18, E0);
            s2.vmudl(27, 12, 16, E2);
            s2.vmadm(27, 13, 16, E2);
            s2.vmadn(12, 12, 15, E2);
            // 0x19bc-0x19dc
            s2.vmadh(13, 13, 15, E2);
            s2.vmudl(27, 17, 16, E2);
            s2.vmadm(27, 18, 16, E2);
            s2.vmadn(17, 17, 15, E2);
            s2.vmadh(18, 18, 15, E2);
            v1 = s2.mfc2(3, 4);
            s2.vadd(15, 0, 31, E4);
            s2.vmudl(27, 10, 31, E4);
            s2.vmadm(9, 11, 31, E4);
            // 0x19e0-0x1a08
            v1 <<= 14;
            s2.vmadn(8, 0, 29, E3);
            s2.vand(4, 2, 31, E5);
            s2.vsub(4, 0, 4, ALL);
            s2.vlt(27, 11, 29, E3);
            s2.vmrg(16, 0, 29, E1);
            s2.vmudn(27, 15, 2, E2);
            s2.vmadn(8, 8, 4, E6);
            s2.vmadh(9, 9, 4, E6);
            s2.vmudn(4, 15, 4, E6);
            s2.vmadh(7, 0, 29, E3);
            // 0x1a0c-0x1a38
            s2.vaddc(19, 8, 16, E0);
            s2.vadd(20, 9, 29, E3);
            s2.vmudl(27, 12, 14, E1);
            s2.vmadm(13, 13, 14, E1);
            s2.vmadn(12, 31, 29, E3);
            s2.vmudl(27, 17, 14, E1);
            s2.vmadm(18, 18, 14, E1);
            s2.vmadn(17, 31, 29, E3);
            s2.vmudn(12, 12, 31, E6);
            s2.vmadh(13, 13, 31, E6);
            s2.vmudn(17, 17, 31, E6);
            s2.vmadh(18, 18, 31, E6);
            // 0x1a3c-0x1a64
            s2.vmudl(27, 12, 10, E0);
            s2.vmadm(27, 13, 10, E0);
            s2.vmadn(27, 12, 11, E0);
            s2.vmadh(27, 13, 11, E0);
            s2.vmadn(14, 17, 29, E1);
            s2.vmadh(15, 18, 29, E1);
            s2.vmudh(27, 2, 29, E1);
            s2.vmadl(27, 14, 4, E6);
            s2.vmadm(27, 15, 4, E6);
            s2.vmadn(16, 14, 7, E6);
            s2.vmadh(21, 15, 7, E6);
            // 0x1a68-0x1ab0
            s2.out_sh(0, v0);
            s2.out_store(2, 5, 12, 2);
            s2.out_store(2, 3, 12, 4);
            s2.out_store(2, 2, 12, 6);
            s2.out_sw(8, v1);
            s2.out_store(2, 11, 4, 12);
            s2.out_store(2, 10, 4, 14);
            s2.out_store(2, 9, 0, 16);
            s2.out_store(2, 8, 0, 18);
            s2.out_store(2, 11, 0, 20);
            s2.out_store(2, 10, 0, 22);
            s2.out_store(2, 20, 2, 24);
            s2.out_store(2, 19, 2, 26);
            s2.out_store(2, 11, 2, 28);
            s2.out_store(2, 10, 2, 30);
            s2.out_store(8, 21, 6, 32);
            s2.out_store(8, 13, 6, 40);
            s2.out_store(8, 16, 6, 48);
            s2.out_store(8, 12, 6, 56);
            // 0x1ab4-0x1ac0
            s2.out_store(8, 15, 6, 64);
            s2.out_store(8, 18, 6, 72);
            s2.out_store(8, 14, 6, 80);
            s2.out_store(8, 17, 6, 88);
            // 0x1ac4-0x1ac8
            let taken = t9 == 0;
            s2.advance(96);
            if taken {
                self.s2dex_emit(gfx);
                return;
            }
            // 0x1acc-0x1adc
            s2.vadd(2, 3, 29, E3);
            t9 = 0;
            s2.vadd(3, 5, 29, E3);
            s2.vadd(5, 6, 29, E3);
        }
    }
}
