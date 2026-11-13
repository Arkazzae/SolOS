//! The audio interface: a DMA that plays buffers of 16-bit stereo samples
//! (left first, big-endian) from RDRAM, at the rate AI_DACRATE sets, with
//! room for one buffer queued behind the one playing. Each buffer that
//! starts playing raises the AI interrupt, which games use to queue the
//! next. What plays is resampled (to the nearest sample) to SAMPLE_HZ for
//! the host as time passes, reading RDRAM as the DAC would. A buffer whose
//! last sample ends on an 8 KB boundary moves the next one 8 KB up (the
//! delayed carry, see `audio_event`).

use crate::{AI_INTR, AUDIO, CPU_HZ, N64};
use savestate::state;

/// The rate of the sound the host gets.
pub const SAMPLE_HZ: u32 = 48_000;

/// The clock AI_DACRATE divides: the video interface's, on NTSC.
const DAC_HZ: u64 = 48_681_812;

#[derive(Clone, Default)]
pub struct Ai {
    /// The address for the next buffer, as written.
    dram_addr: u32,
    /// The buffers playing and queued: address and length in bytes (a
    /// length of 0 when none plays). The playing one's address is where the
    /// AI reads it, after the delayed carry; the queued one's as written.
    playing: (u32, u32),
    queued: Option<(u32, u32)>,
    /// When the buffer playing started, and how long it lasts, in cycles.
    started: u64,
    duration: u64,
    enabled: bool,
    dacrate: u32,
    bitrate: u32,
    /// Where the output has got to: the cycle, and the place in the buffer
    /// playing (in samples, 32.32 fixed point).
    mixed: u64,
    position: u64,
    /// The sound since the frame began, for the host.
    pub samples: Vec<i16>,
}

state!(Ai { dram_addr, playing, queued, started, duration, enabled, dacrate, bitrate, mixed, position });

impl Ai {
    /// The DAC's rate in samples a second.
    fn rate(&self) -> u64 {
        DAC_HZ / (u64::from(self.dacrate) + 1)
    }
}

/// The buffer that starts after one has ended, with the delayed carry: 8 KB
/// higher when the ended one's last sample was the last of an 8 KB page (in
/// the 24 bits of the AI's address).
fn carried(ended: (u32, u32), next: (u32, u32)) -> (u32, u32) {
    let (addr, length) = ended;
    if length > 0 && (addr + length) & 0x1fff == 0 {
        ((next.0 + 0x2000) & 0xff_fff8, next.1)
    } else {
        next
    }
}

/// The host's samples up to a cycle, counted from power on.
fn output_samples(cycle: u64) -> u64 {
    cycle * u64::from(SAMPLE_HZ) / CPU_HZ
}

impl N64 {
    pub(crate) fn read_ai(&mut self, addr: u32) -> u32 {
        let ai = &self.ai;
        match addr & 0x1c {
            // The length left of the buffer playing.
            0x04 if ai.playing.1 > 0 && ai.duration > 0 => {
                let played = (self.cycles - ai.started).min(ai.duration);
                let left = u64::from(ai.playing.1) * (ai.duration - played) / ai.duration;
                left as u32 & 0x3fff8
            }
            0x04 => 0,
            // Full with a buffer queued, busy while one plays.
            0x0c => {
                let full = ai.queued.is_some();
                u32::from(full) << 31 | u32::from(ai.playing.1 > 0) << 30 | u32::from(full) | 1 << 24 | 1 << 20
            }
            _ => 0,
        }
    }

    pub(crate) fn write_ai(&mut self, addr: u32, value: u32) {
        match addr & 0x1c {
            0x00 => self.ai.dram_addr = value & 0xff_fff8,
            0x04 => {
                let length = value & 0x3fff8;
                if length == 0 || self.ai.queued.is_some() {
                    return;
                }
                let buffer = (self.ai.dram_addr, length);
                if self.ai.playing.1 == 0 {
                    let now = self.cycles;
                    self.mix_audio(now);
                    self.play(buffer);
                } else {
                    self.ai.queued = Some(buffer);
                }
            }
            0x08 => self.ai.enabled = value & 1 != 0,
            0x0c => self.acknowledge(AI_INTR),
            0x10 => self.ai.dacrate = value & 0x3fff,
            _ => self.ai.bitrate = value & 0xf,
        }
    }

    /// Starts a buffer playing: four bytes a sample, at the DAC's rate.
    fn play(&mut self, buffer: (u32, u32)) {
        let ai = &mut self.ai;
        ai.playing = buffer;
        ai.started = self.cycles;
        ai.position = 0;
        ai.duration = (u64::from(buffer.1 / 4) * CPU_HZ / ai.rate().max(1)).max(1);
        let end = self.cycles + self.ai.duration;
        self.schedule(AUDIO, end);
        self.request(AI_INTR);
    }

    /// The buffer playing has ended: the queued one starts.
    ///
    /// The AI's address counter has a delayed carry (n64brew "Audio
    /// Interface"): its low 13 bits step through the buffer and carry into
    /// the upper 11 bits one sample late. Inside a buffer that changes
    /// nothing, but when a buffer's last sample ends exactly on an 8 KB
    /// boundary the carry lands on the next buffer's address, which then
    /// plays from 0x2000 higher than written. libultra's osAiSetNextBuffer
    /// writes the buffer after such a one 0x2000 low to make up for it
    /// (libdragon works around it too). When nothing is queued the AI runs
    /// dry and the carry is lost: a buffer started on an idle AI plays where
    /// it was written. So the carry follows from the buffer that ended,
    /// which the save state holds, and needs no state of its own.
    pub(crate) fn audio_event(&mut self, at: u64) {
        self.mix_audio(at);
        let ended = self.ai.playing;
        self.ai.playing = (0, 0);
        if let Some(buffer) = self.ai.queued.take() {
            self.play(carried(ended, buffer));
        }
    }

    /// Makes the host's samples up to a cycle from the buffer playing (or
    /// silence).
    pub(crate) fn mix_audio(&mut self, until: u64) {
        if until <= self.ai.mixed {
            return;
        }
        let (addr, length) = self.ai.playing;
        self.touch_rdram(addr as usize, length as usize);
        let ai = &mut self.ai;
        let count = output_samples(until) - output_samples(ai.mixed);
        ai.mixed = until;
        let (addr, length) = ai.playing;
        let step = (ai.rate() << 32) / u64::from(SAMPLE_HZ);
        for _ in 0..count {
            let index = (ai.position >> 32) as u32;
            let pair = if length > 0 && ai.enabled && index < length / 4 {
                let at = (addr + index * 4) as usize & 0x7f_fffc;
                let bytes = &self.rdram[at..at + 4];
                [i16::from_be_bytes([bytes[0], bytes[1]]), i16::from_be_bytes([bytes[2], bytes[3]])]
            } else {
                [0, 0]
            };
            ai.samples.extend_from_slice(&pair);
            ai.position += step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A console with the AI's DMA on, playing at about 32 kHz.
    fn console() -> N64 {
        let mut rom = vec![0u8; 0x1000];
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        let mut n64 = N64::new(&rom).unwrap();
        n64.write_ai(0x10, 1520);
        n64.write_ai(0x08, 1);
        n64
    }

    /// Fills RDRAM with samples of one value.
    fn fill(n64: &mut N64, addr: usize, length: usize, value: i16) {
        for at in (addr..addr + length).step_by(2) {
            n64.rdram[at..at + 2].copy_from_slice(&value.to_be_bytes());
        }
    }

    /// Writes a buffer's address and length, as osAiSetNextBuffer does.
    fn buffer(n64: &mut N64, addr: u32, length: u32) {
        n64.write_ai(0x00, addr);
        n64.write_ai(0x04, length);
    }

    /// Runs to the end of the buffer playing, as the event loop would.
    fn finish(n64: &mut N64) {
        let at = n64.events[AUDIO];
        assert!(at != u64::MAX, "a buffer plays");
        n64.cycles = at;
        n64.schedule(AUDIO, u64::MAX);
        n64.audio_event(at);
    }

    /// A buffer ending on an 8 KB boundary moves the one queued behind it
    /// 0x2000 up: that is where it plays from, and what the host hears.
    #[test]
    fn a_buffer_ending_on_8_kb_moves_the_next_one_up() {
        let mut n64 = console();
        fill(&mut n64, 0x1fe0, 0x20, 1);
        fill(&mut n64, 0x100, 0x20, 2);
        fill(&mut n64, 0x2100, 0x20, 3);
        buffer(&mut n64, 0x1fe0, 0x20);
        buffer(&mut n64, 0x100, 0x20);
        finish(&mut n64);
        assert_eq!(n64.ai.playing, (0x2100, 0x20));
        finish(&mut n64);
        assert_eq!(n64.ai.playing, (0, 0));
        let heard = &n64.ai.samples;
        assert!(heard.contains(&1) && heard.contains(&3) && !heard.contains(&2));
    }

    /// The carry lands on the buffer that follows even when that one ends
    /// on a boundary too, and moves the one after it in turn.
    #[test]
    fn the_carry_follows_a_chain_of_buffers() {
        let mut n64 = console();
        buffer(&mut n64, 0x1fe0, 0x20);
        buffer(&mut n64, 0x1fe0, 0x20);
        finish(&mut n64);
        assert_eq!(n64.ai.playing, (0x3fe0, 0x20));
        buffer(&mut n64, 0x100, 0x20);
        finish(&mut n64);
        assert_eq!(n64.ai.playing, (0x2100, 0x20));
        buffer(&mut n64, 0x100, 0x20);
        finish(&mut n64);
        assert_eq!(n64.ai.playing, (0x100, 0x20));
    }

    /// With nothing queued the AI runs dry and the carry is lost: the next
    /// buffer, started on an idle AI, plays where it was written.
    #[test]
    fn the_carry_is_lost_when_the_ai_runs_dry() {
        let mut n64 = console();
        buffer(&mut n64, 0x1fe0, 0x20);
        finish(&mut n64);
        assert_eq!(n64.ai.playing, (0, 0));
        buffer(&mut n64, 0x100, 0x20);
        assert_eq!(n64.ai.playing, (0x100, 0x20));
    }

    /// A buffer that ends short of a boundary, or crosses one before its
    /// last sample, leaves the next buffer where it was written; the one
    /// that crosses plays straight on over the boundary.
    #[test]
    fn no_carry_without_the_last_sample_on_a_boundary() {
        let mut n64 = console();
        fill(&mut n64, 0x1ff0, 0x10, 1);
        fill(&mut n64, 0x2000, 0x10, 4);
        buffer(&mut n64, 0x1fd8, 0x20);
        buffer(&mut n64, 0x1ff0, 0x20);
        finish(&mut n64);
        assert_eq!(n64.ai.playing, (0x1ff0, 0x20));
        buffer(&mut n64, 0x100, 0x20);
        let before = n64.ai.samples.len();
        finish(&mut n64);
        assert_eq!(n64.ai.playing, (0x100, 0x20));
        let heard = &n64.ai.samples[before..];
        assert!(heard.contains(&1) && heard.contains(&4));
    }

    /// The rule on its own: the ended buffer's end on a boundary moves the
    /// next by 0x2000, in the 24 bits of the AI's address (wrapping at 16 MB).
    #[test]
    fn the_rule_on_addresses() {
        assert_eq!(carried((0xff_ffe0, 0x20), (0xff_e100, 8)), (0x100, 8));
        assert_eq!(carried((0x7f_e000, 0x2000), (0x7f_c000, 0x2000)), (0x7f_e000, 0x2000));
        assert_eq!(carried((0x7f_e000, 0x1ff8), (0x7f_c000, 0x2000)), (0x7f_c000, 0x2000));
    }
}
