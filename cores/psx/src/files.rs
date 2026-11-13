//! The BIOS's files, done by the core: those of the memory cards ("bu00:"
//! for the first slot, "bu10:" for the second), in the file system the BIOS
//! keeps on a card. Sector 0 is the header; sectors 1-15 a frame for each
//! of the fifteen blocks of 8 KB (in use as a file's first, middle or last
//! block, or free, with the next block of the file, and in the first the
//! file's size and name); the blocks are from sector 64 on. Files are read
//! and written a sector at a time; a read or write is done at once, and
//! one asked to go on its own (FASYNC) is announced done at the vertical
//! blank the BIOS would end it at. A call that waits for the card (all but
//! nextfile, lseek and close, and reads and writes on their own) delivers
//! the driver's event as it returns, as the BIOS's reading the card does
//! during the call: programs wait for it after a firstfile that found
//! nothing. The first two files are the TTY's; other devices are not there.

use crate::Psx;
use crate::bios::CARD_INFO;

/// Files that can be open at once (the BIOS's FCBs).
pub(crate) const FILES: usize = 16;

// open's mode: to create the file (with as many blocks as the high
// halfword says), and to read or write it on its own.
const CREATE: u32 = 0x200;
const ASYNC: u32 = 0x8000;

// What errno says.
const ENOENT: u32 = 2;
const EBADF: u32 = 9;
const EBUSY: u32 = 16;
const EEXIST: u32 = 17;
const ENODEV: u32 = 19;
const EINVAL: u32 = 22;
const EMFILE: u32 = 24;
const ENOSPC: u32 = 28;

// A block's frame: free, or a file's first, middle or last block.
const FREE: u32 = 0xa0;
const FIRST: u32 = 0x51;
const MIDDLE: u32 = 0x52;
const LAST: u32 = 0x53;
const NO_BLOCK: u16 = 0xffff;

/// A block's frame in the directory, as the BIOS reads it.
struct Frame {
    state: u32,
    size: u32,
    next: u16,
    name: Vec<u8>,
}

/// Whether a name matches a pattern, where ? is any character; the
/// pattern may go on with ?s after the name.
fn matches(name: &[u8], pattern: &[u8]) -> bool {
    name.iter().enumerate().all(|(i, &c)| pattern.get(i).is_some_and(|&p| p == b'?' || p == c))
        && pattern.get(name.len()).is_none_or(|&p| p == b'?')
}

impl Psx {
    /// The card a path is on ("bu00:NAME", "bu10:NAME"), and its name.
    fn card_path(&self, path: u32) -> Option<(usize, Vec<u8>)> {
        let path = self.c_string(path);
        let colon = path.iter().position(|&c| c == b':')?;
        let device = std::str::from_utf8(&path[..colon]).ok()?;
        let id = u32::from_str_radix(device.strip_prefix("bu")?, 16).ok()?;
        Some(((id >> 4 & 1) as usize, path[colon + 1..].to_vec()))
    }

    fn block_frame(&self, port: usize, block: usize) -> Frame {
        let sector = self.sio.cards[port].as_ref().and_then(|c| c.sector(block as u32 + 1)).unwrap_or(&[0; 128]);
        let word = |at: usize| u32::from_le_bytes(sector[at..at + 4].try_into().unwrap());
        let name = &sector[0x0a..0x0a + 21];
        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let next = u16::from_le_bytes([sector[8], sector[9]]);
        Frame { state: word(0), size: word(4), next, name: name[..end].to_vec() }
    }

    /// Writes a block's frame, with its checksum.
    fn set_block_frame(&mut self, port: usize, block: usize, frame: &Frame) {
        let mut sector = [0; 128];
        sector[..4].copy_from_slice(&frame.state.to_le_bytes());
        sector[4..8].copy_from_slice(&frame.size.to_le_bytes());
        sector[8..10].copy_from_slice(&frame.next.to_le_bytes());
        let len = frame.name.len().min(20);
        sector[0x0a..0x0a + len].copy_from_slice(&frame.name[..len]);
        self.write_card(port, block as u32 + 1, &sector);
    }

    /// Writes a sector of a card, its last byte the checksum of the rest
    /// for the directory's.
    fn write_card(&mut self, port: usize, n: u32, sector: &[u8; 128]) {
        let mut sector = *sector;
        if n < 64 {
            sector[127] = sector[..127].iter().fold(0, |a, b| a ^ b);
        }
        self.sio.write_card_sector(port, n, &sector);
    }

    /// The block of the first file at or after `from` whose name matches.
    fn find_file(&self, port: usize, pattern: &[u8], from: usize) -> Option<usize> {
        (from..15).find(|&b| {
            let f = self.block_frame(port, b);
            f.state == FIRST && !f.name.is_empty() && matches(&f.name, pattern)
        })
    }

    /// The card's sector of a file's sector, following its blocks.
    fn file_sector(&self, port: usize, first: u32, n: u32) -> Option<u32> {
        let mut block = first;
        for _ in 0..n / 64 {
            block = u32::from(self.block_frame(port, block as usize).next);
            if block >= 15 {
                return None;
            }
        }
        Some((block + 1) * 64 + n % 64)
    }

    /// The call waits for the card in a slot: its driver's event comes.
    fn touch(&mut self, port: usize) {
        self.hle.card_touched = Some(port);
    }

    /// A call fails: errno says why; -1 is returned.
    fn fail(&mut self, errno: u32) -> u32 {
        self.hle.errno = errno;
        u32::MAX
    }

    /// A call that returns 1 when done fails.
    fn failed(&mut self, errno: u32) -> u32 {
        self.hle.errno = errno;
        0
    }

    /// open(path, mode): a file descriptor, or -1.
    pub(crate) fn open(&mut self, path: u32, mode: u32) -> u32 {
        let Some((port, name)) = self.card_path(path) else { return self.fail(ENODEV) };
        if mode & ASYNC == 0 {
            self.touch(port);
        }
        if self.sio.cards[port].is_none() {
            return self.fail(EBUSY);
        }
        let Some(fd) = (2..FILES).find(|&fd| self.hle.files[fd][0] == 0) else { return self.fail(EMFILE) };
        let first = if mode & CREATE != 0 {
            if self.find_file(port, &name, 0).is_some() {
                return self.fail(EEXIST);
            }
            let blocks = (mode >> 16).max(1) as usize;
            let free: Vec<usize> = (0..15).filter(|&b| self.block_frame(port, b).state & 0xf0 == FREE).collect();
            if blocks > free.len() {
                return self.fail(ENOSPC);
            }
            let chain = &free[..blocks];
            for (i, &block) in chain.iter().enumerate() {
                let state = match i {
                    0 => FIRST,
                    _ if i + 1 == blocks => LAST,
                    _ => MIDDLE,
                };
                let next = chain.get(i + 1).map_or(NO_BLOCK, |&b| b as u16);
                let (size, name) = if i == 0 { ((mode >> 16) << 13, name.clone()) } else { (0, Vec::new()) };
                self.set_block_frame(port, block, &Frame { state, size, next, name });
            }
            chain[0]
        } else {
            match self.find_file(port, &name, 0) {
                Some(block) => block,
                None => return self.fail(ENOENT),
            }
        };
        let length = self.block_frame(port, first).size;
        self.hle.files[fd] = [mode | 1 << 31, port as u32, first as u32, 0, length];
        self.hle.errno = 0;
        fd as u32
    }

    /// read and write of a card's file: whole sectors from its offset; the
    /// bytes done, or 0 when it goes on its own, or -1.
    pub(crate) fn file_io(&mut self, fd: u32, buffer: u32, size: u32, write: bool) -> u32 {
        let Some(&[mode, port, first, offset, length]) = self.hle.files.get(fd as usize) else {
            return self.fail(EBADF);
        };
        if mode == 0 || fd < 2 {
            return self.fail(EBADF);
        }
        let port = port as usize;
        if mode & ASYNC == 0 {
            self.touch(port);
        }
        if offset & 0x7f != 0 || offset >= length {
            return self.fail(EINVAL);
        }
        let mut done = 0;
        while done < size / 128 {
            let Some(sector) = self.file_sector(port, first, offset / 128 + done) else { break };
            let at = buffer + 128 * done;
            if write {
                let mut data = [0; 128];
                data.iter_mut().enumerate().for_each(|(i, b)| *b = self.peek8(at + i as u32));
                self.write_card(port, sector, &data);
            } else {
                let data = self.sio.cards[port].as_ref().and_then(|c| c.sector(sector)).map(<[u8]>::to_vec);
                for (i, b) in data.unwrap_or_default().into_iter().enumerate() {
                    self.store8(at + i as u32, b);
                }
            }
            done += 1;
        }
        self.hle.files[fd as usize][3] += 128 * done;
        self.hle.errno = 0;
        if mode & ASYNC != 0 {
            // Announced done at the port's next turn.
            let device = (port as u32) << 4;
            if !self.card_start(CARD_INFO, device, 0, 0) {
                return self.fail(EBUSY);
            }
            self.hle.card_new_ok = true;
            self.hle.bu_waits[port] = true;
            return 0;
        }
        128 * done
    }

    pub(crate) fn seek(&mut self, fd: u32, offset: u32, whence: u32) -> u32 {
        let Some(file) = self.hle.files.get_mut(fd as usize).filter(|f| f[0] != 0) else { return self.fail(EBADF) };
        file[3] = if whence == 1 { file[3].wrapping_add(offset) } else { offset };
        file[3]
    }

    pub(crate) fn close(&mut self, fd: u32) -> u32 {
        match self.hle.files.get_mut(fd as usize).filter(|f| f[0] != 0 && fd >= 2) {
            Some(file) => {
                file[0] = 0;
                fd
            }
            None => self.fail(EBADF),
        }
    }

    /// firstfile(pattern, entry): the first file whose name matches, where
    /// ? is any character and * the rest, into the entry; 0 when none.
    pub(crate) fn first_file(&mut self, path: u32, entry: u32) -> u32 {
        let Some((port, name)) = self.card_path(path) else {
            self.hle.errno = ENODEV;
            return 0;
        };
        self.touch(port);
        let mut pattern = [b'?'; 20];
        let given = name.iter().position(|&c| c == b'*').unwrap_or(name.len());
        if !name.is_empty() {
            pattern[..given.min(20)].copy_from_slice(&name[..given.min(20)]);
            if given == name.len() && given < 20 {
                pattern[given] = 0;
            }
        } else {
            pattern[19] = 0;
        }
        self.hle.find = pattern;
        self.hle.find_at = [port as u32, 0];
        self.next_file(entry)
    }

    pub(crate) fn next_file(&mut self, entry: u32) -> u32 {
        let [port, from] = self.hle.find_at;
        let port = port as usize;
        let pattern: Vec<u8> = self.hle.find.iter().copied().take_while(|&c| c != 0).collect();
        let Some(block) = self.find_file(port, &pattern, from as usize) else {
            self.hle.errno = ENOENT;
            return 0;
        };
        self.hle.find_at[1] = block as u32 + 1;
        let frame = self.block_frame(port, block);
        for i in 0..20 {
            self.store8(entry + i, frame.name.get(i as usize).copied().unwrap_or(0));
        }
        self.store32(entry + 0x14, frame.state & 0xf0);
        self.store32(entry + 0x18, frame.size);
        self.store32(entry + 0x20, (block as u32 + 1) * 64);
        self.hle.errno = 0;
        entry
    }

    /// erase(path): the file's blocks become deleted ones; 1 if it was there.
    pub(crate) fn erase(&mut self, path: u32) -> u32 {
        let Some((port, name)) = self.card_path(path) else { return self.failed(ENODEV) };
        self.touch(port);
        let Some(mut block) = self.find_file(port, &name, 0) else { return self.failed(ENOENT) };
        for _ in 0..15 {
            let mut frame = self.block_frame(port, block);
            if !matches!(frame.state, FIRST | MIDDLE | LAST) {
                break;
            }
            frame.state = 0xa0 | frame.state & 0x0f;
            self.set_block_frame(port, block, &frame);
            if frame.next as usize >= 15 {
                break;
            }
            block = frame.next as usize;
        }
        self.hle.errno = 0;
        1
    }

    /// rename(old, new) on one card; 1 if done.
    pub(crate) fn rename(&mut self, old: u32, new: u32) -> u32 {
        let (Some((port, old)), Some((_, new))) = (self.card_path(old), self.card_path(new)) else {
            return self.failed(ENODEV);
        };
        self.touch(port);
        if self.find_file(port, &new, 0).is_some() {
            return self.failed(EEXIST);
        }
        let Some(block) = self.find_file(port, &old, 0) else { return self.failed(ENOENT) };
        let mut frame = self.block_frame(port, block);
        frame.name = new;
        self.set_block_frame(port, block, &frame);
        self.hle.errno = 0;
        1
    }

    /// format(device): the card as formatted, its files gone; 1 if done.
    pub(crate) fn format(&mut self, path: u32) -> u32 {
        let Some((port, _)) = self.card_path(path) else { return self.failed(ENODEV) };
        self.touch(port);
        if self.sio.cards[port].is_none() {
            return self.failed(EBUSY);
        }
        let blank = crate::sio::Card::formatted();
        for n in 0..64 {
            let sector: [u8; 128] = blank.sector(n).unwrap().try_into().unwrap();
            self.write_card(port, n, &sector);
        }
        self.hle.errno = 0;
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A console with the kernel set up and a formatted card in slot 1,
    /// and a string put in RAM.
    fn console() -> Psx {
        let mut psx = Psx::blank();
        psx.set_up_kernel(4, 16);
        psx.insert_memory_card(0, None).unwrap();
        psx
    }

    fn put(psx: &mut Psx, at: u32, text: &str) -> u32 {
        for (i, b) in text.bytes().chain([0]).enumerate() {
            psx.store8(at + i as u32, b);
        }
        at
    }

    #[test]
    fn a_file_is_created_written_found_and_erased() {
        let mut psx = console();
        let name = put(&mut psx, 0x8001_0000, "bu00:BASLUS-00001SAVE");
        assert_eq!(psx.open(name, 1), u32::MAX);
        assert_eq!(psx.hle.errno, ENOENT);
        // Two blocks, written in the second.
        let fd = psx.open(name, 2 << 16 | CREATE);
        assert_eq!(fd, 2);
        psx.close(fd);
        let fd = psx.open(name, 3);
        for i in 0..256 {
            psx.store8(0x8002_0000 + i, i as u8);
        }
        assert_eq!(psx.seek(fd, 0x2000, 0), 0x2000);
        assert_eq!(psx.file_io(fd, 0x8002_0000, 256, true), 256);
        psx.close(fd);
        let fd = psx.open(name, 1);
        psx.seek(fd, 0x2080, 0);
        assert_eq!(psx.file_io(fd, 0x8003_0000, 128, false), 128);
        assert_eq!(psx.peek32(0x8003_0000), 0x8382_8180);
        // The frames: the first block in use, the second its last.
        let first = psx.block_frame(0, 0);
        assert_eq!((first.state, first.size, first.next), (FIRST, 0x4000, 1));
        assert_eq!(psx.block_frame(0, 1).state, LAST);
        let pattern = put(&mut psx, 0x8001_0100, "bu00:BASLUS*");
        assert_eq!(psx.first_file(pattern, 0x8004_0000), 0x8004_0000);
        assert_eq!(psx.c_string(0x8004_0000), b"BASLUS-00001SAVE");
        assert_eq!(psx.peek32(0x8004_0018), 0x4000);
        assert_eq!(psx.next_file(0x8004_0000), 0);
        let new = put(&mut psx, 0x8001_0200, "bu00:BASLUS-00001DATA");
        assert_eq!(psx.rename(name, new), 1);
        assert_eq!(psx.erase(new), 1);
        assert_eq!(psx.block_frame(0, 0).state, 0xa1);
        assert_eq!(psx.block_frame(0, 1).state, 0xa3);
        assert_eq!(psx.first_file(pattern, 0x8004_0000), 0);
        assert!(psx.memory_card_written());
    }
}
