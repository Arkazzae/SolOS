//! Prints the CPU's and PPU's state at the end of some frames, to see where
//! a ROM is stuck.
//!
//!   cargo run --release --example trace -- ROM FRAMES [EVERY]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rom = std::fs::read(&args[1]).expect("cannot read the ROM");
    let frames: u32 = args[2].parse().expect("FRAMES is a number");
    let every: u32 = args.get(3).map_or(60, |n| n.parse().expect("EVERY is a number"));
    let mut nes = nes::Nes::new(&rom).unwrap();
    for frame in 1..=frames {
        nes.run_frame([0, 0]);
        if frame % every == 0 {
            println!("frame {frame}: {}", nes.debug());
        }
    }
}
