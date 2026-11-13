//! Test ROMs of the CPU, each booted through its own IPL3 (krom's 6102
//! boot code). Dillon's run an instruction on many operands and leave in
//! r30 -1 when all gave the right result, or the number of the first that
//! did not. krom's (PeterLemon's, and Lemmy's tests of exceptions built on
//! them) draw a line per case into a 32-bit frame buffer, ending in PASS
//! in a green font or FAIL in a red one; those words are looked for,
//! glyph by glyph, wherever the test draws them.

use n64::N64;

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/");

fn rom(path: &str) -> Option<Vec<u8>> {
    let rom = std::fs::read(format!("{ROMS}{path}")).ok();
    if rom.is_none() {
        eprintln!("skipped: run tests/roms/fetch.sh");
    }
    rom
}

#[test]
fn dillon() {
    let tests = [
        "addiu", "addu", "and", "andi", "basic", "daddiu", "dsll", "dsll32", "dsllv", "dsra32", "nor", "or", "ori",
        "sll", "sllv", "slt", "slti", "sltiu", "sltu", "sra", "srav", "srl", "srlv", "subu", "xor", "xori",
    ];
    let mut failed = Vec::new();
    for test in tests {
        let Some(data) = rom(&format!("dillon/{test}.z64")) else { return };
        let mut n64 = N64::new(&data).unwrap();
        // The boot code uses r30 too: only the game's use counts.
        let entry = u64::from(n64.entry()) | 0xffff_ffff_0000_0000;
        while n64.registers().pc != entry {
            n64.step();
        }
        let start = n64.registers().gpr[30];
        let mut r30 = start;
        for _ in 0..600 {
            n64.run_frame(0);
            // It is cleared first, then set to the result.
            r30 = n64.registers().gpr[30];
            if r30 != start && r30 != 0 {
                break;
            }
        }
        match r30 {
            u64::MAX => eprintln!("{test}: passed"),
            n if n == start || n == 0 => failed.push(format!("{test}: did not finish, at {:x}", n64.registers().pc)),
            n => failed.push(format!("{test}: case {n} failed")),
        }
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

/// A font of krom's tests: 8 x 8 glyphs of RGBA8888 pixels, by character.
struct Font(Vec<u32>);

impl Font {
    fn load(name: &str) -> Option<Font> {
        let data = rom(&format!("krom/CPU/ADD/{name}"))?;
        Some(Font(data.chunks(4).map(|p| u32::from_be_bytes(p.try_into().unwrap())).collect()))
    }

    /// The pixels of a word as the test draws it, a row of glyphs.
    fn word(&self, text: &str) -> Vec<[u32; 64]> {
        text.bytes().map(|c| self.0[usize::from(c) * 64..usize::from(c) * 64 + 64].try_into().unwrap()).collect()
    }
}

/// The 32-bit frame buffer the video interface shows, from its registers:
/// its pixels (RGBA8888), width and height (as many lines as fit).
fn frame_buffer(n64: &N64) -> (Vec<u32>, usize, usize) {
    let word = |addr: u32| (0..4).map(|b| u32::from(n64.peek(addr + b)) << (24 - 8 * b)).sum::<u32>();
    let register = |offset: u32| word(0x0440_0000 + offset);
    let (origin, width) = (register(4) & 0xff_ffff, register(8) as usize & 0xfff);
    let height = 480.min((0x80_0000 - origin as usize) / 4 / width.max(1));
    let pixels = (0..width * height).map(|i| word(origin + 4 * i as u32)).collect();
    (pixels, width, height)
}

/// The text drawn in a font, on lines 8 pixels apart: the words of each
/// line that has any, one space between them.
fn text(pixels: &[u32], width: usize, height: usize, font: &Font) -> Vec<String> {
    let glyphs: Vec<(char, [u32; 64])> =
        (0x21u8..0x7f).map(|c| (char::from(c), font.word(&char::from(c).to_string())[0])).collect();
    let mut lines = Vec::new();
    for y in (0..height.saturating_sub(7)).step_by(8) {
        let (mut line, mut x, mut gap) = (String::new(), 0, false);
        while x + 8 <= width {
            let at = |p: usize| pixels[(y + p / 8) * width + x + p % 8];
            match glyphs.iter().find(|(_, glyph)| (0..64).all(|p| at(p) == glyph[p])) {
                Some(&(c, _)) => {
                    if gap && !line.is_empty() {
                        line.push(' ');
                    }
                    line.push(c);
                    (x, gap) = (x + 8, false);
                }
                None => (x, gap) = (x + 1, true),
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    lines
}

/// How many times a word appears in the frame buffer.
fn count(pixels: &[u32], width: usize, height: usize, word: &[[u32; 64]]) -> usize {
    let span = 8 * word.len();
    let mut found = 0;
    for y in 0..height.saturating_sub(7) {
        for x in 0..width.saturating_sub(span - 1) {
            let matches = word.iter().enumerate().all(|(g, glyph)| {
                (0..64).all(|p| pixels[(y + p / 8) * width + x + 8 * g + p % 8] == glyph[p])
            });
            if matches {
                found += 1;
            }
        }
    }
    found
}

/// Runs each of krom's ROMs for a second and fails with those that drew
/// FAIL, or no PASS.
fn krom(roms: &[&str]) {
    let (Some(red), Some(green)) = (Font::load("FontRed8x8.bin"), Font::load("FontGreen8x8.bin")) else { return };
    let (pass, fail) = (green.word("PASS"), red.word("FAIL"));
    let mut failed = Vec::new();
    for path in roms {
        let Some(data) = rom(&format!("krom/{path}")) else { return };
        let mut n64 = N64::new(&data).unwrap();
        for _ in 0..60 {
            n64.run_frame(0);
        }
        let (pixels, width, height) = frame_buffer(&n64);
        let (passes, fails) = (count(&pixels, width, height, &pass), count(&pixels, width, height, &fail));
        if passes == 0 || fails > 0 {
            failed.push(format!("{path}: {passes} passed, {fails} failed"));
        } else {
            eprintln!("{path}: {passes} passed");
        }
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

#[test]
fn krom_cpu() {
    krom(&[
        "CPU/ADD/CPUADD.N64",
        "CPU/ADDU/CPUADDU.N64",
        "CPU/AND/CPUAND.N64",
        "CPU/DADD/CPUDADD.N64",
        "CPU/DADDU/CPUDADDU.N64",
        "CPU/DDIV/CPUDDIV.N64",
        "CPU/DDIVU/CPUDDIVU.N64",
        "CPU/DIV/CPUDIV.N64",
        "CPU/DIVU/CPUDIVU.N64",
        "CPU/DMULT/CPUDMULT.N64",
        "CPU/DMULTU/CPUDMULTU.N64",
        "CPU/DSUB/CPUDSUB.N64",
        "CPU/DSUBU/CPUDSUBU.N64",
        "CPU/MULT/CPUMULT.N64",
        "CPU/MULTU/CPUMULTU.N64",
        "CPU/NOR/CPUNOR.N64",
        "CPU/OR/CPUOR.N64",
        "CPU/SUB/CPUSUB.N64",
        "CPU/SUBU/CPUSUBU.N64",
        "CPU/XOR/CPUXOR.N64",
        "CPU/SHIFT/DSLL/CPUDSLL.N64",
        "CPU/SHIFT/DSLL32/CPUDSLL32.N64",
        "CPU/SHIFT/DSLLV/CPUDSLLV.N64",
        "CPU/SHIFT/DSRA/CPUDSRA.N64",
        "CPU/SHIFT/DSRA32/CPUDSRA32.N64",
        "CPU/SHIFT/DSRAV/CPUDSRAV.N64",
        "CPU/SHIFT/DSRL/CPUDSRL.N64",
        "CPU/SHIFT/DSRL32/CPUDSRL32.N64",
        "CPU/SHIFT/DSRLV/CPUDSRLV.N64",
        "CPU/SHIFT/SLL/CPUSLL.N64",
        "CPU/SHIFT/SLLV/CPUSLLV.N64",
        "CPU/SHIFT/SRA/CPUSRA.N64",
        "CPU/SHIFT/SRAV/CPUSRAV.N64",
        "CPU/SHIFT/SRL/CPUSRL.N64",
        "CPU/SHIFT/SRLV/CPUSRLV.N64",
        "CPU/LOADSTORE/LB/CPULB.N64",
        "CPU/LOADSTORE/LD/CPULD.N64",
        "CPU/LOADSTORE/LH/CPULH.N64",
        "CPU/LOADSTORE/LW/CPULW.N64",
        "CPU/LOADSTORE/SB/CPUSB.N64",
        "CPU/LOADSTORE/SD/CPUSD.N64",
        "CPU/LOADSTORE/SH/CPUSH.N64",
        "CPU/LOADSTORE/SW/CPUSW.N64",
        "CPU/LOADSTORE/LL_LLD_SC_SCD/LL_LLD_SC_SCD.N64",
    ]);
}

#[test]
fn krom_cop0() {
    krom(&["CP0/COP0Cause/COP0Cause.N64", "CP0/COP0Register/COP0Register.N64"]);
}

#[test]
fn krom_fpu() {
    krom(&[
        "CP1/ABS/CP1ABS.N64",
        "CP1/ADD/CP1ADD.N64",
        "CP1/CEIL/CP1CEIL.N64",
        "CP1/CVT/CP1CVT.N64",
        "CP1/DIV/CP1DIV.N64",
        "CP1/FLOOR/CP1FLOOR.N64",
        "CP1/MUL/CP1MUL.N64",
        "CP1/NEG/CP1NEG.N64",
        "CP1/ROUND/CP1ROUND.N64",
        "CP1/SQRT/CP1SQRT.N64",
        "CP1/SUB/CP1SUB.N64",
        "CP1/TRUNC/CP1TRUNC.N64",
        "CP1/C/EQ/CP1CEQ.N64",
        "CP1/C/F/CP1CF.N64",
        "CP1/C/LE/CP1CLE.N64",
        "CP1/C/LT/CP1CLT.N64",
        "CP1/C/NGE/CP1CNGE.N64",
        "CP1/C/NGL/CP1CNGL.N64",
        "CP1/C/NGLE/CP1CNGLE.N64",
        "CP1/C/NGT/CP1CNGT.N64",
        "CP1/C/OLE/CP1COLE.N64",
        "CP1/C/OLT/CP1COLT.N64",
        "CP1/C/SEQ/CP1CSEQ.N64",
        "CP1/C/SF/CP1CSF.N64",
        "CP1/C/UEQ/CP1CUEQ.N64",
        "CP1/C/ULE/CP1CULE.N64",
        "CP1/C/ULT/CP1CULT.N64",
        "CP1/C/UN/CP1CUN.N64",
    ]);
}

/// COP1FullMode moves values through the FPU's registers with Status.FR
/// clear and set, and draws what comes back; these are the values the
/// picture that comes with the test shows, taken on a console.
#[test]
fn krom_fpu_registers() {
    let Some(black) = Font::load("FontBlack8x8.bin") else { return };
    let Some(data) = rom("krom/CP1/COP1FullMode/COP1FullMode.N64") else { return };
    let mut n64 = N64::new(&data).unwrap();
    for _ in 0..60 {
        n64.run_frame(0);
    }
    let (pixels, width, height) = frame_buffer(&n64);
    let first = [
        "DECAFBAD 00000000  DECAFBAD 11111111  DECAFBAD 22222222  DECAFBAD 33333333",
        "00000000 00000000  01010101 10101010  02020202 20202020  03030303 30303030",
        "11111111 00000000  DECAFBAD 0BADF00D  33333333 22222222  DECAFBAD 0BADF00D",
        "01010101 10101010  01010101 10101010  03030303 30303030  03030303 30303030",
    ];
    let words = |line: &&str| line.split_whitespace().collect::<Vec<_>>().join(" ");
    let expected: Vec<String> = first.iter().chain(&first).map(words).collect();
    assert_eq!(text(&pixels, width, height, &black), expected);
}

#[test]
fn krom_fpu_compare() {
    // Each draws FCR31 after comparing pairs of -1, -0, 0, 1, -inf, inf and
    // a NaN, a row for each first operand: in green when only the condition
    // is set (G), in red when nothing is (R), in black otherwise (?, the
    // invalid operation flags). Read from the pictures that come with the
    // tests, taken on a console.
    let expected = [
        ("eq", "GRRRRRRRGGRRRRRGGRRRRRRRGRRRRRRRGRRRRRRRGRRRRRRRR"),
        ("f", "RRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRR"),
        ("le", "GGGGRG?RGGGRG?RGGGRG?RRRGRG?GGGGGG?RRRRRG????????"),
        ("lt", "RGGGRG?RRRGRG?RRRGRG?RRRRRG?GGGGRG?RRRRRR????????"),
        ("nge", "RGGGRG?RRRGRG?RRRGRG?RRRRRG?GGGGRG?RRRRRR????????"),
        ("ngle", "RRRRRR?RRRRRR?RRRRRR?RRRRRR?RRRRRR?RRRRRR????????"),
        ("ngl", "GRRRRR?RGGRRR?RGGRRR?RRRGRR?RRRRGR?RRRRRG????????"),
        ("ngt", "GGGGRG?RGGGRG?RGGGRG?RRRGRG?GGGGGG?RRRRRG????????"),
        ("ole", "GGGGRGRRGGGRGRRGGGRGRRRRGRGRGGGGGGRRRRRRGRRRRRRRR"),
        ("olt", "RGGGRGRRRRGRGRRRRGRGRRRRRRGRGGGGRGRRRRRRRRRRRRRRR"),
        ("seq", "GRRRRR?RGGRRR?RGGRRR?RRRGRR?RRRRGR?RRRRRG????????"),
        ("sf", "RRRRRR?RRRRRR?RRRRRR?RRRRRR?RRRRRR?RRRRRR????????"),
        ("ueq", "GRRRRRGRGGRRRGRGGRRRGRRRGRRGRRRRGRGRRRRRGGGGGGGGG"),
        ("ule", "GGGGRGGRGGGRGGRGGGRGGRRRGRGGGGGGGGGRRRRRGGGGGGGGG"),
        ("ult", "RGGGRGGRRRGRGGRRRGRGGRRRRRGGGGGGRGGRRRRRRGGGGGGGG"),
        ("un", "RRRRRRGRRRRRRGRRRRRRGRRRRRRGRRRRRRGRRRRRRGGGGGGGG"),
    ];
    let mut failed = Vec::new();
    for (condition, cells) in expected {
        let path = format!("krom/CP1/C/FPUCompare/FPUCompare-c.{condition}.s.N64");
        let Some(data) = rom(&path) else { return };
        let mut n64 = N64::new(&data).unwrap();
        for _ in 0..60 {
            n64.run_frame(0);
        }
        let (pixels, width, _) = frame_buffer(&n64);
        // A cell's colour, from the bottom row of its first glyph.
        let drawn: String = (0..49)
            .map(|i| {
                let (row, column) = (i / 7, i % 7);
                let p = pixels[(32 + 16 * row + 7) * width + 88 + 72 * column + 4];
                let (red, green) = (p >> 24 & 0xff, p >> 16 & 0xff);
                if green > red + 40 { 'G' } else if red > green + 40 { 'R' } else { '?' }
            })
            .collect();
        if drawn == cells {
            eprintln!("c.{condition}.s: passed");
        } else {
            failed.push(format!("c.{condition}.s: {drawn}, not {cells}"));
        }
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

#[test]
fn krom_exceptions() {
    krom(&[
        "Exceptions/Compare/ExceptionCompareDisabled.n64",
        "Exceptions/Compare/ExceptionCompareRegisters.n64",
        "Exceptions/Syscall/ExceptionSyscall.n64",
        "Exceptions/Syscall/ExceptionSyscallDelay.n64",
        "Exceptions/Syscall/ExceptionSyscallDelay2.n64",
        "Exceptions/Syscall/ExceptionSyscallWhileInException.n64",
        "Exceptions/TLB/ExceptionTLBReadMiss.n64",
        "Exceptions/TLB/ExceptionTLBReadMissDelay.n64",
        "Exceptions/TLB/ExceptionTLBReadMissNested.n64",
        "Exceptions/TLB/ExceptionTLBReadMissNestedDelay.n64",
        "Exceptions/TLB/ExceptionTLBWriteMiss.n64",
        "Exceptions/TLB/ExceptionTLBWriteMissDelay.n64",
        "Exceptions/Trap/ExceptionTEQ.n64",
        "Exceptions/Trap/ExceptionTEQDelay.n64",
        "Exceptions/Unaligned/ExceptionUnaligned.n64",
        "Exceptions/Unaligned/ExceptionUnalignedDelay.n64",
        "Exceptions/VIIntr/ExceptionVIIntrDisabled.n64",
    ]);
}

/// The RSP's scalar unit and vector unit (krom's tests run RSP code and
/// draw what it computed, with PASS or FAIL).
#[test]
fn krom_rsp() {
    krom(&[
        "RSPTest/CP2/LOADSTORE/LTV/RSPCP2LTV.N64",
        "RSPTest/CP2/LOADSTORE/LWV/RSPCP2LWV.N64",
        "RSPTest/CP2/LOADSTORE/TransposeMatrix/RSPTransposeMatrix.N64",
        "RSPTest/CP2/LOADSTORE/TransposeMatrixVMOV/RSPTransposeMatrixVMOV.N64",
        "RSPTest/CP2/RESERVED/V056/RSPCP2V056.N64",
        "RSPTest/CP2/RESERVED/VADDB/RSPCP2VADDB.N64",
        "RSPTest/CP2/RESERVED/VMULQ/RSPCP2VMULQ.N64",
        "RSPTest/CP2/RESERVED/VRNDP/RSPCP2VRNDP.N64",
        "RSPTest/CP2/RESERVED/VSUBB/RSPCP2VSUBB.N64",
        "RSPTest/CP2/RESERVED/VSUT/RSPCP2VSUT.N64",
        "RSPTest/CP2/SORT/RSPSORT.N64",
        "RSPTest/CP2/VABS/RSPCP2VABS.N64",
        "RSPTest/CP2/VADD/RSPCP2VADD.N64",
        "RSPTest/CP2/VAND/RSPCP2VAND.N64",
        "RSPTest/CP2/VCL/RSPCP2VCL.N64",
        "RSPTest/CP2/VCR/RSPCP2VCR.N64",
        "RSPTest/CP2/VEQ/RSPCP2VEQ.N64",
        "RSPTest/CP2/VLT/RSPCP2VLT.N64",
        "RSPTest/CP2/VMACF/RSPCP2VMACF.N64",
        "RSPTest/CP2/VMADL/RSPCP2VMADL.N64",
        "RSPTest/CP2/VMADN/RSPCP2VMADN.N64",
        "RSPTest/CP2/VMUDL/RSPCP2VMUDL.N64",
        "RSPTest/CP2/VMUDN/RSPCP2VMUDN.N64",
        "RSPTest/CP2/VMULF/RSPCP2VMULF.N64",
        "RSPTest/CP2/VNOP/RSPCP2VNOP.N64",
        "RSPTest/CP2/VOR/RSPCP2VOR.N64",
        "RSPTest/CP2/VRCP/RSPCP2VRCP.N64",
        "RSPTest/CP2/VRCPH/RSPCP2VRCPH.N64",
        "RSPTest/CP2/VRCPL/RSPCP2VRCPL.N64",
        "RSPTest/CP2/VSAR/RSPCP2VSAR.N64",
        "RSPTest/CP2/VSUB/RSPCP2VSUB.N64",
        "RSPTest/CP2/VXOR/RSPCP2VXOR.N64",
        "RSPTest/CPU/ADD/RSPCPUADD.N64",
        "RSPTest/CPU/ADDU/RSPCPUADDU.N64",
        "RSPTest/CPU/AND/RSPCPUAND.N64",
        "RSPTest/CPU/NOR/RSPCPUNOR.N64",
        "RSPTest/CPU/OR/RSPCPUOR.N64",
        "RSPTest/CPU/SHIFT/SLL/RSPCPUSLL.N64",
        "RSPTest/CPU/SHIFT/SLLV/RSPCPUSLLV.N64",
        "RSPTest/CPU/SHIFT/SRA/RSPCPUSRA.N64",
        "RSPTest/CPU/SHIFT/SRAV/RSPCPUSRAV.N64",
        "RSPTest/CPU/SHIFT/SRL/RSPCPUSRL.N64",
        "RSPTest/CPU/SHIFT/SRLV/RSPCPUSRLV.N64",
        "RSPTest/CPU/SUB/RSPCPUSUB.N64",
        "RSPTest/CPU/SUBU/RSPCPUSUBU.N64",
        "RSPTest/CPU/XOR/RSPCPUXOR.N64",
        "RSPTest/MEM/IMEM/RSPIMEM.N64",
    ]);
}

/// The PI's DMA from the cartridge, long transfers to RDRAM addresses off
/// by 2, 4 and 6 bytes from a doubleword.
#[test]
fn krom_pi() {
    krom(&[
        "DMAAlignment-PI-cart/DMAAlignment-PI-ROM-FROM_large_2.N64",
        "DMAAlignment-PI-cart/DMAAlignment-PI-ROM-FROM_large_4.N64",
        "DMAAlignment-PI-cart/DMAAlignment-PI-ROM-FROM_large_6.N64",
    ]);
}

/// Tests that do not pass yet: the timing of instructions (each takes one
/// cycle here); short DMAs to misaligned RDRAM (which the PI does in a
/// first block of its own, with quirks) and misaligned SP DMAs; vector
/// opcodes the RSP does not document (VACC and VSAC here are 0x1E and 0x1F,
/// which accumulate; the VEXT group), which no microcode uses. `cargo test
/// --release -p n64 -- --ignored` runs them.
#[test]
#[ignore]
fn krom_not_yet() {
    krom(&[
        "CPU/TIMINGNTSC/CPUTIMINGNTSC.N64",
        "CP1/TIMINGNTSC/CP1TIMINGNTSC.N64",
        "DMAAlignment-PI-cart/DMAAlignment-PI-ROM-FROM.N64",
        "RSPTest/CP2/RESERVED/V073/RSPCP2V073.N64",
        "RSPTest/CP2/RESERVED/VACC/RSPCP2VACC.N64",
        "RSPTest/CP2/RESERVED/VEXTN/RSPCP2VEXTN.N64",
        "RSPTest/CP2/RESERVED/VEXTQ/RSPCP2VEXTQ.N64",
        "RSPTest/CP2/RESERVED/VEXTT/RSPCP2VEXTT.N64",
        "RSPTest/CP2/RESERVED/VSAC/RSPCP2VSAC.N64",
        "RSPTest/DMAAlignment-SP/DMAAlignment-SP-FROM.N64",
        "RSPTest/DMAAlignment-SP/DMAAlignment-SP-TO.N64",
    ]);
}
