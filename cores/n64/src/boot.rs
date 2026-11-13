//! The console as the PIF's boot code leaves it for the cartridge's. At
//! power on the CPU runs IPL1 from the PIF's ROM, which halts the RSP,
//! resets the PI, blanks the video and silences the audio, and copies
//! IPL2 to IMEM; IPL2 reads the CIC's seed, sets the PI's timings from the
//! ROM's header, copies the cartridge's boot code (IPL3, ROM 0x40-0xFFF)
//! to DMEM and starts it at A400_0040. Here that is all done at once
//! (IPL1 and IPL2 are Nintendo's, and not needed), and the CPU starts at
//! IPL3, which sets up RDRAM, loads the game's first megabyte, checks it
//! and jumps to it.
//!
//! What the state is, and where it comes from: n64brew's "Initial Program
//! Load" (IPL1's code, which sets Status to 3400_0000 and Config to
//! 0006_E463) and "PIF-NUS" (the seeds of each CIC), and mupen64plus's
//! IPL2 in place of Nintendo's (src/device/pif/bootrom_hle.c: s3-s7, t3,
//! sp and ra, the PI's timings, and the words of IPL2 that CIC 6105's boot
//! code runs from IMEM). s7 is IPL2's reading of a bit of the boot
//! information the PIF writes to its RAM, which ares's model of the PIF's
//! firmware has always set (ares/n64/pif/hle.cpp); mupen64plus leaves it
//! clear.

use crate::N64;
use crate::cart::Region;

// The general registers IPL2 leaves set.
const T3: usize = 11;
const S3: usize = 19;
const S4: usize = 20;
const S5: usize = 21;
const S6: usize = 22;
const S7: usize = 23;
const SP: usize = 29;
const RA: usize = 31;

impl N64 {
    pub(crate) fn power_on(&mut self) {
        self.power_on_rdram();
        let region = self.cart.region;
        self.power_on_vi(region);

        // IPL1 sets Status to 3400_0000 (COP0 and COP1 usable, FR set),
        // but what a game finds, since IPL3 writes neither, is IPL2's: COP1
        // usable, FR, and 64-bit addresses in every mode. The other
        // registers come out of reset and the PIF's code as below: those
        // are the values PeterLemon's COP0Register test and Lemmy's tests
        // of exceptions read on a console (in their tables of expected
        // values and pictures), Cause with bits that are read-only.
        let cop0 = &mut self.cpu.cop0;
        cop0.status = 0x2410_00e0;
        cop0.cause = 0xb000_007c;
        cop0.config = 0x7006_e463;
        cop0.epc = u64::MAX;
        cop0.error_epc = u64::MAX;
        cop0.bad_vaddr = u64::MAX;
        cop0.context = 0x007f_fff0;
        cop0.x_context = 0xffff_fff0;
        cop0.ll_addr = 0xffff_ffff;
        cop0.watch_lo = 0xffff_fffb;
        cop0.watch_hi = 0xf;
        // The RSP halted; no vertical interrupt (V_INTR at 3FF).
        self.write_vi(0x0440_000c, 0x3ff);

        // IPL2: the PI's domain 1 timings from the header's first word
        // (its latency, pulse width, page size and release).
        let header = u32::from_be_bytes(self.cart.rom[..4].try_into().unwrap());
        self.pi.domains[0] = [header & 0xff, header >> 8 & 0xff, header >> 16 & 0xf, header >> 20 & 0x3];
        self.rsp.memory[0x40..0x1000].copy_from_slice(&self.cart.rom[0x40..0x1000]);
        let ipl2 =
            [0x3c0d_bfc0, 0x8da8_07fc, 0x25ad_07c0, 0x3108_0080, 0x5500_fffc, 0x3c0d_bfc0, 0x8da8_0024, 0x3c0b_b000];
        for (i, word) in ipl2.iter().enumerate() {
            self.rsp.memory[0x1000 + 4 * i..0x1004 + 4 * i].copy_from_slice(&u32::to_be_bytes(*word));
        }
        let gpr = &mut self.cpu.gpr;
        // The ROM's kind (a cartridge, not the 64DD), the TV standard,
        // the reset (a cold one), the CIC's seed and the PIF's version.
        gpr[S3] = 0;
        gpr[S4] = match region {
            Region::Pal => 0,
            Region::Ntsc => 1,
            Region::Mpal => 2,
        };
        gpr[S5] = 0;
        gpr[S6] = u64::from(self.cart.cic.seed());
        gpr[S7] = 1;
        gpr[T3] = 0xffff_ffff_a400_0040;
        gpr[SP] = 0xffff_ffff_a400_1ff0;
        gpr[RA] = 0xffff_ffff_a400_1550;
        self.cpu.jump(0xffff_ffff_a400_0040);
        self.schedule_timer();
    }
}
