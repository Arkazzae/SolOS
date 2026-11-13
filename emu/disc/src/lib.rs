//! Disc images for the cores that read discs: a CHD (as MAME's chdman
//! makes them, how the console's games are kept) opened as raw 2352-byte
//! sectors by LBA, with its tracks. The decompression is the `chd`
//! crate's; the layout of a CD in it is here.
//!
//! A CD in a CHD is a list of tracks (CHT2 metadata), each stored as
//! frames of 2448 bytes (the sector and 96 of subcode), padded to a
//! multiple of four frames, many frames to a hunk. A track's pregap is
//! stored only when its type starts with V; else it is silence before the
//! track. Audio is stored with its samples big-endian.
//!
//! A Dreamcast GD-ROM (CHGD metadata, the same fields) is laid out the same
//! way, each track's padding frames counted in its FRAMES, so the
//! high-density area starts at LBA 45000.
//!
//! GameCube discs (ISO/GCM and RVZ) are read by byte offset in [`gamecube`].

pub mod gamecube;
mod image;

pub use image::{Image, ImageTrack, image};

use chd::metadata::KnownMetadata;
use chd::{Chd, header::Header};
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::Path;

const FRAME: usize = 2448;
const SECTOR: usize = 2352;
// CHDs made by chdman normally use a few dozen frames per hunk. Keep malformed
// V5 headers from allocating gigabytes or supplying a partial/zero CD frame.
const MAX_HUNK: u32 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Track {
    /// The LBA of index 1, where the track starts.
    pub start: u32,
    pub audio: bool,
    /// The LBA of its first stored frame (its pregap's, when stored), and
    /// how many it has.
    data: u32,
    frames: u32,
    /// Its first frame in the CHD.
    chd_frame: u32,
    /// Sectors stored as 2048 bytes of data (MODE1), to be given a header.
    cooked: bool,
}

pub struct ChdDisc {
    chd: Chd<BufReader<File>>,
    tracks: Vec<Track>,
    end: u32,
    frames_per_hunk: u32,
    /// The last hunk decompressed, and which it is.
    hunk: Vec<u8>,
    hunk_number: Option<u32>,
    compressed: Vec<u8>,
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Chd(chd::Error),
    /// Not a CD: no track metadata, or tracks it cannot read.
    NotCd,
    Invalid(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Chd(e) => write!(f, "{e}"),
            Error::NotCd => write!(f, "not a CD image"),
            Error::Invalid(reason) => write!(f, "invalid CD image: {reason}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<chd::Error> for Error {
    fn from(e: chd::Error) -> Error {
        Error::Chd(e)
    }
}

impl ChdDisc {
    pub fn open(path: impl AsRef<Path>) -> Result<ChdDisc, Error> {
        let mut file = BufReader::new(File::open(path)?);
        // Validate before Chd::open creates its codecs/map and before our hunk
        // allocation. chd 0.4 intentionally skips most V5 header validation.
        let header = Header::try_read_header(&mut file)?;
        let hunk_size = header.hunk_size();
        if hunk_size < FRAME as u32
            || hunk_size > MAX_HUNK
            || !hunk_size.is_multiple_of(FRAME as u32)
        {
            return Err(Error::Invalid(
                "CD hunk size must contain complete 2448-byte frames and be at most 16 MiB",
            ));
        }
        file.seek(SeekFrom::Start(0))?;
        let mut chd = Chd::open(file, None)?;
        let metas: Vec<chd::metadata::Metadata> = chd.metadata_refs().try_into()?;
        let mut tracks = Vec::new();
        let (mut lba, mut chd_frame) = (0u32, 0u32);
        for meta in metas
            .iter()
            .filter(|m| m.metatag == KnownMetadata::CdRomTrack2 as u32 || m.metatag == KnownMetadata::GdRomTrack as u32)
        {
            let text = String::from_utf8_lossy(&meta.value);
            let field = |name: &str| {
                text.split_whitespace()
                    .find_map(|kv| kv.strip_prefix(name).and_then(|v| v.strip_prefix(':')))
                    .map(|v| v.trim_end_matches('\0').to_string())
            };
            let number = |name: &str| {
                field(name)
                    .and_then(|v| v.parse::<u32>().ok())
                    .ok_or(Error::Invalid("missing or invalid CD track numeric field"))
            };
            let kind = field("TYPE").ok_or(Error::NotCd)?;
            let (frames, pregap) = (number("FRAMES")?, number("PREGAP")?);
            let stored = field("PGTYPE").is_some_and(|t| t.starts_with('V'));
            let audio = kind == "AUDIO";
            let cooked = kind == "MODE1";
            if !(audio || cooked || kind.ends_with("_RAW")) {
                return Err(Error::NotCd);
            }
            if frames == 0 || (stored && pregap > frames) {
                return Err(Error::Invalid("invalid CD track or stored pregap length"));
            }
            // A pregap not stored is silence before the track's frames.
            if !stored {
                lba = lba
                    .checked_add(pregap)
                    .ok_or(Error::Invalid("CD LBA range overflows"))?;
            }
            let start = lba
                .checked_add(if stored { pregap } else { 0 })
                .ok_or(Error::Invalid("CD pregap range overflows"))?;
            tracks.push(Track {
                start,
                audio,
                data: lba,
                frames,
                chd_frame,
                cooked,
            });
            lba = lba
                .checked_add(frames)
                .ok_or(Error::Invalid("CD LBA range overflows"))?;
            let padded_frames = frames
                .div_ceil(4)
                .checked_mul(4)
                .ok_or(Error::Invalid("padded CD track length overflows"))?;
            chd_frame = chd_frame
                .checked_add(padded_frames)
                .ok_or(Error::Invalid("CHD frame range overflows"))?;
        }
        if tracks.is_empty() {
            return Err(Error::NotCd);
        }
        Ok(ChdDisc {
            frames_per_hunk: hunk_size / FRAME as u32,
            hunk: vec![0; hunk_size as usize],
            hunk_number: None,
            compressed: Vec::new(),
            chd,
            tracks,
            end: lba,
        })
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// The LBA past the last sector.
    pub fn end(&self) -> u32 {
        self.end
    }

    /// A raw sector; false past the end (or where the CHD is damaged).
    /// Sectors of a pregap that is not stored read as zeros.
    pub fn read(&mut self, lba: u32, sector: &mut [u8; SECTOR]) -> bool {
        let Some(track) = self.tracks.iter().rev().find(|t| t.data <= lba).copied() else {
            sector.fill(0);
            return lba < self.end;
        };
        if lba >= track.data + track.frames {
            sector.fill(0);
            return lba < self.end;
        }
        let frame = track.chd_frame + (lba - track.data);
        let hunk = frame / self.frames_per_hunk;
        if self.hunk_number != Some(hunk) {
            self.hunk_number = None;
            let Ok(mut h) = self.chd.hunk(hunk) else {
                return false;
            };
            if h.read_hunk_in(&mut self.compressed, &mut self.hunk)
                .is_err()
            {
                return false;
            }
            self.hunk_number = Some(hunk);
        }
        let at = (frame % self.frames_per_hunk) as usize * FRAME;
        let raw = &self.hunk[at..at + SECTOR];
        if track.audio {
            for (out, pair) in sector.chunks_exact_mut(2).zip(raw.chunks_exact(2)) {
                out.copy_from_slice(&[pair[1], pair[0]]);
            }
        } else if track.cooked {
            write_header(sector, lba, 1);
            sector[16..16 + 2048].copy_from_slice(&raw[..2048]);
        } else {
            sector.copy_from_slice(raw);
        }
        true
    }
}

/// The sync pattern and header of a sector made from its data alone.
fn write_header(sector: &mut [u8; SECTOR], lba: u32, mode: u8) {
    sector.fill(0);
    sector[1..11].fill(0xff);
    let f = u64::from(lba) + 150;
    let bcd = |v: u64| (((v / 10) << 4) | (v % 10)) as u8;
    sector[12..16].copy_from_slice(&[bcd(f / 4500), bcd(f / 75 % 60), bcd(f % 75), mode]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    struct Input(std::path::PathBuf);

    impl Input {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "sol-disc-test-{}-{}.chd",
                std::process::id(),
                NEXT_FILE.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            file.write_all(bytes).unwrap();
            Self(path)
        }
    }

    impl Drop for Input {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn header(hunk: u32) -> Vec<u8> {
        let mut bytes = vec![0; 124];
        bytes[..8].copy_from_slice(b"MComprHD");
        bytes[8..12].copy_from_slice(&124u32.to_be_bytes());
        bytes[12..16].copy_from_slice(&5u32.to_be_bytes());
        bytes[32..40].copy_from_slice(&(FRAME as u64 * 4).to_be_bytes());
        bytes[40..48].copy_from_slice(&124u64.to_be_bytes());
        bytes[48..56].copy_from_slice(&140u64.to_be_bytes());
        bytes[56..60].copy_from_slice(&hunk.to_be_bytes());
        bytes[60..64].copy_from_slice(&(FRAME as u32).to_be_bytes());
        bytes
    }

    fn cd_metadata(tracks: &[&str]) -> Vec<u8> {
        let mut bytes = header(FRAME as u32);
        bytes.extend_from_slice(&[0; 16]); // Four uncompressed hunk map entries.
        for (index, fields) in tracks.iter().enumerate() {
            let text = format!("TRACK:{} TYPE:MODE1_RAW SUBTYPE:NONE {fields}\0", index + 1);
            let next = if index + 1 < tracks.len() {
                (bytes.len() + 16 + text.len()) as u64
            } else {
                0
            };
            bytes.extend_from_slice(b"CHT2");
            bytes.extend_from_slice(&(text.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&next.to_be_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        bytes
    }

    #[test]
    fn malformed_v5_hunk_sizes_fail_before_map_or_hunk_allocation() {
        for hunk in [
            1,
            FRAME as u32 + 1,
            (MAX_HUNK / FRAME as u32 + 1) * FRAME as u32,
        ] {
            // Only the header exists: an attempted map read would be an I/O
            // error. Previously a one-byte hunk was accepted and sector reads
            // panicked when dividing by frames_per_hunk == 0.
            let input = Input::new(&header(hunk));
            assert!(matches!(ChdDisc::open(&input.0), Err(Error::Invalid(_))));
        }
    }

    #[test]
    fn malformed_track_counters_fail_without_wrapping_or_panicking() {
        for tracks in [
            vec!["FRAMES:0 PREGAP:0 PGTYPE:MODE1_RAW"],
            vec!["FRAMES:4294967296 PREGAP:0 PGTYPE:MODE1_RAW"],
            vec!["FRAMES:4 PREGAP:no PGTYPE:MODE1_RAW"],
            vec!["FRAMES:4 PREGAP:5 PGTYPE:VMODE1_RAW"],
            vec!["FRAMES:4294967295 PREGAP:0 PGTYPE:MODE1_RAW"],
            vec!["FRAMES:4294967292 PREGAP:4 PGTYPE:MODE1_RAW"],
            vec!["FRAMES:2147483648 PREGAP:0 PGTYPE:MODE1_RAW"; 2],
        ] {
            let input = Input::new(&cd_metadata(&tracks));
            assert!(
                matches!(ChdDisc::open(&input.0), Err(Error::Invalid(_))),
                "{tracks:?}"
            );
        }
    }

    #[test]
    fn ordinary_track_and_stored_pregap_keep_their_existing_layout() {
        let input = Input::new(&cd_metadata(&[
            "FRAMES:4 PREGAP:0 PGTYPE:MODE1_RAW",
            "FRAMES:8 PREGAP:2 PGTYPE:VMODE1_RAW",
        ]));
        let disc = ChdDisc::open(&input.0).unwrap();
        assert_eq!(disc.end(), 12);
        assert_eq!(disc.frames_per_hunk, 1);
        assert_eq!(
            (
                disc.tracks()[1].start,
                disc.tracks()[1].data,
                disc.tracks()[1].chd_frame
            ),
            (6, 4, 4)
        );
    }
}
