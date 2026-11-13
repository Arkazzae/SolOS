//! Writes a CHD's sectors as one raw image, for checking the other kinds of disc image against it:
//! `dump-raw GAME.chd OUT.bin`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [from, to] = &args[..] else { panic!("usage: dump-raw GAME.chd OUT.bin") };
    let mut disc = disc::ChdDisc::open(from).unwrap();
    let mut out = std::io::BufWriter::new(std::fs::File::create(to).unwrap());
    let mut sector = [0; 2352];
    for lba in 0..disc.end() {
        assert!(disc.read(lba, &mut sector), "sector {lba}");
        std::io::Write::write_all(&mut out, &sector).unwrap();
    }
    for track in disc.tracks() {
        println!("track at {} audio {}", track.start, track.audio);
    }
    println!("{} sectors", disc.end());
}
