//! Where a program is stuck: runs a disc (or a PS-X EXE) to a pc, or for
//! some frames, and prints what it was doing, for a disassembler.
//!
//!   cargo run --release --example trace-psx -- BIOS.BIN|EXE|- DISC.chd|- PC|fFRAMES [KEEP]
//!
//! With - for the BIOS, the disc starts with the core's own.
//! With a pc (hex), it stops there and prints the last KEEP instructions'
//! pc and registers; with fFRAMES it runs that many frames and prints the
//! hottest pcs of the next 300 000 instructions. Either way it writes all
//! of RAM to $TMPDIR/psx-ram.bin; to disassemble it:
//!
//!   llvm-objcopy -I binary -O elf32-tradlittlemips psx-ram.bin ram.elf
//!   llvm-objdump -D --triple=mipsel --mcpu=mips1 --adjust-vma=0x80000000 ram.elf

use std::collections::VecDeque;

struct Chd(disc::ChdDisc);

impl psx::Disc for Chd {
    fn read(&mut self, lba: u32, sector: &mut [u8; 2352]) -> bool {
        self.0.read(lba, sector)
    }
    fn tracks(&self) -> Vec<psx::Track> {
        self.0.tracks().iter().map(|t| psx::Track { start: t.start, audio: t.audio }).collect()
    }
    fn end(&self) -> u32 {
        self.0.end()
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut psx = if args[1].ends_with(".exe") {
        psx::Psx::with_exe(&std::fs::read(&args[1]).unwrap()).unwrap()
    } else if args[1] == "-" {
        psx::Psx::with_disc(Chd(disc::ChdDisc::open(&args[2]).unwrap())).unwrap()
    } else {
        psx::Psx::new(&std::fs::read(&args[1]).unwrap()).unwrap()
    };
    if args[2] != "-" && args[1] != "-" {
        psx.insert_disc(Chd(disc::ChdDisc::open(&args[2]).unwrap()));
    }
    let keep: usize = args.get(4).map_or(40, |k| k.parse().unwrap());
    let mut last = VecDeque::new();
    if let Some(frames) = args[3].strip_prefix('f') {
        for _ in 0..frames.parse::<u32>().unwrap() {
            psx.run_frame();
        }
        // Where it spends its time now: the hottest pcs.
        let mut hot = std::collections::HashMap::<u32, u32>::new();
        for _ in 0..300_000 {
            *hot.entry(psx.registers().pc).or_default() += 1;
            psx.step_instruction();
        }
        let mut hot: Vec<_> = hot.into_iter().collect();
        hot.sort_by_key(|&(pc, n)| (std::cmp::Reverse(n), pc));
        for (pc, n) in hot.iter().take(20) {
            println!("hot {pc:08x} {n}");
        }
    } else {
        let target = u32::from_str_radix(&args[3], 16).unwrap();
        for _ in 0..4_000_000_000u64 {
            let r = psx.registers();
            last.push_back(r);
            if last.len() > keep {
                last.pop_front();
            }
            if r.pc == target {
                break;
            }
            psx.step_instruction();
        }
    }
    for r in &last {
        println!("{:08x} ra {:08x} v0 {:08x} v1 {:08x} a0 {:08x} a1 {:08x} a2 {:08x}", r.pc, r.r[31], r.r[2], r.r[3], r.r[4], r.r[5], r.r[6]);
    }
    let ram: Vec<u8> = (0..0x20_0000u32 / 4).flat_map(|i| psx.peek32(0x8000_0000 + 4 * i).to_le_bytes()).collect();
    std::fs::write(std::env::temp_dir().join("psx-ram.bin"), ram).unwrap();
}
