//! The JIT against the interpreter, the reference: each program of
//! ps1-tests run with the JIT and without, for the same frames, prints the
//! same and leaves the same VRAM. On ARM (32 or 64-bit) the JIT must also
//! have compiled blocks, so the comparison is not of the interpreter with
//! itself; elsewhere both runs are the interpreter's.

use psx::Psx;

const TESTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/ps1-tests/");

/// Programs that run on their own: CPU, GTE, DMA, GPU drawing, MDEC. (The
/// CD-ROM's need a disc; the SPU's and the pad's wait for input. The timers'
/// measure the counters to the cycle, and the JIT counts a block's cycles at
/// its end and looks at events between blocks, so its counts land a few
/// cycles apart: 909 ticks for 910.)
const PROGRAMS: [&str; 28] = [
    "cpu/access-time/access-time",
    "cpu/code-in-io/code-in-io",
    "cpu/cop/cop",
    "cpu/io-access-bitwidth/io-access-bitwidth",
    "dma/chain-looping/chain-looping",
    "dma/chopping/chopping",
    "dma/dpcr/dpcr",
    "dma/otc-test/otc-test",
    "gpu/animated-triangle/animated-triangle",
    "gpu/bandwidth/bandwidth",
    "gpu/benchmark/benchmark",
    "gpu/clipping/clipping",
    "gpu/clut-cache/clut-cache",
    "gpu/gp0-e1/gp0-e1",
    "gpu/lines/lines",
    "gpu/mask-bit/mask-bit",
    "gpu/quad/quad",
    "gpu/rectangles/rectangles",
    "gpu/texture-flip/texture-flip",
    "gpu/texture-overflow/texture-overflow",
    "gpu/transparency/transparency",
    "gpu/triangle/triangle",
    "gpu/uv-interpolation/uv-interpolation",
    "gpu/vram-to-vram-overlap/vram-to-vram-overlap",
    "gte-fuzz/gte-fuzz",
    "gte/test-all/test-all",
    "mdec/frame/frame-15bit",
    "mdec/movie/movie-15bit",
];

const FRAMES: usize = 600;

/// What a program printed and left in VRAM after FRAMES frames, and how
/// many blocks were compiled.
fn run(exe: &[u8], jit: bool) -> (Vec<u8>, Vec<u16>, usize) {
    let mut psx = Psx::with_exe(exe).unwrap();
    psx.set_gpu_thread(false);
    psx.set_jit(jit);
    for _ in 0..FRAMES {
        psx.run_frame();
    }
    let blocks = psx.jit_blocks().len();
    (psx.tty().to_vec(), psx.vram().to_vec(), blocks)
}

#[test]
fn the_jit_does_what_the_interpreter_does() {
    let arm = cfg!(any(target_arch = "arm", target_arch = "aarch64"));
    let mut differ = Vec::new();
    for program in PROGRAMS {
        let name = program.rsplit('/').next().unwrap();
        let Ok(exe) = std::fs::read(format!("{TESTS}{program}.exe")) else {
            eprintln!("skipped: run tests/roms/fetch.sh");
            return;
        };
        let (tty, vram, blocks) = run(&exe, true);
        let (tty_ref, vram_ref, _) = run(&exe, false);
        if arm {
            assert!(blocks > 0, "{name}: the JIT compiled nothing");
        }
        if tty != tty_ref || vram != vram_ref {
            let what = if tty != tty_ref { "printed" } else { "VRAM" };
            eprintln!("{name}: {what} differs ({blocks} blocks)");
            differ.push(name);
        }
    }
    assert!(differ.is_empty(), "the JIT differs from the interpreter in {differ:?}");
}
