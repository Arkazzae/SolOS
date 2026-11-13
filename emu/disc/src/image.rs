//! A PlayStation disc kept as one plain file (.iso, .img, .mdf, or a lone .bin), or inside an
//! EBOOT.PBP as the PSP's PlayStation emulator takes it, by LBA as a CHD's are.
//!
//! A plain file is one data track: its raw sectors, or its 2048 bytes of data each, given back the
//! rest of a Mode 2 sector (what an XA stream had is not in such a file). Which of the two it is
//! shows at its start, a raw sector's sync pattern, then in its size. An EBOOT's disc is in blocks
//! of 16 raw sectors, each deflated by itself, found through its index; its table of contents has
//! the tracks. One sold on the PSN is encrypted, and is refused.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::{Error, SECTOR, write_header};

const COOKED: usize = 2048;
const SYNC: [u8; 12] = [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0];

/// A track: the LBA of its index 1, and whether it is sound.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageTrack {
    pub start: u32,
    pub audio: bool,
}

/// A disc image with its sectors read one by one.
pub trait Image: Send {
    /// A raw sector; false past the end, or where it cannot be read.
    fn read(&mut self, lba: u32, sector: &mut [u8; SECTOR]) -> bool;
    fn tracks(&self) -> Vec<ImageTrack>;
    /// The LBA past the last sector.
    fn end(&self) -> u32;
}

/// The disc in a plain image or an EBOOT.PBP, by the file's extension.
pub fn image(path: &Path) -> Result<Box<dyn Image>, Error> {
    let eboot = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pbp"));
    Ok(if eboot { Box::new(Pbp::open(path)?) } else { Box::new(Flat::open(path)?) })
}

struct Flat {
    file: File,
    cooked: bool,
    sectors: u32,
}

impl Flat {
    fn open(path: &Path) -> Result<Flat, Error> {
        let mut file = File::open(path).map_err(Error::Io)?;
        let size = file.metadata().map_err(Error::Io)?.len();
        let mut head = [0; 12];
        file.read_exact(&mut head).map_err(Error::Io)?;
        let cooked = match () {
            () if head == SYNC => false,
            () if size % COOKED as u64 == 0 => true,
            () if size % SECTOR as u64 == 0 => false,
            () => return Err(Error::Invalid("not a disc image: its size is no number of sectors")),
        };
        let sectors = u32::try_from(size / if cooked { COOKED } else { SECTOR } as u64).map_err(|_| Error::Invalid("too big for a disc"))?;
        Ok(Flat { file, cooked, sectors })
    }
}

impl Image for Flat {
    fn read(&mut self, lba: u32, sector: &mut [u8; SECTOR]) -> bool {
        if lba >= self.sectors {
            sector.fill(0);
            return false;
        }
        let size = if self.cooked { COOKED } else { SECTOR };
        if self.file.seek(SeekFrom::Start(u64::from(lba) * size as u64)).is_err() {
            return false;
        }
        if !self.cooked {
            return self.file.read_exact(sector).is_ok();
        }
        write_header(sector, lba, 2);
        // The subheader of a data sector, Form 1, twice.
        sector[18] = 8;
        sector[22] = 8;
        self.file.read_exact(&mut sector[24..24 + COOKED]).is_ok()
    }

    fn tracks(&self) -> Vec<ImageTrack> {
        vec![ImageTrack { start: 0, audio: false }]
    }

    fn end(&self) -> u32 {
        self.sectors
    }
}

const BLOCK_SECTORS: u32 = 16;
const BLOCK: usize = BLOCK_SECTORS as usize * SECTOR;
const TOC: u64 = 0x800;
const ENCRYPTED: u64 = 0x400;
const INDEX: u64 = 0x4000;
const INDEX_ENTRY: usize = 32;
const DATA: u64 = 0x10_0000;
/// Where a multi-disc EBOOT lists its discs, from its PSAR.
const DISCS: u64 = 0x200;
/// The two seconds before LBA 0, which a table of contents counts.
const LEAD_IN: u32 = 150;

struct Pbp {
    file: File,
    /// Where the blocks' offsets count from.
    base: u64,
    /// Each block's offset and packed size.
    blocks: Vec<(u32, u32)>,
    tracks: Vec<ImageTrack>,
    end: u32,
    block: Vec<u8>,
    at: Option<u32>,
}

fn bcd(value: u8) -> u32 {
    u32::from(value >> 4) * 10 + u32::from(value & 15)
}

/// The sectors a table of contents' minutes, seconds and frames count, its lead-in included.
fn sectors(msf: &[u8]) -> u32 {
    (bcd(msf[0]) * 60 + bcd(msf[1])) * 75 + bcd(msf[2])
}

impl Pbp {
    fn open(path: &Path) -> Result<Pbp, Error> {
        let mut file = File::open(path).map_err(Error::Io)?;
        let mut head = [0; 40];
        read_at(&mut file, 0, &mut head)?;
        if &head[..4] != b"\0PBP" {
            return Err(Error::Invalid("not an EBOOT.PBP"));
        }
        let psar = u64::from(u32::from_le_bytes([head[36], head[37], head[38], head[39]]));
        let mut magic = [0; 16];
        read_at(&mut file, psar, &mut magic)?;
        // A game on several discs lists them; the first is the one started.
        let disc = if magic.starts_with(b"PSTITLEIMG") {
            let mut first = [0; 4];
            read_at(&mut file, psar + DISCS, &mut first)?;
            let disc = psar + u64::from(u32::from_le_bytes(first));
            read_at(&mut file, disc, &mut magic)?;
            disc
        } else {
            psar
        };
        if !magic.starts_with(b"PSISOIMG") {
            return Err(Error::Invalid("no PlayStation disc in this EBOOT.PBP"));
        }
        let mut sealed = [0; 4];
        read_at(&mut file, disc + ENCRYPTED, &mut sealed)?;
        if &sealed == b"\0PGD" {
            return Err(Error::Invalid("this EBOOT.PBP is encrypted"));
        }
        let mut toc = [0; 10 * 102];
        read_at(&mut file, disc + TOC, &mut toc)?;
        let entries: Vec<&[u8]> = toc.chunks_exact(10).collect();
        // Three entries say the first track, the last and the lead-out, the tracks follow them.
        let last = bcd(entries[1][7]) as usize;
        if last == 0 || last + 3 > entries.len() {
            return Err(Error::Invalid("a damaged EBOOT.PBP: its table of contents"));
        }
        let at = |entry: &[u8]| sectors(&entry[7..10]).saturating_sub(LEAD_IN);
        let tracks = entries[3..3 + last].iter().map(|e| ImageTrack { start: at(e), audio: e[0] == 1 }).collect();
        let end = at(entries[2]);

        let mut index = vec![0; (DATA - INDEX) as usize];
        read_at(&mut file, disc + INDEX, &mut index)?;
        let word = |entry: &[u8], at: usize| u32::from_le_bytes([entry[at], entry[at + 1], entry[at + 2], entry[at + 3]]);
        let blocks: Vec<(u32, u32)> =
            index.chunks_exact(INDEX_ENTRY).map(|e| (word(e, 0), word(e, 4))).take_while(|&(_, size)| size != 0).collect();
        Ok(Pbp { file, base: disc + DATA, blocks, tracks, end, block: vec![0; BLOCK], at: None })
    }

    /// Unpacks a block into `block`, unless it is there.
    fn load(&mut self, number: u32) -> Option<()> {
        if self.at == Some(number) {
            return Some(());
        }
        self.at = None;
        let &(offset, size) = self.blocks.get(number as usize)?;
        let mut packed = vec![0; size as usize];
        self.file.seek(SeekFrom::Start(self.base + u64::from(offset))).ok()?;
        self.file.read_exact(&mut packed).ok()?;
        if packed.len() >= BLOCK {
            self.block.copy_from_slice(&packed[..BLOCK]);
        } else {
            let bytes = miniz_oxide::inflate::decompress_to_vec_with_limit(&packed, BLOCK).ok()?;
            self.block.fill(0);
            self.block[..bytes.len()].copy_from_slice(&bytes);
        }
        self.at = Some(number);
        Some(())
    }
}

impl Image for Pbp {
    fn read(&mut self, lba: u32, sector: &mut [u8; SECTOR]) -> bool {
        if lba >= self.end || self.load(lba / BLOCK_SECTORS).is_none() {
            sector.fill(0);
            return false;
        }
        let at = (lba % BLOCK_SECTORS) as usize * SECTOR;
        sector.copy_from_slice(&self.block[at..at + SECTOR]);
        true
    }

    fn tracks(&self) -> Vec<ImageTrack> {
        self.tracks.clone()
    }

    fn end(&self) -> u32 {
        self.end
    }
}

fn read_at(file: &mut File, at: u64, bytes: &mut [u8]) -> Result<(), Error> {
    file.seek(SeekFrom::Start(at)).and_then(|_| file.read_exact(bytes)).map_err(Error::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sol-disc-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// 40 raw sectors, each filled with its LBA.
    fn raw_sectors() -> Vec<u8> {
        let mut image = Vec::new();
        for lba in 0..40u8 {
            let mut sector = [lba; SECTOR];
            write_header(&mut sector, u32::from(lba), 2);
            sector[24..].fill(lba);
            image.extend(sector);
        }
        image
    }

    #[test]
    fn a_raw_image() {
        let image = raw_sectors();
        let mut disc = super::image(&written("raw.img", &image)).unwrap();
        assert_eq!((disc.end(), disc.tracks()), (40, vec![ImageTrack { start: 0, audio: false }]));
        let mut sector = [0; SECTOR];
        assert!(disc.read(39, &mut sector));
        assert_eq!(sector[..], image[39 * SECTOR..]);
        assert!(!disc.read(40, &mut sector));
    }

    #[test]
    fn an_image_of_data_only_is_given_its_sector_back() {
        let image: Vec<u8> = (0..10u8).flat_map(|lba| [lba; COOKED]).collect();
        let mut disc = super::image(&written("cooked.iso", &image)).unwrap();
        assert_eq!(disc.end(), 10);
        let mut sector = [0xaa; SECTOR];
        assert!(disc.read(3, &mut sector));
        assert_eq!(sector[..12], SYNC);
        assert_eq!(sector[12..16], [0, 2, 3, 2]);
        assert_eq!(sector[16..24], [0, 0, 8, 0, 0, 0, 8, 0]);
        assert_eq!(sector[24..24 + COOKED], [3; COOKED]);
    }

    #[test]
    fn a_file_no_number_of_sectors_long_is_refused() {
        assert!(super::image(&written("odd.img", &[1; 5000])).is_err());
    }

    /// An EBOOT as popstation writes one, of these raw sectors, a data track and a sound one.
    fn eboot(raw: &[u8], packed: bool) -> Vec<u8> {
        const PSAR: usize = 0x1000;
        let mut file = vec![0; PSAR + DATA as usize];
        file[..4].copy_from_slice(b"\0PBP");
        file[36..40].copy_from_slice(&(PSAR as u32).to_le_bytes());
        file[PSAR..PSAR + 12].copy_from_slice(b"PSISOIMG0000");
        let entry = |file: &mut Vec<u8>, i: usize, kind: u8, msf: [u8; 3]| {
            let at = PSAR + TOC as usize + i * 10;
            file[at] = kind;
            file[at + 7..at + 10].copy_from_slice(&msf);
        };
        // The last track is 2; the lead-out at 00:02:40 (LBA 40).
        entry(&mut file, 1, 0x41, [2, 0, 0]);
        entry(&mut file, 2, 0x41, [0, 2, 0x40]);
        entry(&mut file, 3, 0x41, [0, 2, 0]);
        entry(&mut file, 4, 0x01, [0, 2, 0x32]);
        let mut offset = 0u32;
        for (i, block) in raw.chunks(BLOCK).enumerate() {
            let bytes = if packed { miniz_oxide::deflate::compress_to_vec(block, 6) } else { block.to_vec() };
            let at = PSAR + INDEX as usize + i * INDEX_ENTRY;
            file[at..at + 4].copy_from_slice(&offset.to_le_bytes());
            file[at + 4..at + 8].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
            file.extend(&bytes);
            offset += bytes.len() as u32;
        }
        file
    }

    #[test]
    fn an_eboots_disc_stored_or_deflated() {
        let raw = raw_sectors();
        for packed in [true, false] {
            // A block stored whole is one of 16 sectors exactly, so the disc is cut to two of them.
            let raw = if packed { &raw[..] } else { &raw[..32 * SECTOR] };
            let mut disc = super::image(&written(&format!("EBOOT{packed}.PBP"), &eboot(raw, packed))).unwrap();
            assert_eq!(disc.end(), 40);
            assert_eq!(disc.tracks(), [ImageTrack { start: 0, audio: false }, ImageTrack { start: 32, audio: true }]);
            let mut sector = [0; SECTOR];
            for lba in [0, 17, 31] {
                assert!(disc.read(lba, &mut sector), "{lba}");
                assert_eq!(sector[..], raw[lba as usize * SECTOR..(lba as usize + 1) * SECTOR], "{lba}");
            }
        }
    }

    #[test]
    fn an_encrypted_eboot_is_refused() {
        let mut file = eboot(&raw_sectors(), true);
        file[0x1000 + ENCRYPTED as usize..0x1000 + ENCRYPTED as usize + 4].copy_from_slice(b"\0PGD");
        let refused = super::image(&written("sealed.pbp", &file)).err().unwrap();
        assert!(format!("{refused:?}").contains("encrypted"));
    }
}
