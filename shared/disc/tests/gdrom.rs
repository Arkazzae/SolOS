//! A Dreamcast GD-ROM in a CHD (MAME's CHGD tracks), checked on a real disc: Crazy Taxi.
//! The discs are dev data: set SOL_DC_DISCS to their folder, or put them in ~/dc-roms; without
//! crazy-taxi.chd the test only says it was skipped.

use disc::ChdDisc;
use std::path::PathBuf;

fn disc_path() -> Option<PathBuf> {
    let dir = match std::env::var_os("SOL_DC_DISCS") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME")?).join("dc-roms"),
    };
    let path = dir.join("crazy-taxi.chd");
    path.is_file().then_some(path)
}

#[test]
fn a_gd_rom_opens_with_its_high_density_area_at_lba_45000() {
    let Some(path) = disc_path() else {
        eprintln!("crazy-taxi.chd not found (SOL_DC_DISCS): skipped");
        return;
    };
    let mut disc = ChdDisc::open(path).unwrap();
    let tracks = disc.tracks();
    assert_eq!(tracks.len(), 3);
    assert_eq!((tracks[0].start, tracks[0].audio), (0, false));
    assert_eq!((tracks[1].start, tracks[1].audio), (450, true));
    assert_eq!((tracks[2].start, tracks[2].audio), (45000, false));
    assert_eq!(disc.end(), 549150);
    // The first sector of the high-density area is IP.BIN's: a raw sector, 12 bytes of sync and 4 of
    // header, then 2048 bytes of data that start with the hardware ID.
    let mut sector = [0u8; 2352];
    assert!(disc.read(45000, &mut sector));
    assert_eq!(&sector[..12], &[0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 0]);
    assert_eq!(&sector[16..32], b"SEGA SEGAKATANA ");
    // The ISO 9660 volume descriptor is sector 16 of the track, an absolute disc LBA.
    assert!(disc.read(45016, &mut sector));
    assert_eq!((sector[16], &sector[17..22]), (1, &b"CD001"[..]));
    assert!(!disc.read(549150, &mut sector));
}
