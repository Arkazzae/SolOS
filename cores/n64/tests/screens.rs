//! Demos of krom's that draw with the CPU or the RDP into 16- and 32-bit
//! frame buffers, judged by the picture the video interface shows
//! (frame()), by a hash of its pixels. Their pictures in the collection
//! were not taken from a console (16-bit colours are widened differently,
//! the fractals' single and double precision pictures are one file, a fill
//! color comes out other than the one set), so the hashes are of this
//! core's pictures, each looked at and compared with those. A picture that
//! differs is saved next to the test binaries.

use n64::N64;
use std::io::Write;

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/krom/");

/// FNV-1a over the picture's size and pixels.
fn hash(pixels: &[u32], width: usize, height: usize) -> u64 {
    [width as u32, height as u32]
        .iter()
        .chain(pixels)
        .fold(0xcbf2_9ce4_8422_2325, |h, &p| (h ^ u64::from(p)).wrapping_mul(0x100_0000_01b3))
}

fn suite(roms: &[(&str, u32, u64)]) {
    let mut failed = Vec::new();
    for &(path, frames, expected) in roms {
        let Ok(rom) = std::fs::read(format!("{ROMS}{path}")) else {
            eprintln!("skipped: run tests/roms/fetch.sh");
            return;
        };
        let mut n64 = N64::new(&rom).unwrap();
        // The picture of the last field itself, not one the drawing threads
        // took a field or more behind.
        n64.set_rdp_exact_pictures(true);
        for _ in 0..frames {
            n64.run_frame(0);
        }
        let (pixels, width, height) = n64.frame();
        let got = hash(pixels, width, height);
        if got == expected {
            eprintln!("{path}: passed");
            continue;
        }
        let out = format!("{}/{}.ppm", env!("CARGO_TARGET_TMPDIR"), path.replace('/', "_"));
        let mut file = std::fs::File::create(&out).unwrap();
        write!(file, "P6\n{width} {height}\n255\n").unwrap();
        for &p in pixels {
            file.write_all(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]).unwrap();
        }
        failed.push(format!("{path}: {got:#018x} ({width} x {height}), saved to {out}"));
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

#[test]
fn frame_buffers() {
    suite(&[
        ("FrameBuffer/16BPP/FrameBufferCPU320x240/FrameBufferCPU16BPP320X240.N64", 30, 0x00c2_60d8_c139_366f),
        ("FrameBuffer/32BPP/FrameBufferCPU640x480/FrameBufferCPU32BPP640X480.N64", 30, 0x4fd2_91e8_1c4b_02f7),
        ("HelloWorld/16BPP/HelloWorldCPU320x240/HelloWorldCPU16BPP320X240.N64", 30, 0x14ec_0e62_2eca_d6da),
        ("HelloWorld/32BPP/HelloWorldCPU320x240/HelloWorldCPU32BPP320X240.N64", 30, 0x14ec_0e62_2eca_d6da),
    ]);
}

/// The Mandelbrot set, drawn by the FPU in single and in double precision.
#[test]
fn fractals() {
    suite(&[
        ("CP1/Fractal/32BPP/320X240/Mandelbrot/Single/Mandelbrot32BPP320X240.N64", 120, 0x537c_0ba2_7cca_3ef8),
        ("CP1/Fractal/32BPP/320X240/Mandelbrot/Double/Mandelbrot32BPP320X240.N64", 120, 0x4208_c736_e69c_6da2),
    ]);
}

/// The RDP: krom's demos of fill, copy and 1-cycle rectangles and
/// triangles, flat, shaded, depth tested and textured in every format
/// (with palettes), into 16-bit frame buffers and one 32-bit one. Each sits
/// in a directory of its own under RDP/, found by its name.
#[test]
fn rdp() {
    let demos = [
        ("FillRectangle16BPP320X240", 60, 0x7702_4e4d_729b_d809),
        ("Cycle1FillRectangle16BPP320X240", 60, 0x130e_879d_b3ab_0dce),
        ("Cycle1FillZBufferRectangle16BPP320X240", 60, 0xfb0a_93a2_d5c4_87b6),
        ("FillTriangle16BPP320X240", 60, 0xd7d9_52b2_b760_e09c),
        ("Cycle1FillTriangle16BPP320X240", 60, 0x0ea4_9b7f_b407_c950),
        ("Cycle1ShadeTriangle16BPP320X240", 60, 0x31c5_302b_8265_2cd1),
        ("Cycle1FillZBufferTriangle16BPP320X240", 60, 0x07ba_f90b_a4ab_10b6),
        ("CubeFillTriangle16BPP320X240", 60, 0xec02_a65d_60ab_6510),
        ("LeftMajorTriangle16BPP320X240", 60, 0x33de_9070_e199_a8b6),
        ("RightMajorTriangle16BPP320X240", 60, 0x33de_9070_e199_a8b6),
        ("CopyTextureRectangle16BPPRGBA16B320X240", 60, 0x78a7_3613_4ba0_0850),
        ("Cycle1TextureRectangle16BPPRGBA16B320X240", 60, 0x45a1_4a4c_9211_79ec),
        ("Cycle1TextureRectangle16BPPRGBA32B320X240", 60, 0xbd2d_c767_7251_79ec),
        ("Cycle1TextureRectangle16BPPIA16B320X240", 60, 0x7a51_a706_9687_613c),
        ("Cycle1TextureRectangle16BPPIA8B320X240", 60, 0x8c73_cca7_6e56_aec2),
        ("Cycle1TextureRectangle16BPPIA4B320X240", 60, 0x3ad1_70c7_9332_da8f),
        ("Cycle1TextureRectangle16BPPI8B320X240", 60, 0x0a4d_6b3b_9a9e_0efc),
        ("Cycle1TextureRectangle16BPPI4B320X240", 60, 0x8d5e_8a5b_5ef7_3290),
        ("CopyTextureRectangle16BPPTLUTRGBA4B320X240", 60, 0x24c9_c368_9b98_dee0),
        ("CopyTextureRectangle16BPPTLUTRGBA8B320X240", 60, 0x78a7_3613_4ba0_0850),
        ("Cycle1TextureRectangle16BPPTLUTIA4B320X240", 60, 0x3e2d_3812_554a_bcac),
        ("Cycle1TextureRectangle16BPPTLUTIA8B320X240", 60, 0x7a51_a706_9687_613c),
        ("Cycle1TextureRectangle16BPPTLUTRGBA4B320X240", 60, 0x43ee_47ff_ec6b_bbd7),
        ("Cycle1TextureRectangle16BPPTLUTRGBA8B320X240", 60, 0x45a1_4a4c_9211_79ec),
        ("CopyTextureTriangle16BPPRGBA16B320X240", 60, 0x4875_a092_85f0_e350),
        ("Cycle1TextureTriangle16BPPRGBA16B320X240", 60, 0x6ca7_f2a6_dcb9_18a3),
        ("Cycle1TextureTriangle16BPPRGBA32B320X240", 60, 0xef7b_043a_1389_18a3),
        ("Cycle1ShadeTriangle32BPP320X240", 60, 0x1478_628e_88b3_d40a),
    ];
    let paths: Vec<(String, u32, u64)> = demos
        .iter()
        .filter_map(|&(name, frames, hash)| Some((find(&format!("{ROMS}RDP"), &format!("{name}.N64"))?, frames, hash)))
        .collect();
    if paths.len() < demos.len() {
        eprintln!("skipped: run tests/roms/fetch.sh");
        return;
    }
    let roms: Vec<_> = paths.iter().map(|(path, frames, hash)| (path.as_str(), *frames, *hash)).collect();
    suite(&roms);
}

/// The RDP fed from DMEM (the XBUS) by the RSP: depth-tested rectangles in
/// 1-cycle mode at primitive depths, over a depth image cleared by fill.
#[test]
fn rdp_from_dmem() {
    suite(&[("RSPTest/XBUS/RSPXBUSRDP.N64", 60, 0xfb0a_93a2_d5c4_87b6)]);
}

/// Where under krom/ a file of this name is, looking through a directory.
fn find(dir: &str, name: &str) -> Option<String> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find(path.to_str()?, name) {
                return Some(found);
            }
        } else if path.file_name()?.to_str()? == name {
            return Some(path.to_str()?.strip_prefix(ROMS)?.to_string());
        }
    }
    None
}
