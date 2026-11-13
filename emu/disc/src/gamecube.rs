//! GameCube disc images read as the disc's bytes by offset: a plain ISO/GCM,
//! or Dolphin's RVZ, how the console's games are kept.
//!
//! An RVZ (docs/WiaAndRvz.md in Dolphin's repository) cuts the disc into
//! chunks of `chunk_size` bytes, each compressed on its own (zstd here, the
//! only method Dolphin writes by default) so any offset can be read without
//! the chunks before it. A GameCube disc has no partitions: its bytes are
//! "raw data" entries, each a run of chunks ("groups"). A group may be
//! "packed": its pseudo-random padding is stored as a 68-byte seed of the
//! generator that made it, rebuilt here.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// The magic word of a GameCube disc header, at 0x1c.
const GAMECUBE_MAGIC: u32 = 0xc233_9f3d;
/// The magic word of a Wii disc header, at 0x18.
const WII_MAGIC: u32 = 0x5d1c_9ea3;
/// Padding is generated per 32 KiB sector.
const SECTOR: u64 = 0x8000;
/// Larger than any GameCube disc (1.36 GiB) with room for odd dumps.
const MAX_DISC: u64 = 8 << 30;
/// Dolphin writes chunks from 32 KiB to 2 MiB; refuse absurd ones.
const MAX_CHUNK: u64 = 32 << 20;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn be32(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
}

fn be64(data: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(data[at..at + 8].try_into().unwrap())
}

/// A GameCube disc image opened read-only.
pub struct Disc {
    file: File,
    size: u64,
    rvz: Option<Rvz>,
}

impl std::fmt::Debug for Disc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Disc")
            .field("size", &self.size)
            .field("rvz", &self.rvz.is_some())
            .finish_non_exhaustive()
    }
}

impl Disc {
    /// Opens an ISO/GCM or an RVZ, told apart by their contents.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Disc> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        let mut head = [0u8; 0x48];
        let got = read_up_to(&mut file, &mut head)?;
        let (size, rvz) = if got >= 4 && &head[..4] == b"RVZ\x01" {
            let rvz = Rvz::open(&mut file, &head, length)?;
            (rvz.size, Some(rvz))
        } else if got >= 4 && &head[..4] == b"WIA\x01" {
            return Err(invalid("WIA images are not supported: convert the disc to RVZ or ISO"));
        } else {
            (length, None)
        };
        let mut disc = Disc { file, size, rvz };
        if disc.size < 0x440 {
            return Err(invalid("image is too small to be a GameCube disc"));
        }
        let mut header = [0u8; 0x20];
        disc.read_at(0, &mut header)?;
        if be32(&header, 0x18) == WII_MAGIC {
            return Err(invalid("image is a Wii disc, not a GameCube one"));
        }
        if be32(&header, 0x1c) != GAMECUBE_MAGIC {
            return Err(invalid("image is not a GameCube disc"));
        }
        Ok(disc)
    }

    /// The disc's size, as an ISO of it would have.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Whether the image is an RVZ rather than a plain ISO/GCM.
    pub fn is_rvz(&self) -> bool {
        self.rvz.is_some()
    }

    /// Reads exactly `out.len()` disc bytes from `offset`.
    pub fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        if offset.checked_add(out.len() as u64).is_none_or(|end| end > self.size) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("disc range {offset:#x}+{:#x} is past its end {:#x}", out.len(), self.size),
            ));
        }
        match &mut self.rvz {
            Some(rvz) => rvz.read_at(&mut self.file, offset, out),
            None => {
                self.file.seek(SeekFrom::Start(offset))?;
                self.file.read_exact(out)
            }
        }
    }
}

fn read_up_to(file: &mut File, out: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < out.len() {
        match file.read(&mut out[got..])? {
            0 => break,
            n => got += n,
        }
    }
    Ok(got)
}

/// A run of the disc's bytes stored as consecutive groups.
#[derive(Clone, Copy, Debug)]
struct Raw {
    /// Rounded down to a sector, as the format asks: the first entry says
    /// 0x80 but holds the whole first sector.
    start: u64,
    end: u64,
    first_group: u32,
}

#[derive(Clone, Copy, Debug)]
struct Group {
    /// Where its stored bytes are in the file, and how many.
    offset: u64,
    stored: u32,
    compressed: bool,
    /// Its size before unpacking, or 0 when it is not packed.
    packed: u32,
}

struct Rvz {
    size: u64,
    /// The first 0x80 bytes of the disc, kept in the header.
    head: [u8; 0x80],
    zstd: bool,
    chunk: u64,
    raws: Vec<Raw>,
    groups: Vec<Group>,
    decoder: zrip::DecompressContext,
    /// The group last read and its disc bytes.
    cached: Option<usize>,
    data: Vec<u8>,
    stored: Vec<u8>,
    unpacked: Vec<u8>,
}

impl Rvz {
    fn open(file: &mut File, file_head: &[u8; 0x48], length: u64) -> io::Result<Rvz> {
        let compatible = be32(file_head, 0x08);
        if compatible > 0x0100_0000 {
            return Err(invalid(format!("RVZ version {compatible:#x} is newer than this reader")));
        }
        let disc_size = be32(file_head, 0x0c) as usize;
        if !(0xdc..=0x1000).contains(&disc_size) {
            return Err(invalid("RVZ disc header has an unexpected size"));
        }
        let size = be64(file_head, 0x24);
        if size < 0x440 || size > MAX_DISC {
            return Err(invalid("RVZ disc size is out of range"));
        }
        let mut disc = vec![0u8; disc_size];
        file.seek(SeekFrom::Start(0x48))?;
        file.read_exact(&mut disc)?;
        if be32(&disc, 0x00) != 1 {
            return Err(invalid("RVZ image is not of a GameCube disc"));
        }
        let zstd = match be32(&disc, 0x04) {
            0 => false,
            5 => true,
            2..=4 => return Err(invalid("RVZ compressed with bzip2 or LZMA is not supported: use zstd")),
            _ => return Err(invalid("RVZ compression method is unknown")),
        };
        let chunk = u64::from(be32(&disc, 0x0c));
        let chunk_ok = if chunk < 2 << 20 {
            chunk >= SECTOR && chunk.is_power_of_two()
        } else {
            chunk.is_multiple_of(2 << 20) && chunk <= MAX_CHUNK
        };
        if !chunk_ok {
            return Err(invalid(format!("RVZ chunk size {chunk:#x} is invalid")));
        }
        let mut head = [0u8; 0x80];
        head.copy_from_slice(&disc[0x10..0x90]);
        if be32(&disc, 0x90) != 0 {
            return Err(invalid("RVZ of a GameCube disc lists partitions"));
        }
        let raw_count = be32(&disc, 0xb4) as usize;
        let group_count = be32(&disc, 0xc4) as usize;
        let most_groups = size.div_ceil(chunk) as usize + raw_count;
        if raw_count == 0 || raw_count > 4096 || group_count > most_groups {
            return Err(invalid("RVZ raw data or group count is out of range"));
        }
        let mut decoder = zrip::DecompressContext::new();
        let mut table = |offset: u64, stored: u32, count: usize, entry: usize| -> io::Result<Vec<u8>> {
            let want = count * entry;
            if offset.checked_add(u64::from(stored)).is_none_or(|end| end > length) || stored > 64 << 20 {
                return Err(invalid("RVZ table is outside the file"));
            }
            let mut bytes = vec![0u8; stored as usize];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut bytes)?;
            let mut entries = if zstd {
                let mut out = Vec::with_capacity(want);
                decoder
                    .decompress_into_with_limit(&bytes, &mut out, want)
                    .map_err(|e| invalid(format!("RVZ table does not decompress: {e:?}")))?;
                out
            } else {
                bytes
            };
            if entries.len() < want {
                return Err(invalid("RVZ table is shorter than its entries"));
            }
            entries.truncate(want);
            Ok(entries)
        };
        let raw_table = table(be64(&disc, 0xb8), be32(&disc, 0xc0), raw_count, 0x18)?;
        let group_table = table(be64(&disc, 0xc8), be32(&disc, 0xd0), group_count, 0x0c)?;
        let groups: Vec<Group> = group_table
            .chunks_exact(0x0c)
            .map(|g| {
                let word = be32(g, 4);
                Group {
                    offset: u64::from(be32(g, 0)) * 4,
                    stored: word & 0x7fff_ffff,
                    compressed: word & 0x8000_0000 != 0,
                    packed: be32(g, 8),
                }
            })
            .collect();
        for group in &groups {
            if group.offset.checked_add(u64::from(group.stored)).is_none_or(|end| end > length) {
                return Err(invalid("RVZ group is outside the file"));
            }
            if u64::from(group.stored) > chunk * 2 + 0x1000 || u64::from(group.packed) > chunk * 2 + 0x1000 {
                return Err(invalid("RVZ group is larger than its chunk allows"));
            }
        }
        let mut raws = Vec::with_capacity(raw_count);
        for r in raw_table.chunks_exact(0x18) {
            let (offset, length) = (be64(r, 0), be64(r, 8));
            let end = offset
                .checked_add(length)
                .filter(|&end| end <= size)
                .ok_or_else(|| invalid("RVZ raw data is past the disc's end"))?;
            let start = offset - offset % SECTOR;
            let (first_group, count) = (be32(r, 0x10), be32(r, 0x14));
            if u64::from(count) < (end - start).div_ceil(chunk)
                || first_group as usize + count as usize > groups.len()
            {
                return Err(invalid("RVZ raw data does not match its groups"));
            }
            raws.push(Raw { start, end, first_group });
        }
        raws.sort_by_key(|r| r.start);
        if raws.windows(2).any(|w| w[0].end > w[1].start) {
            return Err(invalid("RVZ raw data entries overlap"));
        }
        Ok(Rvz {
            size,
            head,
            zstd,
            chunk,
            raws,
            groups,
            decoder,
            cached: None,
            data: Vec::new(),
            stored: Vec::new(),
            unpacked: Vec::new(),
        })
    }

    fn read_at(&mut self, file: &mut File, mut offset: u64, mut out: &mut [u8]) -> io::Result<()> {
        while !out.is_empty() {
            if offset < 0x80 {
                let n = out.len().min(0x80 - offset as usize);
                out[..n].copy_from_slice(&self.head[offset as usize..offset as usize + n]);
                (offset, out) = (offset + n as u64, &mut out[n..]);
                continue;
            }
            let raw = *self
                .raws
                .iter()
                .find(|r| r.start <= offset && offset < r.end)
                .ok_or_else(|| invalid(format!("RVZ has no data for disc offset {offset:#x}")))?;
            let index = (offset - raw.start) / self.chunk;
            let start = raw.start + index * self.chunk;
            let length = self.chunk.min(raw.end - start) as usize;
            self.load(file, (raw.first_group as u64 + index) as usize, start, length)?;
            let at = (offset - start) as usize;
            let n = out.len().min(length - at);
            out[..n].copy_from_slice(&self.data[at..at + n]);
            (offset, out) = (offset + n as u64, &mut out[n..]);
        }
        Ok(())
    }

    /// Makes `data` hold group `index`: `length` disc bytes from `start`.
    fn load(&mut self, file: &mut File, index: usize, start: u64, length: usize) -> io::Result<()> {
        if self.cached == Some(index) && self.data.len() == length {
            return Ok(());
        }
        self.cached = None;
        let group = self.groups[index];
        self.data.clear();
        if group.stored == 0 {
            self.data.resize(length, 0);
            self.cached = Some(index);
            return Ok(());
        }
        self.stored.resize(group.stored as usize, 0);
        file.seek(SeekFrom::Start(group.offset))?;
        file.read_exact(&mut self.stored)?;
        let expected = if group.packed != 0 { group.packed as usize } else { length };
        let bytes: &[u8] = if group.compressed && self.zstd {
            self.unpacked.clear();
            self.decoder
                .decompress_into_with_limit(&self.stored, &mut self.unpacked, expected)
                .map_err(|e| invalid(format!("RVZ group {index} does not decompress: {e:?}")))?;
            &self.unpacked
        } else {
            &self.stored
        };
        if bytes.len() < expected {
            return Err(invalid(format!("RVZ group {index} is shorter than its data")));
        }
        let bytes = &bytes[..expected];
        if group.packed != 0 {
            unpack(bytes, start, length, &mut self.data)
                .map_err(|reason| invalid(format!("RVZ group {index}: {reason}")))?;
        } else {
            self.data.extend_from_slice(bytes);
        }
        self.cached = Some(index);
        Ok(())
    }
}

/// Decodes RVZ packing: runs of stored bytes, and runs of padding made by the
/// generator from a stored seed. `start` is the group's disc offset.
fn unpack(mut input: &[u8], start: u64, length: usize, out: &mut Vec<u8>) -> Result<(), &'static str> {
    let mut junk = Junk::new();
    while !input.is_empty() {
        let size = be32(input.get(..4).ok_or("truncated packed run")?, 0);
        input = &input[4..];
        let n = (size & 0x7fff_ffff) as usize;
        if out.len() + n > length {
            return Err("packed runs are longer than the group");
        }
        if size & 0x8000_0000 != 0 {
            let seed = input.get(..68).ok_or("truncated padding seed")?;
            input = &input[68..];
            junk.seed(seed);
            junk.skip(((start + out.len() as u64) % SECTOR) as usize);
            let at = out.len();
            out.resize(at + n, 0);
            junk.fill(&mut out[at..]);
        } else {
            out.extend_from_slice(input.get(..n).ok_or("truncated stored run")?);
            input = &input[n..];
        }
    }
    if out.len() != length {
        return Err("packed runs are shorter than the group");
    }
    Ok(())
}

const LAG: usize = 521;
const SHORT: usize = 32;

/// The lagged Fibonacci generator (xor, j = 32, k = 521) that made the
/// discs' padding. Its words are kept already turned into the order its bytes
/// come out in (bits 24, 18, 8 and 0 of each word start the four bytes).
struct Junk {
    words: [u32; LAG],
    /// Bytes of the current buffer already given out.
    used: usize,
}

impl Junk {
    fn new() -> Junk {
        Junk { words: [0; LAG], used: 0 }
    }

    fn seed(&mut self, seed: &[u8]) {
        for (i, word) in seed.chunks_exact(4).take(17).enumerate() {
            self.words[i] = be32(word, 0);
        }
        for i in 17..LAG {
            self.words[i] = (self.words[i - 17] << 23) ^ (self.words[i - 16] >> 9) ^ self.words[i - 1];
        }
        // Each output bit is one bit of the word, so this commutes with the
        // xors of `advance` and can be done once here.
        for word in &mut self.words {
            *word = (*word & 0xff00_ffff) | ((*word >> 2) & 0x00ff_0000);
        }
        for _ in 0..4 {
            self.advance();
        }
        self.used = 0;
    }

    fn advance(&mut self) {
        for i in 0..SHORT {
            self.words[i] ^= self.words[i + LAG - SHORT];
        }
        for i in SHORT..LAG {
            self.words[i] ^= self.words[i - SHORT];
        }
    }

    fn skip(&mut self, bytes: usize) {
        self.used += bytes;
        while self.used >= LAG * 4 {
            self.advance();
            self.used -= LAG * 4;
        }
    }

    fn fill(&mut self, mut out: &mut [u8]) {
        while !out.is_empty() {
            let word = self.used / 4;
            let bytes = self.words[word].to_be_bytes();
            let from = self.used % 4;
            let n = out.len().min(4 - from);
            out[..n].copy_from_slice(&bytes[from..from + n]);
            out = &mut out[n..];
            self.skip(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generator's first bytes from an all-zero-but-one seed, computed
    /// by the plain algorithm of the format description (no reordering).
    fn reference(seed: &[u32; 17], skip: usize, count: usize) -> Vec<u8> {
        let mut b = [0u32; LAG];
        b[..17].copy_from_slice(seed);
        for i in 17..LAG {
            b[i] = (b[i - 17] << 23) ^ (b[i - 16] >> 9) ^ b[i - 1];
        }
        let forward = |b: &mut [u32; LAG]| {
            for i in 0..SHORT {
                b[i] ^= b[i + LAG - SHORT];
            }
            for i in SHORT..LAG {
                b[i] ^= b[i - SHORT];
            }
        };
        for _ in 0..4 {
            forward(&mut b);
        }
        let mut out = Vec::new();
        let mut i = 0;
        while out.len() < skip + count {
            if i == LAG {
                forward(&mut b);
                i = 0;
            }
            let w = b[i];
            out.extend_from_slice(&[(w >> 24) as u8, (w >> 18) as u8, (w >> 8) as u8, w as u8]);
            i += 1;
        }
        out[skip..skip + count].to_vec()
    }

    #[test]
    fn junk_matches_the_format_description() {
        let mut seed = [0u32; 17];
        for (i, word) in seed.iter_mut().enumerate() {
            *word = 0x9e37_79b9u32.wrapping_mul(i as u32 + 1) ^ 0x0123_4567;
        }
        let bytes: Vec<u8> = seed.iter().flat_map(|w| w.to_be_bytes()).collect();
        for (skip, count) in [(0, 64), (3, 5000), (2083, 10), (0x7fff, 0x8001)] {
            let mut junk = Junk::new();
            junk.seed(&bytes);
            junk.skip(skip);
            let mut out = vec![0; count];
            junk.fill(&mut out);
            assert_eq!(out, reference(&seed, skip, count), "skip {skip} count {count}");
        }
    }

    #[test]
    fn unpack_joins_stored_and_generated_runs() {
        let seed = [0x5au8; 68];
        let mut packed = Vec::new();
        packed.extend_from_slice(&3u32.to_be_bytes());
        packed.extend_from_slice(b"abc");
        packed.extend_from_slice(&(0x8000_0000u32 | 100).to_be_bytes());
        packed.extend_from_slice(&seed);
        let mut out = Vec::new();
        unpack(&packed, 0x18000, 103, &mut out).unwrap();
        assert_eq!(&out[..3], b"abc");
        let mut junk = Junk::new();
        junk.seed(&seed);
        junk.skip(3);
        let mut expected = vec![0; 100];
        junk.fill(&mut expected);
        assert_eq!(&out[3..], &expected[..]);
        assert!(unpack(&packed, 0, 102, &mut Vec::new()).is_err());
        assert!(unpack(&packed[..20], 0, 103, &mut Vec::new()).is_err());
    }
}
