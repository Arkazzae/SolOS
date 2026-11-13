//! RustyN64's RDP conformance vectors (crates/rustyn64-test-harness/tests/
//! vectors, MIT or Apache-2.0), fetched by tests/roms/fetch.sh: each is a
//! short list of RDP commands, sometimes a texture to load, and the frame
//! buffer angrylion's RDP drew from them (angrylion-rdp-plus at 31bdb1f,
//! run by the collection's own driver). 44 were written by hand, each about
//! one feature; 144 more (fuzz/) are fill rectangles, scissored fill
//! rectangles and fill triangles at random sizes and places.
//!
//! Each list runs here three times: on the reference rasteriser (pixel()),
//! with the accelerations on (the span paths, NEON on ARM), and on three
//! drawing threads. The three frame buffers must be the same bytes, for
//! every vector. The reference's must be angrylion's, except for the
//! vectors listed in EXPECTED, which draw what this RDP leaves out on
//! purpose or does not do yet: those must differ the way the list says, so
//! that a vector that starts to match, or one that differs where it did
//! not, fails the test.
//!
//! The commands go in as a game's would: the console starts a 4 KB
//! cartridge whose boot code (run from DMEM, as an IPL3 is) stores the
//! texture and the commands into RDRAM, points DPC_START and DPC_END at
//! them and waits in a loop in RDRAM. RDRAM starts zeroed, as it does in
//! the collection's driver.
//!
//! The collection's 13 VI vectors (.vivec) are not used: their golden
//! pictures are angrylion's TV picture, the active area resampled to the
//! screen and cut to what a TV shows (25 x 16 for 40 pixels of H_VIDEO),
//! through the VI's filters (bilinear resampling, gamma, de-dither, divot,
//! antialiasing). This VI hands over the frame buffer's active area as it
//! is, for the host to scale, and has none of those filters, so none of
//! its pictures could be compared.

use n64::N64;
use std::fmt::Write;

const VECTORS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/rdp-vectors");

/// How a vector on EXPECTED differs from angrylion's frame buffer.
#[derive(Clone, Copy, PartialEq)]
enum Differs {
    /// Only in which pixels are drawn and in their coverage bits: a pixel
    /// both draw has the same color.
    Edges,
    /// In the color of pixels both draw.
    Colors,
}
use Differs::{Colors, Edges};

// Why. First what docs/n64.md says this RDP leaves out on purpose:
const COVERAGE: &str = "coverage: angrylion (antialiasing off) draws a pixel when the sample at its top left \
    corner (x + 0 on the line's first subscanline) lies between the edges, and stores its coverage (bit 0 of a \
    16-bit pixel, the top three bits of a 32-bit one's low byte); here a pixel is drawn when its centre lies \
    between them on any subscanline, and counts as fully covered";
const ANTIALIASING: &str = "antialiasing: edge pixels get partial coverage";
const DITHER: &str = "dithering";
const LOD: &str = "LOD: without mipmaps the LOD fraction is 255 here, and the tile the primitive's";
const CHROMA_KEY: &str = "chroma key";
// Then differences that are not on purpose:
const FILL_TRIANGLE: &str = "a triangle in fill mode: angrylion fills each line from the pixel its leftmost \
    subscanline's left edge is in to the pixel its rightmost right edge is in, both included; here as in the \
    other modes, by pixel centres";
const CI4_WITHOUT_TLUT: &str = "CI4 with TLUT off: angrylion gives the palette number and the index (pal << 4 | \
    index) in every channel, here the index widened as I4 (index * 17)";
const COPY_SCALED: &str = "copy mode at dsdx 2 (not 4): the RDP copies four texels from s for each four pixels \
    and steps s by dsdx between them, here s steps a quarter of dsdx each pixel";
const MID_TEXEL: &str = "mid-texel: at fractions of exactly one half both ways angrylion averages all four \
    texels; here the three-texel filter as at any fraction";

/// The vectors whose golden frame buffer this RDP does not draw: how they
/// differ, and why.
const EXPECTED: &[(&str, Differs, &str)] = &[
    ("aa_tri_coverage_16", Edges, ANTIALIASING),
    ("alpha_compare_16", Edges, COVERAGE),
    ("alpha_compare_z_16", Edges, COVERAGE),
    ("ci4_tlut_disabled_16", Colors, CI4_WITHOUT_TLUT),
    ("cvg_dest_full_16", Edges, COVERAGE),
    ("dither_tri_32", Colors, DITHER),
    ("fill_tri_16", Edges, FILL_TRIANGLE),
    ("fill_tri_frac_16", Edges, FILL_TRIANGLE),
    ("fill_tri_neg_16", Edges, FILL_TRIANGLE),
    ("load_block_count_16", Edges, COVERAGE),
    ("prim_combiner_32", Edges, COVERAGE),
    ("shade_depth_tri_frac_16", Edges, COVERAGE),
    ("shade_grad_tri_32", Edges, COVERAGE),
    ("shade_tri_32", Edges, COVERAGE),
    ("shade_tri_frac_16", Edges, COVERAGE),
    ("tex_rect_mag_16", Colors, COPY_SCALED),
    ("tex_tri_16", Edges, COVERAGE),
    ("tex_tri_2cycle_16", Edges, COVERAGE),
    ("tex_tri_base_tile_16", Edges, COVERAGE),
    ("tex_tri_bilinear_16", Edges, COVERAGE),
    ("tex_tri_bilinear_wrap_16", Edges, COVERAGE),
    ("tex_tri_chromakey_16", Edges, COVERAGE),
    ("tex_tri_chromakey_alpha_16", Colors, CHROMA_KEY),
    ("tex_tri_ci4_tlut_16", Edges, COVERAGE),
    ("tex_tri_clamp_16", Edges, COVERAGE),
    ("tex_tri_convert_k45_16", Edges, COVERAGE),
    ("tex_tri_convert_kneg_16", Edges, COVERAGE),
    ("tex_tri_fixed_16", Edges, COVERAGE),
    ("tex_tri_i4_16", Edges, COVERAGE),
    ("tex_tri_lodfrac_16", Colors, LOD),
    ("tex_tri_mid_texel_16", Colors, MID_TEXEL),
    ("tex_tri_mip_tile_16", Colors, LOD),
    ("tex_tri_mirror_s_16", Edges, COVERAGE),
    ("tex_tri_primlodfrac_16", Edges, COVERAGE),
    ("tex_tri_wrap_16", Edges, COVERAGE),
    ("fz_tri_000", Edges, FILL_TRIANGLE),
    ("fz_tri_001", Edges, FILL_TRIANGLE),
    ("fz_tri_002", Edges, FILL_TRIANGLE),
    ("fz_tri_003", Edges, FILL_TRIANGLE),
    ("fz_tri_004", Edges, FILL_TRIANGLE),
    ("fz_tri_005", Edges, FILL_TRIANGLE),
    ("fz_tri_009", Edges, FILL_TRIANGLE),
    ("fz_tri_011", Edges, FILL_TRIANGLE),
    ("fz_tri_013", Edges, FILL_TRIANGLE),
    ("fz_tri_014", Edges, FILL_TRIANGLE),
    ("fz_tri_017", Edges, FILL_TRIANGLE),
    ("fz_tri_018", Edges, FILL_TRIANGLE),
    ("fz_tri_020", Edges, FILL_TRIANGLE),
    ("fz_tri_021", Edges, FILL_TRIANGLE),
    ("fz_tri_022", Edges, FILL_TRIANGLE),
    ("fz_tri_023", Edges, FILL_TRIANGLE),
    ("fz_tri_024", Edges, FILL_TRIANGLE),
    ("fz_tri_025", Edges, FILL_TRIANGLE),
    ("fz_tri_029", Edges, FILL_TRIANGLE),
    ("fz_tri_030", Edges, FILL_TRIANGLE),
    ("fz_tri_031", Edges, FILL_TRIANGLE),
    ("fz_tri_032", Edges, FILL_TRIANGLE),
    ("fz_tri_034", Edges, FILL_TRIANGLE),
    ("fz_tri_035", Edges, FILL_TRIANGLE),
    ("fz_tri_037", Edges, FILL_TRIANGLE),
    ("fz_tri_038", Edges, FILL_TRIANGLE),
    ("fz_tri_039", Edges, FILL_TRIANGLE),
    ("fz_tri_040", Edges, FILL_TRIANGLE),
    ("fz_tri_041", Edges, FILL_TRIANGLE),
    ("fz_tri_042", Edges, FILL_TRIANGLE),
    ("fz_tri_044", Edges, FILL_TRIANGLE),
    ("fz_tri_045", Edges, FILL_TRIANGLE),
    ("fz_tri_046", Edges, FILL_TRIANGLE),
    ("fz_tri_047", Edges, FILL_TRIANGLE),
];

/// A vector (the .rvec container): its frame buffer's place and size, the
/// commands and the texture with their places, and the golden pixels.
/// Version 1 has no texture, version 2 has one, and version 3 adds after
/// the pixels the two bits RDRAM keeps beside each 16-bit pixel (the rest
/// of its coverage), which this core does not keep, so they are not
/// compared.
struct Vector {
    fb_addr: u32,
    width: usize,
    height: usize,
    bpp: usize,
    other_modes: u64,
    cmd_addr: u32,
    cmds: Vec<u8>,
    preload_addr: u32,
    preload: Vec<u8>,
    golden: Vec<u8>,
}

fn parse(bytes: &[u8]) -> Result<Vector, String> {
    let word = |i: usize| bytes.get(4 * i..4 * i + 4).map(|w| u32::from_be_bytes(w.try_into().unwrap()));
    if word(0) != Some(0x5256_4543) {
        return Err("not an RVEC file".into());
    }
    let header = |i| word(i).ok_or_else(|| "short header".to_string());
    let (fb_addr, width, height, bpp) = (header(2)?, header(3)? as usize, header(4)? as usize, header(5)? as usize);
    let (cmd_addr, cmd_len, fb_len) = (header(6)?, header(7)? as usize, header(8)? as usize);
    let (start, preload_addr, preload_len) = match header(1)? {
        1 | 3 => (36, 0, 0),
        2 => (44, header(9)?, header(10)? as usize),
        version => return Err(format!("version {version}")),
    };
    if !(bpp == 2 || bpp == 4) || fb_len != width * height * bpp || cmd_len % 8 != 0 {
        return Err("bad sizes".into());
    }
    let body = |from: usize, len: usize| bytes.get(from..from + len).map(<[u8]>::to_vec).ok_or("short file");
    let preload = body(start, preload_len)?;
    let cmds = body(start + preload_len, cmd_len)?;
    let golden = body(start + preload_len + cmd_len, fb_len)?;
    // The other modes the draw sees (Set Other Modes, the last one), for failure messages.
    let words = cmds.chunks(8).map(|w| u64::from_be_bytes(w.try_into().unwrap()));
    let other_modes = words.rev().find(|w| w >> 56 & 0x3f == 0x2f).unwrap_or(0) & 0x00ff_ffff_ffff_ffff;
    Ok(Vector { fb_addr, width, height, bpp, other_modes, cmd_addr, cmds, preload_addr, preload, golden })
}

// The few MIPS instructions the boot code needs.
const T0: u32 = 8;
const T1: u32 = 9;
fn lui(rt: u32, imm: u32) -> u32 {
    0x0f << 26 | rt << 16 | imm >> 16
}
fn ori(rt: u32, rs: u32, imm: u32) -> u32 {
    0x0d << 26 | rs << 21 | rt << 16 | imm & 0xffff
}
fn sw(rt: u32, offset: u32, base: u32) -> u32 {
    0x2b << 26 | base << 21 | rt << 16 | offset
}
fn jr(rs: u32) -> u32 {
    rs << 21 | 0x08
}

/// The cartridge that draws a vector: its boot code (ROM 0x40 to 0xFFF,
/// which the console copies to DMEM and starts) stores each word of the
/// texture and of the commands into RDRAM through KSEG1 (two instructions
/// build a word, one stores it), and a loop of its own at RDRAM 0 (a
/// branch to itself, which the core skips to the next event); clears XBUS,
/// FREEZE and FLUSH in DPC_STATUS, sets DPC_START and DPC_END (the RDP
/// runs the list when DPC_END is written) and jumps to the loop.
fn cartridge(v: &Vector) -> Vec<u8> {
    let mut code = Vec::new();
    let set = |code: &mut Vec<u32>, reg: u32, value: u32| code.extend([lui(reg, value), ori(reg, reg, value)]);
    // BEQ zero, zero to itself, and a NOP in its slot.
    let idle = [0x10, 0x00, 0xff, 0xff, 0, 0, 0, 0];
    let clear = |addr: u32, len: usize| len == 0 || addr >= 8;
    let clear = clear(v.fb_addr, 1) && clear(v.cmd_addr, 1) && clear(v.preload_addr, v.preload.len());
    assert!(clear, "RDRAM 0 is the loop's");
    for (addr, bytes) in [(v.preload_addr, &v.preload[..]), (v.cmd_addr, &v.cmds[..]), (0, &idle[..])] {
        set(&mut code, T1, 0xa000_0000 | addr);
        for (i, chunk) in bytes.chunks(4).enumerate() {
            let mut word = [0; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            set(&mut code, T0, u32::from_be_bytes(word));
            code.push(sw(T0, 4 * i as u32, T1));
        }
    }
    set(&mut code, T1, 0xa410_0000);
    for (register, value) in [(3, 0x15), (0, v.cmd_addr), (1, v.cmd_addr + v.cmds.len() as u32)] {
        set(&mut code, T0, value);
        code.push(sw(T0, 4 * register, T1));
    }
    set(&mut code, T0, 0x8000_0000);
    code.extend([jr(T0), 0]);
    let mut rom = vec![0u8; 0x1000];
    rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
    rom[0x3e] = b'E';
    assert!(0x40 + 4 * code.len() <= rom.len(), "the boot code does not fit in DMEM");
    for (i, word) in code.iter().enumerate() {
        rom[0x40 + 4 * i..0x44 + 4 * i].copy_from_slice(&word.to_be_bytes());
    }
    rom
}

/// The three ways a vector is drawn, all of which must agree: the reference
/// rasteriser on the machine's thread, the accelerated one there, and the
/// accelerated one on three drawing threads (each its own lines).
const WAYS: [(&str, bool, usize); 3] = [("reference", false, 0), ("accelerated", true, 0), ("three threads", true, 3)];

/// Draws a vector one way: the frame buffer's bytes after a field.
fn draw(v: &Vector, rom: &[u8], accelerated: bool, threads: usize) -> Vec<u8> {
    let mut n64 = N64::new(rom).unwrap();
    n64.set_rdp_acceleration(accelerated);
    n64.set_rdp_thread(threads > 0);
    n64.set_rdp_drawers(threads.max(1));
    n64.run_frame(0);
    (0..(v.width * v.height * v.bpp) as u32).map(|i| n64.peek(v.fb_addr + i)).collect()
}

/// The pixels of a frame buffer, as numbers.
fn pixels<'a>(v: &Vector, fb: &'a [u8]) -> impl Iterator<Item = u32> + 'a {
    fb.chunks(v.bpp).map(|p| p.iter().fold(0u32, |w, &b| w << 8 | u32::from(b)))
}

/// Whether a pixel both draw differs in more than its coverage bits (bit 0
/// of a 16-bit pixel, bits 5 to 7 of a 32-bit one). Undrawn pixels are 0.
fn colors_differ(v: &Vector, got: &[u8], expected: &[u8]) -> bool {
    let coverage = if v.bpp == 2 { 1 } else { 0xe0 };
    pixels(v, got).zip(pixels(v, expected)).any(|(a, b)| a != 0 && b != 0 && (a ^ b) & !coverage != 0)
}

/// Where two frame buffers differ: how many pixels, the first few, and
/// both buffers side by side (ours, then the other).
fn differences(v: &Vector, got: &[u8], expected: &[u8]) -> String {
    let (got, expected): (Vec<u32>, Vec<u32>) = (pixels(v, got).collect(), pixels(v, expected).collect());
    let digits = 2 * v.bpp;
    let differing: Vec<usize> = (0..got.len()).filter(|&i| got[i] != expected[i]).collect();
    let mut text = format!("{} pixels differ:", differing.len());
    for &i in differing.iter().take(4) {
        let (x, y) = (i % v.width, i / v.width);
        write!(text, " ({x},{y}) {:0digits$x}, not {:0digits$x};", got[i], expected[i]).unwrap();
    }
    for row in 0..v.height {
        let line = |fb: &[u32]| {
            fb[row * v.width..(row + 1) * v.width].iter().map(|p| format!(" {p:0digits$x}")).collect::<String>()
        };
        write!(text, "\n   {}   |  {}", line(&got), line(&expected)).unwrap();
    }
    text
}

/// Runs every vector in a directory of the collection (sorted by name).
fn suite(dir: &str) {
    let dir = std::env::var("N64_RDP_VECTORS").unwrap_or_else(|_| VECTORS.into()) + dir;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("skipped: run tests/roms/fetch.sh (no {dir})");
        return;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".rvec").map(String::from))
        .collect();
    names.sort();
    let (mut failed, mut matched, mut expected) = (Vec::new(), 0, 0);
    for name in &names {
        let v = match parse(&std::fs::read(format!("{dir}/{name}.rvec")).unwrap()) {
            Ok(v) => v,
            Err(error) => {
                failed.push(format!("{name}: {error}"));
                continue;
            }
        };
        let rom = cartridge(&v);
        let drawn: Vec<Vec<u8>> =
            WAYS.iter().map(|&(_, accelerated, threads)| draw(&v, &rom, accelerated, threads)).collect();
        for (way, fb) in WAYS.iter().zip(&drawn).skip(1) {
            if *fb != drawn[0] {
                let what = differences(&v, fb, &drawn[0]);
                failed.push(format!("{name}: {} differs from the reference: {what}", way.0));
            }
        }
        let fb = &drawn[0];
        let modes = format!("other modes {:014x}", v.other_modes);
        let listed = EXPECTED.iter().find(|(n, ..)| n == name);
        let colors = colors_differ(&v, fb, &v.golden);
        match listed {
            None if *fb == v.golden => matched += 1,
            None => failed.push(format!("{name} ({modes}) differs from angrylion: {}", differences(&v, fb, &v.golden))),
            Some((_, _, reason)) if *fb == v.golden => {
                failed.push(format!("{name}: now matches angrylion; take it off EXPECTED (it was: {reason})"))
            }
            Some((_, Edges, reason)) if colors => failed.push(format!(
                "{name} ({modes}): expected to differ only at edges ({reason}), but colors differ: {}",
                differences(&v, fb, &v.golden)
            )),
            Some((_, Colors, reason)) if !colors => failed.push(format!(
                "{name}: expected to differ in color ({reason}), but now only at edges; mark it Edges: {}",
                differences(&v, fb, &v.golden)
            )),
            Some(_) => expected += 1,
        }
    }
    eprintln!("{dir}: {} vectors, {matched} match angrylion, {expected} differ as expected", names.len());
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

/// The hand-written vectors, each about one feature.
#[test]
fn vectors() {
    suite("");
    // A name on the list that is not in the collection is a mistake.
    let dir = std::env::var("N64_RDP_VECTORS").unwrap_or_else(|_| VECTORS.into());
    if std::fs::metadata(&dir).is_ok() {
        for (name, ..) in EXPECTED {
            let path = format!("{dir}/{}{name}.rvec", if name.starts_with("fz_") { "fuzz/" } else { "" });
            assert!(std::fs::metadata(&path).is_ok(), "{name} is on EXPECTED but not in the collection");
        }
    }
}

/// Fill rectangles (also clipped by the scissor) and fill triangles.
#[test]
fn fuzz() {
    suite("/fuzz");
}
