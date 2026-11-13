//! Prints memory the CPU sees, and the state, after some frames.
//!
//!   cargo run --release --example peek -- ROM FRAMES ADDR LEN

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rom = std::fs::read(&args[1]).expect("cannot read the ROM");
    let frames: u32 = args[2].parse().unwrap();
    let addr = u16::from_str_radix(args[3].trim_start_matches('$'), 16).unwrap();
    let len: u16 = args[4].parse().unwrap();
    let mut nes = nes::Nes::new(&rom).unwrap();
    for frame in 1..=frames {
        nes.run_frame([0, 0]);
        if frame + 5 > frames {
            println!("frame {frame}: {}", nes.debug());
        }
    }
    for row in (0..len).step_by(16) {
        let bytes: Vec<String> = (0..16.min(len - row)).map(|i| format!("{:02X}", nes.peek(addr + row + i))).collect();
        println!("{:04X}: {}", addr + row, bytes.join(" "));
    }
}
