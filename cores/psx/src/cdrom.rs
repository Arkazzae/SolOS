//! The CD-ROM controller: four ports banked by an index, a parameter FIFO
//! the CPU fills before writing a command, and responses that come back
//! some time later in a response FIFO with an interrupt (INT3 to
//! acknowledge a command, INT2 when one finishes, INT1 for each sector
//! read, INT5 for errors). One interrupt is pending at a time; the next
//! waits until the CPU acknowledges it. Sectors come from the disc at 75
//! a second (150 at double speed) into a buffer the CPU then reads through
//! the data FIFO or DMA channel 3.
//!
//! The disc itself is the host's: anything that gives raw 2352-byte
//! sectors by LBA (sector 0 is 00:02:00) and the tracks.

use crate::Psx;
use savestate::{BadState, State, state, take};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// A disc in the drive.
pub trait Disc: Send {
    /// A sector, raw (sync, header, subheader and data: 2352 bytes); false
    /// past the end.
    fn read(&mut self, lba: u32, sector: &mut [u8; 2352]) -> bool;
    /// The tracks, in order.
    fn tracks(&self) -> Vec<Track>;
    /// The LBA past the last sector: where the lead-out starts.
    fn end(&self) -> u32;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Track {
    pub start: u32,
    pub audio: bool,
}

/// The interrupt a response raises.
const INT1: u8 = 1;
const INT2: u8 = 2;
const INT3: u8 = 3;
const INT5: u8 = 5;

// The drive's status byte.
const ERROR: u8 = 1 << 0;
const MOTOR: u8 = 1 << 1;
const SHELL_OPEN: u8 = 1 << 4;
const READING: u8 = 1 << 5;
const SEEKING: u8 = 1 << 6;
const PLAYING: u8 = 1 << 7;

/// Cycles until a command is acknowledged, and a sector at single speed.
const ACK: u64 = 25_000;
const SECTOR: u64 = 33_868_800 / 75;
const SEEK: u64 = 100_000;

#[derive(Clone, Default)]
struct Response {
    int: u8,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq)]
enum Drive {
    Idle,
    Reading,
    Playing,
}

#[derive(Clone)]
pub struct Cdrom {
    disc: Option<Arc<Mutex<dyn Disc>>>,
    tracks: Vec<Track>,
    end: u32,
    index: u8,
    params: VecDeque<u8>,
    response: VecDeque<u8>,
    int_enable: u8,
    int_flag: u8,
    /// Responses waiting for the pending interrupt to be acknowledged.
    queue: VecDeque<Response>,
    /// The command's first response, and its second, with when they come.
    ack: Option<(u64, Response)>,
    second: Option<(u64, Response)>,
    /// The command being worked on (until its acknowledgement).
    busy: bool,
    stat: u8,
    mode: u8,
    setloc: Option<u32>,
    position: u32,
    drive: Drive,
    /// When the next sector comes off the disc.
    next_sector: Option<u64>,
    sector: Box<[u8; 2352]>,
    /// The sector the CPU reads (once it asks for it), and where.
    data: Vec<u8>,
    data_pos: usize,
    /// The last data sector's header and subheader, for GetlocL.
    header: [u8; 8],
    filter: (u8, u8),
    muted: bool,
    /// CD audio for the SPU, at 44100 Hz.
    audio: VecDeque<[i16; 2]>,
    xa: [XaChannel; 2],
    /// The CD audio volumes (left to left, left to right, right to right,
    /// right to left), as applied and as written until ADPCTL applies
    /// them; ADPCTL's mute.
    volume: [u8; 4],
    pending_volume: [u8; 4],
    xa_muted: bool,
}

impl Cdrom {
    /// The disc in the drive, for the BIOS the core answers to read files
    /// from.
    pub(crate) fn disc(&self) -> Option<Arc<Mutex<dyn Disc>>> {
        self.disc.clone()
    }

    pub fn new(disc: Option<Arc<Mutex<dyn Disc>>>) -> Cdrom {
        let (tracks, end) = match &disc {
            Some(d) => {
                let d = d.lock().unwrap();
                (d.tracks(), d.end())
            }
            None => (Vec::new(), 0),
        };
        let stat = if disc.is_some() { MOTOR } else { SHELL_OPEN };
        Cdrom {
            disc,
            tracks,
            end,
            index: 0,
            params: VecDeque::new(),
            response: VecDeque::new(),
            int_enable: 0,
            int_flag: 0,
            queue: VecDeque::new(),
            ack: None,
            second: None,
            busy: false,
            stat,
            mode: 0,
            setloc: None,
            position: 0,
            drive: Drive::Idle,
            next_sector: None,
            sector: Box::new([0; 2352]),
            data: Vec::new(),
            data_pos: 0,
            header: [0; 8],
            filter: (0, 0),
            muted: false,
            audio: VecDeque::new(),
            xa: [XaChannel::default(); 2],
            volume: [0x80, 0, 0x80, 0],
            pending_volume: [0x80, 0, 0x80, 0],
            xa_muted: false,
        }
    }

    /// The SPU takes the next sample of CD audio (silence when there is
    /// none).
    pub fn audio_sample(&mut self) -> [i16; 2] {
        self.audio.pop_front().unwrap_or([0; 2])
    }

    /// The next cycle something happens.
    pub fn due(&self) -> u64 {
        let ack = self.ack.as_ref().map_or(u64::MAX, |a| a.0);
        let second = self.second.as_ref().map_or(u64::MAX, |s| s.0);
        ack.min(second).min(self.next_sector.unwrap_or(u64::MAX))
    }

    fn status(&self) -> u8 {
        let mut s = self.index;
        s |= u8::from(self.params.is_empty()) << 3;
        s |= u8::from(self.params.len() < 16) << 4;
        s |= u8::from(!self.response.is_empty()) << 5;
        s |= u8::from(self.data_pos < self.data.len()) << 6;
        s |= u8::from(self.busy) << 7;
        s
    }

    pub fn read(&mut self, port: u32) -> u8 {
        match (port, self.index) {
            (0, _) => self.status(),
            (1, _) => self.response.pop_front().unwrap_or(0),
            (2, _) => self.read_data(),
            (3, 0 | 2) => self.int_enable | 0xe0,
            (3, _) => self.int_flag | 0xe0,
            _ => 0,
        }
    }

    pub fn read_data(&mut self) -> u8 {
        let byte = self.data.get(self.data_pos).copied().unwrap_or(0);
        if self.data_pos < self.data.len() {
            self.data_pos += 1;
        }
        byte
    }

    /// A word of the data FIFO, for DMA.
    pub fn read_data_word(&mut self) -> u32 {
        u32::from_le_bytes([self.read_data(), self.read_data(), self.read_data(), self.read_data()])
    }

    fn pending(&self) -> bool {
        self.int_flag & 7 != 0
    }

    /// A response: given to the CPU now if no interrupt is pending, else
    /// kept until then. Whether the interrupt line rose.
    fn respond(&mut self, response: Response) -> bool {
        if self.pending() {
            self.queue.push_back(response);
            return false;
        }
        self.response = response.bytes.into();
        self.int_flag = self.int_flag & !7 | response.int;
        self.int_flag & self.int_enable & 7 != 0
    }

    fn reply(&self, int: u8, bytes: &[u8]) -> Response {
        Response { int, bytes: bytes.to_vec() }
    }

    fn stat_reply(&self, int: u8) -> Response {
        self.reply(int, &[self.stat])
    }

    fn error(&self, code: u8) -> Response {
        self.reply(INT5, &[self.stat | ERROR, code])
    }

    fn lba_to_bcd(lba: u32) -> [u8; 3] {
        let f = lba + 150;
        [bcd(f / 4500), bcd(f / 75 % 60), bcd(f % 75)]
    }

    fn track_of(&self, lba: u32) -> usize {
        self.tracks.iter().rposition(|t| t.start <= lba).unwrap_or(0)
    }

    /// The command written: its acknowledgement (and any second response)
    /// scheduled from `now`.
    fn command(&mut self, command: u8, now: u64) {
        let params: Vec<u8> = self.params.drain(..).collect();
        let p = |i: usize| params.get(i).copied().unwrap_or(0);
        self.busy = true;
        self.second = None;
        let no_disc = self.disc.is_none();
        let ack = match command {
            0x01 => {
                let r = self.stat_reply(INT3);
                // Getstat clears the shell-open bit once it is closed.
                if !no_disc {
                    self.stat &= !SHELL_OPEN;
                }
                r
            }
            0x02 => {
                let (m, s, f) = (from_bcd(p(0)), from_bcd(p(1)), from_bcd(p(2)));
                self.setloc = Some(((m * 60 + s) * 75 + f).saturating_sub(150));
                self.stat_reply(INT3)
            }
            0x03 => {
                // Play: from a track if one is given, else from Setloc.
                if let Some(track) = params.first().map(|&t| from_bcd(t) as usize).filter(|&t| t > 0) {
                    if let Some(t) = self.tracks.get(track - 1) {
                        self.position = t.start;
                    }
                } else if let Some(lba) = self.setloc.take() {
                    self.position = lba;
                }
                self.start(Drive::Playing, now);
                self.stat_reply(INT3)
            }
            0x04 | 0x05 => self.stat_reply(INT3),
            0x06 | 0x1b => {
                if no_disc {
                    self.error(0x80)
                } else {
                    let r = self.stat_reply(INT3);
                    self.start(Drive::Reading, now);
                    r
                }
            }
            0x07 => {
                self.stat |= MOTOR;
                self.second = Some((now + ACK + SEEK, self.stat_reply(INT2)));
                self.stat_reply(INT3)
            }
            0x08 => {
                let r = self.stat_reply(INT3);
                self.stop_drive();
                self.stat &= !MOTOR;
                self.second = Some((now + ACK + SEEK, self.stat_reply(INT2)));
                r
            }
            0x09 => {
                let r = self.stat_reply(INT3);
                let was = self.drive;
                self.stop_drive();
                let delay = if was == Drive::Idle { 7000 } else { SECTOR };
                self.second = Some((now + ACK + delay, self.stat_reply(INT2)));
                r
            }
            0x0a => {
                let r = self.stat_reply(INT3);
                self.stop_drive();
                self.mode = 0x20;
                self.stat = if no_disc { SHELL_OPEN } else { MOTOR };
                self.second = Some((now + ACK + 80_000, self.stat_reply(INT2)));
                r
            }
            0x0b => {
                self.muted = true;
                self.stat_reply(INT3)
            }
            0x0c => {
                self.muted = false;
                self.stat_reply(INT3)
            }
            0x0d => {
                self.filter = (p(0), p(1));
                self.stat_reply(INT3)
            }
            0x0e => {
                self.mode = p(0);
                self.stat_reply(INT3)
            }
            0x0f => self.reply(INT3, &[self.stat, self.mode, 0, self.filter.0, self.filter.1]),
            0x10 => self.reply(INT3, &self.header.clone()),
            0x11 => {
                let track = self.track_of(self.position);
                let start = self.tracks.get(track).map_or(0, |t| t.start);
                let relative = self.position - start;
                let f = relative;
                let rel = [bcd(f / 4500), bcd(f / 75 % 60), bcd(f % 75)];
                let abs = Cdrom::lba_to_bcd(self.position);
                self.reply(INT3, &[bcd(track as u32 + 1), 1, rel[0], rel[1], rel[2], abs[0], abs[1], abs[2]])
            }
            0x12 => {
                self.second = Some((now + ACK + SEEK, self.stat_reply(INT2)));
                self.stat_reply(INT3)
            }
            0x13 => self.reply(INT3, &[self.stat, 1, bcd(self.tracks.len().max(1) as u32)]),
            0x14 => {
                let track = from_bcd(p(0)) as usize;
                let lba = match track {
                    0 => Some(self.end),
                    t => self.tracks.get(t - 1).map(|t| t.start),
                };
                match lba {
                    Some(lba) => {
                        let msf = Cdrom::lba_to_bcd(lba);
                        self.reply(INT3, &[self.stat, msf[0], msf[1]])
                    }
                    None => self.error(0x10),
                }
            }
            0x15 | 0x16 => {
                self.stop_drive();
                if let Some(lba) = self.setloc.take() {
                    self.position = lba;
                }
                let r = self.reply(INT3, &[self.stat | SEEKING]);
                self.second = Some((now + ACK + SEEK, self.stat_reply(INT2)));
                r
            }
            0x19 => match p(0) {
                0x20 => self.reply(INT3, &[0x94, 0x09, 0x19, 0xc0]),
                _ => self.error(0x10),
            },
            0x1a => {
                if no_disc {
                    self.second = Some((now + ACK + 20_000, self.reply(INT5, &[0x08, 0x40, 0, 0, 0, 0, 0, 0])));
                } else {
                    let region = self.region();
                    let mut id = vec![self.stat, 0x00, 0x20, 0x00];
                    id.extend_from_slice(region);
                    self.second = Some((now + ACK + 20_000, self.reply(INT2, &id)));
                }
                self.stat_reply(INT3)
            }
            0x1e => {
                self.second = Some((now + ACK + 500_000, self.stat_reply(INT2)));
                self.stat_reply(INT3)
            }
            _ => self.error(0x40),
        };
        self.ack = Some((now + ACK, ack));
    }

    /// The licence on sector 4 says the region.
    fn region(&mut self) -> &'static [u8] {
        let mut sector = [0; 2352];
        if let Some(disc) = &self.disc {
            disc.lock().unwrap().read(4, &mut sector);
        }
        let text = &sector[24..24 + 80];
        if text.windows(6).any(|w| w == b"Europe") {
            b"SCEE"
        } else if text.windows(4).any(|w| w == b"Inc.") {
            b"SCEI"
        } else {
            b"SCEA"
        }
    }

    fn start(&mut self, drive: Drive, now: u64) {
        let seek = match self.setloc.take() {
            Some(lba) => {
                self.position = lba;
                SEEK
            }
            None => 0,
        };
        self.drive = drive;
        self.stat &= !(READING | PLAYING | SEEKING);
        self.stat |= MOTOR | if drive == Drive::Reading { READING } else { PLAYING };
        self.next_sector = Some(now + ACK + seek + self.sector_time());
    }

    fn stop_drive(&mut self) {
        self.drive = Drive::Idle;
        self.next_sector = None;
        self.stat &= !(READING | PLAYING | SEEKING);
    }

    fn sector_time(&self) -> u64 {
        if self.mode & 0x80 != 0 { SECTOR / 2 } else { SECTOR }
    }

    /// The next sector comes off the disc: data for the CPU (unless the
    /// XA filter takes it for audio), or audio played.
    fn next_sector(&mut self, now: u64) -> Option<Response> {
        self.next_sector = Some(now + self.sector_time());
        let lba = self.position;
        self.position += 1;
        if lba >= self.end {
            self.stop_drive();
            return Some(self.stat_reply(INT4));
        }
        let mut sector = [0u8; 2352];
        let ok = match &self.disc {
            Some(disc) => disc.lock().unwrap().read(lba, &mut sector),
            None => false,
        };
        if self.drive == Drive::Playing {
            return self.play_sector(lba, &sector);
        }
        if !ok {
            return Some(self.error(0x04));
        }
        self.header.copy_from_slice(&sector[12..20]);
        // Audio and real-time sectors go to the XA decoder when ADPCM is on
        // and the filter (if on) takes their file and channel; with the
        // filter on, the CPU never gets them.
        let (submode, mode2) = (sector[18], sector[15] == 2);
        let xa = mode2 && submode & 0x44 == 0x44;
        let filtered = self.mode & 0x08 != 0;
        let matches = sector[16] == self.filter.0 && sector[17] == self.filter.1;
        if xa && self.mode & 0x40 != 0 && (!filtered || matches) {
            self.decode_xa(&sector);
            return None;
        }
        if xa && filtered {
            return None;
        }
        *self.sector = sector;
        Some(self.stat_reply(INT1))
    }

    /// A sector of CD-DA: its samples to the SPU; a position report now
    /// and then if Setmode asks; at a track's end with auto-pause on, INT4.
    fn play_sector(&mut self, lba: u32, sector: &[u8; 2352]) -> Option<Response> {
        let track = self.track_of(lba);
        if self.tracks.get(track).is_some_and(|t| t.audio) {
            for pair in sector.chunks_exact(4) {
                let l = i16::from_le_bytes([pair[0], pair[1]]);
                let r = i16::from_le_bytes([pair[2], pair[3]]);
                self.push_audio(l, r, self.muted);
            }
        }
        let next_track = self.tracks.get(track + 1).map_or(self.end, |t| t.start);
        if self.mode & 0x02 != 0 && lba + 1 >= next_track {
            self.stop_drive();
            return Some(self.stat_reply(INT4));
        }
        if self.mode & 0x04 != 0 && lba.is_multiple_of(10) {
            let start = self.tracks.get(track).map_or(0, |t| t.start);
            // Absolute and relative positions take turns; a relative one has
            // bit 7 of its seconds set.
            let msf = if lba.is_multiple_of(20) {
                Cdrom::lba_to_bcd(lba)
            } else {
                let f = lba - start;
                [bcd(f / 4500), bcd(f / 75 % 60) | 0x80, bcd(f % 75)]
            };
            return Some(self.reply(INT1, &[self.stat, bcd(track as u32 + 1), 1, msf[0], msf[1], msf[2], 0, 0]));
        }
        None
    }

    /// A stereo pair of CD audio through the volume matrix, for the SPU.
    fn push_audio(&mut self, l: i16, r: i16, muted: bool) {
        let (l, r) = (i32::from(l), i32::from(r));
        let v = self.volume.map(i32::from);
        let left = ((l * v[0] + r * v[3]) >> 7).clamp(-0x8000, 0x7fff) as i16;
        let right = ((r * v[2] + l * v[1]) >> 7).clamp(-0x8000, 0x7fff) as i16;
        // A second of sound is plenty to hold; more means nobody takes it.
        if self.audio.len() < 44100 {
            self.audio.push_back(if muted { [0; 2] } else { [left, right] });
        }
    }

    /// An XA-ADPCM sector: 18 portions of 128 bytes, each eight blocks of
    /// 28 four-bit samples (or four of eight-bit), mono or the blocks
    /// alternating left and right, at 37800 or 18900 Hz; out at 44100 Hz.
    fn decode_xa(&mut self, sector: &[u8; 2352]) {
        let coding = sector[19];
        let stereo = coding & 3 == 1;
        let half_rate = coding >> 2 & 1 == 1;
        let eight_bit = coding >> 4 & 3 == 1;
        let blocks = if eight_bit { 4 } else { 8 };
        let mut out: [Vec<i16>; 2] = [Vec::with_capacity(4704), Vec::with_capacity(4704)];
        for portion in sector[24..24 + 18 * 128].chunks_exact(128) {
            for block in 0..blocks {
                let header = portion[4 + block];
                let range = u32::from(header & 15).min(12);
                let range = if header & 15 > 12 { 9 } else { range };
                let (f0, f1) = XA_FILTERS[usize::from(header >> 4 & 3)];
                let side = if stereo { block & 1 } else { 0 };
                let ch = &mut self.xa[side];
                for j in 0..28 {
                    let t = if eight_bit {
                        (i32::from(portion[16 + block + j * 4] as i8) << 8) >> range
                    } else {
                        let byte = portion[16 + j * 4 + block / 2];
                        let nibble = (byte >> (4 * (block & 1))) & 15;
                        (i32::from((nibble << 4) as i8) << 8) >> range
                    };
                    let s = (t + ((ch.old * f0 + ch.older * f1 + 32) >> 6)).clamp(-0x8000, 0x7fff);
                    ch.older = ch.old;
                    ch.old = s;
                    ch.resample(s as i16, &mut out[side]);
                    if half_rate {
                        ch.resample(s as i16, &mut out[side]);
                    }
                }
            }
        }
        let muted = self.muted || self.xa_muted;
        let [left, right] = out;
        let right = if stereo { right } else { left.clone() };
        for (&l, &r) in left.iter().zip(right.iter()) {
            self.push_audio(l, r, muted);
        }
    }

    /// The request register: bit 7 moves the sector into the data FIFO,
    /// clearing it otherwise.
    fn request(&mut self, value: u8) {
        if value & 0x80 == 0 {
            self.data.clear();
            self.data_pos = 0;
            return;
        }
        if self.data_pos < self.data.len() {
            return;
        }
        let (from, len) = if self.mode & 0x20 != 0 {
            (12, 0x924)
        } else if self.sector[15] == 1 {
            (16, 0x800)
        } else {
            (24, 0x800)
        };
        self.data.clear();
        self.data.extend_from_slice(&self.sector[from..from + len]);
        self.data_pos = 0;
    }
}

/// The end of the data, as a response to a read.
const INT4: u8 = 4;

/// The 29-tap tables of the CD-ROM decoder's zigzag resampling from
/// 37800 Hz to 44100 Hz, one for each of the seven outputs (psx-spx).
const ZIGZAG: [[i32; 29]; 7] = [
    [0, 0, 0, 0, 0, -2, 10, -34, 65, -84, 52, 9, -266, 1024, -2680, 9036, 26516, -6016, 3021, -1571, 848, -365, 107, 10, -16, 17, -8, 3, -1],
    [0, 0, 0, -2, 0, 3, -19, 60, -75, 162, -227, 306, -67, -615, 3229, 29883, -4532, 2488, -1471, 882, -424, 166, -27, 5, 6, -8, 3, -1, 0],
    [0, 0, -1, 3, -2, -5, 31, -74, 179, -402, 689, -926, 1272, -1446, 31033, -1446, 1272, -926, 689, -402, 179, -74, 31, -5, -2, 3, -1, 0, 0],
    [0, -1, 3, -8, 6, 5, -27, 166, -424, 882, -1471, 2488, -4532, 29883, 3229, -615, -67, 306, -227, 162, -75, 60, -19, 3, 0, -2, 0, 0, 0],
    [-1, 3, -8, 17, -16, 10, 107, -365, 848, -1571, 3021, -6016, 26516, 9036, -2680, 1024, -266, 9, 52, -84, 65, -34, 10, -1, 0, 1, 0, 0, 0],
    [2, -8, 16, -35, 43, 26, -235, 635, -1352, 2810, -5882, 21472, 15367, -4681, 2062, -839, 347, -68, -23, 70, -35, 17, -5, 0, 0, 0, 0, 0, 0],
    [-5, 17, -35, 70, -23, -68, 347, -839, 2062, -4681, 15367, 21472, -5882, 2810, -1352, 635, -235, 26, 43, -35, 16, -8, 2, 0, 0, 0, 0, 0, 0],
];

/// XA-ADPCM's prediction filters (it has four of SPU-ADPCM's five).
const XA_FILTERS: [(i32, i32); 4] = [(0, 0), (60, 0), (115, -52), (98, -55)];

/// One channel of the XA decoder: the filter's last two samples, and the
/// resampler's ring of 37800 Hz samples.
#[derive(Clone, Copy, Default)]
struct XaChannel {
    old: i32,
    older: i32,
    ring: [i16; 32],
    at: usize,
    sixstep: u8,
}

state!(XaChannel { old, older, ring, at, sixstep });

/// Bytes of any length: their count, then them.
fn save_bytes(out: &mut Vec<u8>, bytes: impl ExactSizeIterator<Item = u8>) {
    bytes.len().save(out);
    out.extend(bytes);
}

fn load_bytes(input: &mut &[u8]) -> Result<Vec<u8>, BadState> {
    let mut len = 0usize;
    len.load(input)?;
    Ok(take(input, len)?.to_vec())
}

impl State for Response {
    fn save(&self, out: &mut Vec<u8>) {
        out.push(self.int);
        save_bytes(out, self.bytes.iter().copied());
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        self.int = take(input, 1)?[0];
        self.bytes = load_bytes(input)?;
        Ok(())
    }
}

/// The controller's state; the disc is the host's, given again on load.
impl State for Cdrom {
    fn save(&self, out: &mut Vec<u8>) {
        for v in [self.index, self.int_enable, self.int_flag, self.stat, self.mode, self.drive as u8] {
            out.push(v);
        }
        save_bytes(out, self.params.iter().copied());
        save_bytes(out, self.response.iter().copied());
        self.queue.len().save(out);
        self.queue.iter().for_each(|r| r.save(out));
        self.ack.clone().unwrap_or_default().save(out);
        out.push(u8::from(self.ack.is_some()));
        self.second.clone().unwrap_or_default().save(out);
        out.push(u8::from(self.second.is_some()));
        (self.busy, self.muted).save(out);
        (self.xa_muted, self.setloc).save(out);
        (self.position, self.next_sector).save(out);
        save_bytes(out, self.sector.iter().copied());
        save_bytes(out, self.data.iter().copied());
        (self.data_pos, self.header).save(out);
        [self.filter.0, self.filter.1].save(out);
        (self.volume, self.pending_volume).save(out);
        self.xa.save(out);
        self.audio.len().save(out);
        self.audio.iter().for_each(|&[l, r]| (l, r).save(out));
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        let b = take(input, 6)?;
        (self.index, self.int_enable, self.int_flag, self.stat, self.mode) = (b[0], b[1], b[2], b[3], b[4]);
        self.drive = *[Drive::Idle, Drive::Reading, Drive::Playing].get(usize::from(b[5])).ok_or(BadState)?;
        self.params = load_bytes(input)?.into();
        self.response = load_bytes(input)?.into();
        let mut n = 0usize;
        n.load(input)?;
        self.queue.clear();
        for _ in 0..n {
            let mut r = Response::default();
            r.load(input)?;
            self.queue.push_back(r);
        }
        let timed = |input: &mut &[u8]| -> Result<Option<(u64, Response)>, BadState> {
            let mut t = (0u64, Response::default());
            t.load(input)?;
            Ok(if take(input, 1)?[0] != 0 { Some(t) } else { None })
        };
        self.ack = timed(input)?;
        self.second = timed(input)?;
        let mut flags = (false, false);
        flags.load(input)?;
        (self.busy, self.muted) = flags;
        let mut more = (false, None);
        more.load(input)?;
        (self.xa_muted, self.setloc) = more;
        let mut at = (0u32, None);
        at.load(input)?;
        (self.position, self.next_sector) = at;
        let sector = load_bytes(input)?;
        self.sector.copy_from_slice(sector.get(..2352).ok_or(BadState)?);
        self.data = load_bytes(input)?;
        let mut d = (0usize, [0u8; 8]);
        d.load(input)?;
        (self.data_pos, self.header) = d;
        let mut filter = [0u8; 2];
        filter.load(input)?;
        self.filter = (filter[0], filter[1]);
        let mut volume = ([0u8; 4], [0u8; 4]);
        volume.load(input)?;
        (self.volume, self.pending_volume) = volume;
        self.xa.load(input)?;
        let mut n = 0usize;
        n.load(input)?;
        self.audio.clear();
        for _ in 0..n {
            let mut pair = (0i16, 0i16);
            pair.load(input)?;
            self.audio.push_back([pair.0, pair.1]);
        }
        Ok(())
    }
}

impl XaChannel {
    /// A 37800 Hz sample in; every sixth, seven 44100 Hz samples out.
    fn resample(&mut self, sample: i16, out: &mut Vec<i16>) {
        self.ring[self.at & 31] = sample;
        self.at += 1;
        self.sixstep = if self.sixstep <= 1 { 6 } else { self.sixstep - 1 };
        if self.sixstep != 6 {
            return;
        }
        for table in &ZIGZAG {
            let sum: i32 = (1..30).map(|i| (i32::from(self.ring[(self.at - i) & 31]) * table[i - 1]) >> 15).sum();
            out.push(sum.clamp(-0x8000, 0x7fff) as i16);
        }
    }
}

fn bcd(v: u32) -> u8 {
    (((v / 10 % 10) << 4) | (v % 10)) as u8
}

fn from_bcd(v: u8) -> u32 {
    u32::from(v >> 4) * 10 + u32::from(v & 15)
}

impl Psx {
    pub(crate) fn cdrom_write(&mut self, port: u32, value: u8) {
        let cd = &mut self.cdrom;
        match (port, cd.index) {
            (0, _) => cd.index = value & 3,
            (1, 0) => {
                let now = self.cycles;
                self.cdrom.command(value, now);
                self.schedule();
            }
            (2, 0) => {
                if cd.params.len() < 16 {
                    cd.params.push_back(value);
                }
            }
            (2, 1) => {
                cd.int_enable = value & 0x1f;
                if cd.int_flag & cd.int_enable != 0 {
                    self.request(crate::CDROM);
                }
            }
            (3, 0) => cd.request(value),
            (2, 2) => cd.pending_volume[0] = value,
            (3, 2) => cd.pending_volume[1] = value,
            (1, 3) => cd.pending_volume[2] = value,
            (2, 3) => cd.pending_volume[3] = value,
            (3, 3) => {
                cd.xa_muted = value & 1 != 0;
                if value & 0x20 != 0 {
                    cd.volume = cd.pending_volume;
                }
            }
            (3, 1) => {
                let before = cd.int_flag;
                cd.int_flag &= !(value & 0x1f);
                if value & 0x40 != 0 {
                    cd.params.clear();
                }
                // Acknowledged: the response FIFO empties, the next waiting
                // response comes.
                if before & 7 != 0 && cd.int_flag & 7 == 0 {
                    cd.response.clear();
                    if let Some(next) = cd.queue.pop_front() {
                        let raised = cd.respond(next);
                        if raised {
                            self.request(crate::CDROM);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) fn cdrom_read(&mut self, port: u32) -> u8 {
        self.cdrom.read(port)
    }

    /// Runs what is due: responses and sectors.
    pub(crate) fn cdrom_update(&mut self) {
        let now = self.cycles;
        let cd = &mut self.cdrom;
        let mut raised = false;
        if let Some((at, _)) = &cd.ack
            && *at <= now
        {
            let (_, response) = cd.ack.take().unwrap();
            cd.busy = false;
            raised |= cd.respond(response);
        }
        if cd.ack.is_none()
            && let Some((at, _)) = &cd.second
            && *at <= now
        {
            let (_, response) = cd.second.take().unwrap();
            raised |= cd.respond(response);
        }
        if let Some(at) = cd.next_sector
            && at <= now
            && let Some(response) = cd.next_sector(now)
        {
            // A sector the CPU has not taken is lost to the next one.
            if response.int == INT1 {
                cd.queue.retain(|r| r.int != INT1);
            }
            raised |= cd.respond(response);
        }
        if raised {
            self.request(crate::CDROM);
        }
    }
}
