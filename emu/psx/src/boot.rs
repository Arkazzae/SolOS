//! What the BIOS does with a disc at power-on, done by the core: it finds
//! the program to start in the disc's ISO 9660 file system (SYSTEM.CNF's
//! BOOT line, or PSX.EXE on a disc without one), with the stack and the
//! numbers of threads and events SYSTEM.CNF asks for.

use crate::Disc;

/// The program a disc starts, and what SYSTEM.CNF asks of the kernel.
pub(crate) struct Boot {
    pub exe: Vec<u8>,
    pub stack: u32,
    pub threads: u32,
    pub events: u32,
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// A sector's 2048 bytes of data: after the header in Mode 1, after the
/// subheader too in Mode 2 (Form 1, which files are in).
fn sector(disc: &mut dyn Disc, lba: u32) -> Option<Vec<u8>> {
    let mut raw = [0; 2352];
    if !disc.read(lba, &mut raw) {
        return None;
    }
    let at = if raw[15] == 2 { 24 } else { 16 };
    Some(raw[at..at + 2048].to_vec())
}

/// A file's or a directory's bytes, from its first sector on.
fn extent(disc: &mut dyn Disc, lba: u32, size: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(size.next_multiple_of(2048));
    for i in 0..size.div_ceil(2048) as u32 {
        out.extend(sector(disc, lba + i)?);
    }
    out.truncate(size);
    Some(out)
}

/// A name as ISO 9660 compares it here: without case and without the
/// version (";1") or a dot left before it.
fn plain(name: &[u8]) -> Vec<u8> {
    let end = name.iter().position(|&c| c == b';').unwrap_or(name.len());
    let name = name[..end].strip_suffix(b".").unwrap_or(&name[..end]);
    name.to_ascii_uppercase()
}

/// A file in the file system, by a path of names from the root: its bytes.
pub(crate) fn file(disc: &mut dyn Disc, path: &str) -> Option<Vec<u8>> {
    let pvd = sector(disc, 16)?;
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return None;
    }
    // The root's directory record is in the volume descriptor.
    let (mut lba, mut size) = (le32(&pvd, 158), le32(&pvd, 166) as usize);
    let names: Vec<&str> = path.split(['\\', '/']).filter(|n| !n.is_empty()).collect();
    for (i, name) in names.iter().enumerate() {
        let dir = extent(disc, lba, size)?;
        let want = plain(name.as_bytes());
        let mut at = 0;
        let mut found = None;
        while at < dir.len() {
            let len = dir[at] as usize;
            // Records stay within a sector: a zero length pads its end.
            if len == 0 {
                at = (at / 2048 + 1) * 2048;
                continue;
            }
            if len < 34 || at + len > dir.len() {
                break;
            }
            let record = &dir[at..at + len];
            let id = &record[33..(33 + record[32] as usize).min(len)];
            if plain(id) == want {
                found = Some((le32(record, 2), le32(record, 10) as usize, record[25] & 2 != 0));
                break;
            }
            at += len;
        }
        let (l, s, directory) = found?;
        if directory != (i + 1 < names.len()) {
            return None;
        }
        (lba, size) = (l, s);
    }
    extent(disc, lba, size)
}

/// A number SYSTEM.CNF gives, in hexadecimal.
fn hex(value: &str) -> Option<u32> {
    let digits: String = value.chars().take_while(char::is_ascii_hexdigit).collect();
    u32::from_str_radix(&digits, 16).ok()
}

/// The program on the disc and what it asks for; None if the disc has no
/// file system, or no program where SYSTEM.CNF (or the lack of it) says.
pub(crate) fn find(disc: &mut dyn Disc) -> Option<Boot> {
    // Without SYSTEM.CNF, as the BIOS's defaults are.
    let mut boot = Boot { exe: Vec::new(), stack: 0x801f_ff00, threads: 4, events: 16 };
    let mut path = "PSX.EXE".to_string();
    if let Some(cnf) = file(disc, "SYSTEM.CNF") {
        for line in String::from_utf8_lossy(&cnf).lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            match key.trim().to_ascii_uppercase().as_str() {
                // "cdrom:\DIR\NAME.EXE;1", maybe with an argument after it.
                "BOOT" => {
                    let target = value.split([' ', '\t']).next().unwrap_or("");
                    let target = target.split_once(':').map_or(target, |(_, p)| p);
                    path = target.to_string();
                }
                "TCB" => boot.threads = hex(value).unwrap_or(boot.threads),
                "EVENT" => boot.events = hex(value).unwrap_or(boot.events),
                "STACK" => boot.stack = hex(value).unwrap_or(boot.stack),
                _ => {}
            }
        }
    }
    boot.exe = file(disc, &path)?;
    Some(boot)
}
