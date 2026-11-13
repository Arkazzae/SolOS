//! Prints a sector's header and the start of its data:
//!   cargo run --release -p disc --example sector -- IMAGE.chd LBA...
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut disc = disc::ChdDisc::open(&args[1]).unwrap_or_else(|e| panic!("{}: {e}", args[1]));
    println!("{:?}, end {}", disc.tracks(), disc.end());
    for lba in &args[2..] {
        let lba: u32 = lba.parse().expect("LBA is a number");
        let mut sector = [0; 2352];
        let ok = disc.read(lba, &mut sector);
        println!("{lba}: {ok} sync {:02x?} header {:02x?} sub {:02x?}", &sector[..12], &sector[12..16], &sector[16..24]);
        println!("  data {:02x?}", &sector[24..56]);
        if std::env::var_os("SECTOR_DUMP").is_some() {
            std::fs::write(format!("sector-{lba}.bin"), sector).unwrap();
        }
    }
}
