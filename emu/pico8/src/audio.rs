//! PICO-8's sound: four channels playing sound effects from RAM (0x3200,
//! 68 bytes each) and the music patterns (0x3100) that start them; one
//! channel out at 22 050 Hz. A note lasts its effect's speed times 183
//! samples.
//!
//! The synthesis follows zepto8's measurements of PICO-8's WAV exports
//! (github.com/samhocevar/zepto8, src/synth.cpp and src/pico8/sfx.cpp,
//! WTFPL): the eight waveforms and their buzz and noiz variants, detune as
//! a second wave, the effects, a short crossfade where a note changes
//! sharply, reverb from two delay lines, and dampen as high-shelf filters.
//! The registers at 0x5f40..0x5f43 (half rate, reverb, distort, lowpass)
//! act on the channels as there.

pub const SAMPLE_HZ: usize = 22_050;
const TICK: u32 = 183;
const RATE: f32 = SAMPLE_HZ as f32;

/// The frequency of a key, 33 being A4 (440 Hz): from a table for the
/// 64 keys notes have.
fn key_hz(key: f32) -> f32 {
    static TABLE: std::sync::LazyLock<[f32; 64]> =
        std::sync::LazyLock::new(|| std::array::from_fn(|k| 440.0 * ((k as f32 - 33.0) / 12.0).exp2()));
    let k = key as usize;
    if k as f32 == key && k < 64 { TABLE[k] } else { 440.0 * ((key - 33.0) / 12.0).exp2() }
}

/// A sound effect's settings, from RAM.
#[derive(Clone, Copy)]
struct Sfx {
    speed: u32,
    loop_start: usize,
    loop_end: usize,
    filters: u8,
}

fn sfx_at(ram: &[u8; 0x10000], n: usize) -> Sfx {
    let a = 0x3200 + (n & 63) * 68;
    Sfx { speed: u32::from(ram[a + 65]).max(1), loop_start: usize::from(ram[a + 66]), loop_end: usize::from(ram[a + 67]), filters: ram[a + 64] }
}

impl Sfx {
    fn loops(&self) -> bool {
        self.loop_end > self.loop_start
    }
    /// Notes it plays when it does not loop, as music counts them.
    fn length(&self) -> usize {
        if self.loop_end == 0 && self.loop_start > 0 { self.loop_start.min(32) } else { 32 }
    }
}

#[derive(Clone, Copy, Default)]
struct Note {
    key: f32,
    wave: u8,
    vol: f32,
    fx: u8,
    custom: bool,
}

fn note_at(ram: &[u8; 0x10000], n: usize, i: usize) -> Note {
    let a = 0x3200 + (n & 63) * 68 + (i & 31) * 2;
    let w = u16::from_le_bytes([ram[a], ram[a + 1]]);
    Note {
        key: f32::from((w & 63) as u8),
        wave: ((w >> 6) & 7) as u8,
        vol: f32::from(((w >> 9) & 7) as u8) / 7.0,
        fx: ((w >> 12) & 7) as u8,
        custom: w & 0x8000 != 0,
    }
}

/// What a channel plays at a sample.
#[derive(Clone, Copy, Default, PartialEq)]
struct Synth {
    hz: f32,
    vol: f32,
    wave: u8,
    filters: u8,
    key: f32,
}

/// An oscillator: where it is in its period, its whole periods (to 110,
/// for the phaser's second triangle), and the noise's last value.
#[derive(Clone, Copy, Default)]
struct Osc {
    phase: f32,
    cycles: f32,
    noise: f32,
    seed: u32,
}

impl Osc {
    fn advance(&mut self, hz: f32) {
        self.phase += hz / RATE;
        if self.phase >= 1.0 {
            let whole = self.phase.floor();
            self.phase -= whole;
            self.cycles = (self.cycles + whole) % 110.0;
        }
    }
    fn random(&mut self) -> f32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        (self.seed >> 8) as f32 / 8_388_608.0 - 1.0
    }
}

/// One waveform's sample at the oscillator's phase.
fn waveform(wave: u8, filters: u8, key: f32, hz: f32, o: &mut Osc) -> f32 {
    let t = o.phase;
    let noiz = filters & 2 != 0;
    let buzz = filters & 4 != 0;
    let tilted = |t: f32, a: f32| if t < a { 2.0 * t / a - 1.0 } else { 2.0 * (1.0 - t) / (1.0 - a) - 1.0 };
    match wave {
        0 => {
            let mut r = 1.0 - (4.0 * t - 2.0).abs();
            if buzz {
                r = r * 0.75 + tilted(t, 0.875) * 0.25;
            }
            r * 0.5
        }
        1 => tilted(t, if buzz { 0.975 } else { 0.875 }) * 0.5,
        2 => {
            let mut r = if t < 0.5 { t } else { t - 1.0 };
            if buzz {
                // A small offset every other period.
                let two = (o.cycles % 2.0 + t - 1.0).abs() < 0.5;
                r = r * 0.83 - if two { 0.085 } else { 0.0 };
            }
            0.653 * r
        }
        3 => {
            if t < if buzz { 0.4 } else { 0.5 } {
                0.25
            } else {
                -0.25
            }
        }
        4 => {
            if t < if buzz { 0.255 } else { 0.316 } {
                0.25
            } else {
                -0.25
            }
        }
        5 => {
            let mut r = if t < 0.5 { 3.0 - (24.0 * t - 6.0).abs() } else { 1.0 - (16.0 * t - 12.0).abs() };
            if buzz {
                if t < 0.5 {
                    r = r * 2.0 + 3.0;
                }
                r = if t < 0.5 && r > -1.875 { r * 0.2 - 1.0 } else { r + 0.5 };
            }
            r / 9.0
        }
        6 => {
            // A random walk that follows the pitch.
            let scale = hz / RATE * 8.858_923;
            let new = (o.noise + scale * o.random()) / (1.0 + scale);
            o.noise = new;
            let f = 1.0 - key / 63.0;
            let mut r = new * 1.5 * (1.0 + f * f);
            if noiz {
                r *= 2.0 * if t < 0.5 { t } else { t - 1.0 };
            }
            r
        }
        _ => {
            // Two triangles 109 to 110 apart.
            let adv = o.cycles + t;
            let mut r = 2.0 - (8.0 * t - 4.0).abs();
            r += 1.0 - (4.0 * ((adv * 109.0 / 110.0) % 1.0) - 2.0).abs();
            if buzz {
                r += 0.25 - ((adv * 2.0 + 0.5) % 1.0 - 0.5).abs();
                r += 0.125 - (0.5 * ((adv * 4.0) % 1.0) - 0.25).abs();
            }
            r / 6.0
        }
    }
}

/// A sample of what a channel plays: the wave, and detune's second one.
fn synth_sample(s: &Synth, o: &mut Osc) -> f32 {
    let mut v = waveform(s.wave, s.filters, s.key, s.hz, o);
    let detune = (s.filters / 8) % 3;
    if detune != 0 && s.wave != 6 {
        let factor = match (s.wave, detune) {
            (0, 1) => 3.0 / 4.0,
            (0, _) => 3.0 / 2.0,
            (5, 1) => 200.0 / 199.0,
            (5, _) => 800.0 / 199.0,
            (7, 1) => 49.0 / 50.0,
            (7, _) => 400.0 / 199.0,
            (_, 1) => 200.0 / 199.0,
            _ => 400.0 / 199.0,
        };
        let adv = (o.cycles + o.phase) * factor;
        let mut second = Osc { phase: adv % 1.0, cycles: adv.floor() % 110.0, ..*o };
        let wave = if detune == 2 && s.wave == 5 { 0 } else { s.wave };
        v += waveform(wave, s.filters, s.key, s.hz * factor, &mut second) * 0.5;
    }
    (v * s.vol).clamp(-1.0, 1.0)
}

/// A sound effect playing: a channel's, or a custom instrument's.
#[derive(Clone, Default)]
struct Voice {
    sfx: usize,
    /// Samples into it since its first note, back by a loop when it loops.
    pos: u32,
    /// Samples it has played, for its length.
    time: u32,
    start: usize,
    /// The notes it may play (sfx's length argument), or 0 for all.
    length: u32,
    can_loop: bool,
    music: bool,
    prev_key: f32,
    prev_vol: f32,
    last_note: usize,
    /// The note after its last with a sound, found again at each note.
    sounding: usize,
}

impl Voice {
    fn new(sfx: usize, start: usize, length: u32, music: bool) -> Voice {
        Voice { sfx, start, length, can_loop: true, music, last_note: start, sounding: 32, ..Voice::default() }
    }

    /// The note it is on and the fraction through it, moving on a sample
    /// (at half speed, every other); None once it has ended.
    fn step(&mut self, ram: &[u8; 0x10000], slow: bool) -> Option<(usize, f32, Sfx)> {
        let s = sfx_at(ram, self.sfx);
        let dur = s.speed * TICK;
        let mut i = self.start + (self.pos / dur) as usize;
        if s.loops() && self.can_loop && i >= s.loop_end {
            let span = s.loop_end - s.loop_start;
            let back = (i - s.loop_start) / span * span;
            self.pos -= back as u32 * dur;
            i -= back;
        }
        // Where it ends: its length, the loop's start when that is all it
        // has, and (not for music) its last note with a sound.
        if self.length > 0 && (self.time / dur) >= self.length {
            return None;
        }
        let mut end = 32usize;
        if !self.music && s.loop_end == 0 && s.loop_start > 0 {
            end = end.min(s.loop_start);
        }
        if i != self.last_note || self.time == 0 {
            if i != self.last_note {
                let prev = note_at(ram, self.sfx, self.last_note);
                (self.prev_key, self.prev_vol) = (prev.key, prev.vol);
                self.last_note = i;
            }
            self.sounding = (0..32).rev().find(|&n| note_at(ram, self.sfx, n).vol > 0.0).map_or(0, |n| n + 1);
        }
        if !s.loops() && !self.music {
            end = end.min(self.sounding);
        }
        if i >= end {
            return None;
        }
        let t = (self.pos % dur) as f32 / dur as f32;
        if !slow || self.time & 1 == 0 {
            self.pos += 1;
        }
        self.time += 1;
        Some((i, t, s))
    }

    /// Seconds into the effect, for vibrato and arpeggios.
    fn seconds(&self, s: &Sfx) -> f32 {
        (self.start as f32 * (s.speed * TICK) as f32 + self.pos as f32) / RATE
    }
}

/// The frequency and volume of a note with its effect, its frequency
/// scaled by factor (a custom instrument's notes follow the note's key).
fn effect(ram: &[u8; 0x10000], v: &Voice, s: &Sfx, note: &Note, i: usize, t: f32, factor: f32) -> (f32, f32) {
    let (mut hz, mut vol) = (key_hz(note.key) * factor, note.vol);
    let secs = v.seconds(s);
    match note.fx {
        // From the note before, in pitch, and in volume when it sounded.
        1 => {
            let from = key_hz(v.prev_key) * factor;
            hz = from + (hz - from) * t;
            if v.prev_vol > 0.0 {
                vol = v.prev_vol + (vol - v.prev_vol) * t;
            }
        }
        // A quarter of a semitone up and down, 7.5 times a second.
        2 => {
            let w = ((7.5 * secs) % 1.0 - 0.5).abs() - 0.25;
            hz += (hz * 1.059_463_1 - hz) * w;
        }
        3 => hz *= 1.0 - t,
        4 => vol *= t,
        5 => vol *= 1.0 - t,
        // The group of four notes this one is in, at a speed of 4 or 8
        // (2 or 4 when the effect's speed is 8 or less).
        6 | 7 => {
            let m = if s.speed <= 8 { 32.0 } else { 16.0 } / if note.fx == 6 { 4.0 } else { 8.0 };
            let n = (m * 7.5 * secs) as usize;
            hz = key_hz(note_at(ram, v.sfx, (i & !3) | (n & 3)).key) * factor;
        }
        _ => {}
    }
    (hz, vol)
}

/// A high-shelf biquad (the audio EQ cookbook's), for dampen.
#[derive(Clone, Copy, Default)]
struct Shelf {
    x: [f32; 2],
    y: [f32; 2],
}

struct Coeffs([f32; 5]);

fn high_shelf(f0: f32, gain_db: f32) -> Coeffs {
    let a = 10f32.powf(gain_db / 40.0);
    let w0 = std::f32::consts::TAU * f0 / RATE;
    let (cos, alpha) = (w0.cos(), w0.sin() / 2.0);
    let sq = 2.0 * a.sqrt() * alpha;
    let b0 = a * ((a + 1.0) + (a - 1.0) * cos + sq);
    let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos);
    let b2 = a * ((a + 1.0) + (a - 1.0) * cos - sq);
    let a0 = (a + 1.0) - (a - 1.0) * cos + sq;
    let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos);
    let a2 = (a + 1.0) - (a - 1.0) * cos - sq;
    Coeffs([b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0])
}

impl Shelf {
    fn run(&mut self, c: &Coeffs, x: f32) -> f32 {
        let [b0, b1, b2, a1, a2] = c.0;
        let y = b0 * x + b1 * self.x[0] + b2 * self.x[1] - a1 * self.y[0] - a2 * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

const REVERB: usize = 732;

#[derive(Clone)]
struct Channel {
    voice: Option<Voice>,
    /// A custom instrument playing: its effect, and the main note's
    /// instrument and key it belongs to.
    inner: Option<(Voice, u8, f32)>,
    music: bool,
    note: i32,
    sfx: i32,
    osc: Osc,
    last: Synth,
    /// Where a note changed sharply: the one before, fading out.
    fade_synth: Synth,
    fade_osc: Osc,
    fade: f32,
    reverb: Box<[f32; REVERB]>,
    at: usize,
    damp: [Shelf; 2],
    /// Samples it has been silent: past the reverb's length, nothing
    /// is left to play.
    quiet: u32,
}

impl Default for Channel {
    fn default() -> Channel {
        Channel {
            voice: None,
            inner: None,
            music: false,
            note: -1,
            sfx: -1,
            osc: Osc { seed: 0x2545_f491, ..Osc::default() },
            last: Synth::default(),
            fade_synth: Synth::default(),
            fade_osc: Osc::default(),
            fade: 0.0,
            reverb: Box::new([0.0; REVERB]),
            at: 0,
            damp: [Shelf::default(); 2],
            quiet: u32::MAX,
        }
    }
}

impl Channel {
    /// Stops what it plays; its tails (fade, reverb) die out on their own.
    fn stop(&mut self) {
        self.voice = None;
        self.inner = None;
        self.music = false;
        self.note = -1;
        self.sfx = -1;
    }
}

#[derive(Clone, Default)]
struct Music {
    pattern: usize,
    /// Patterns played since it started.
    count: i32,
    pos: u32,
    length: u32,
    /// Channels it plays on.
    mask: u8,
    volume: f32,
    /// Change of volume a sample (fades).
    fade: f32,
    stop_at_zero: bool,
}

#[derive(Clone)]
pub struct Audio {
    /// 8-bit PCM from serial(0x808), 5512 Hz, waiting to play.
    pcm: std::collections::VecDeque<u8>,
    pcm_step: u8,
    channels: [Channel; 4],
    music: Option<Music>,
    /// Samples owed from fractional frames (a 60 Hz frame is 367.5).
    owed: u32,
}

impl Default for Audio {
    fn default() -> Audio {
        Audio::new()
    }
}

impl Audio {
    pub fn new() -> Audio {
        let mut channels: [Channel; 4] = Default::default();
        for (i, c) in channels.iter_mut().enumerate() {
            c.osc.seed ^= (i as u32).wrapping_mul(0x9e37_79b9);
        }
        Audio { pcm: Default::default(), pcm_step: 0, channels, music: None, owed: 0 }
    }

    /// sfx(n, channel, offset, length).
    pub fn sfx(&mut self, n: i32, channel: i32, offset: i32, length: i32) {
        match n {
            -1 => {
                for (i, c) in self.channels.iter_mut().enumerate() {
                    if (channel < 0 && !c.music) || channel as usize == i {
                        c.stop();
                    }
                }
                return;
            }
            -2 => {
                for (i, c) in self.channels.iter_mut().enumerate() {
                    if (channel < 0 || channel as usize == i)
                        && let Some(v) = &mut c.voice
                    {
                        v.can_loop = false;
                    }
                }
                return;
            }
            _ => {}
        }
        if !(0..64).contains(&n) {
            return;
        }
        if channel == -2 {
            for c in &mut self.channels {
                if c.sfx == n {
                    c.stop();
                }
            }
            return;
        }
        let music_mask = self.music.as_ref().map_or(0, |m| m.mask);
        let ch = if (0..4).contains(&channel) {
            // The same effect elsewhere stops.
            for (i, c) in self.channels.iter_mut().enumerate() {
                if i as i32 != channel && c.sfx == n && !c.music {
                    c.stop();
                }
            }
            channel as usize
        } else {
            let free = |i: usize, c: &Channel| c.voice.is_none() && music_mask & (1 << i) == 0;
            match self.channels.iter().enumerate().position(|(i, c)| free(i, c)) {
                Some(i) => i,
                None => match self.channels.iter().position(|c| c.sfx == n && !c.music) {
                    Some(i) => i,
                    None => (0..4).rev().find(|&i| music_mask & (1 << i) == 0).unwrap_or(3),
                },
            }
        };
        let start = offset.clamp(0, 31) as usize;
        let c = &mut self.channels[ch];
        c.voice = Some(Voice::new(n as usize, start, length.max(0) as u32, false));
        c.inner = None;
        c.music = false;
        c.note = start as i32;
        c.sfx = n;
    }

    /// music(n, fade_ms, mask).
    pub fn music(&mut self, ram: &[u8; 0x10000], n: i32, fade_ms: i32, mask: i32) {
        let fade_samples = (fade_ms.max(0) as f32 * RATE / 1000.0).max(1.0);
        if n < 0 {
            match &mut self.music {
                Some(m) if fade_ms > 0 => {
                    m.fade = -m.volume / fade_samples;
                    m.stop_at_zero = true;
                }
                _ => self.stop_music(),
            }
            return;
        }
        self.stop_music();
        let mask = if mask == 0 { 15 } else { (mask & 15) as u8 };
        let mut m = Music { pattern: (n & 63) as usize, count: 0, mask, volume: 1.0, ..Music::default() };
        if fade_ms > 0 {
            m.volume = 0.0;
            m.fade = 1.0 / fade_samples;
        }
        self.music = Some(m);
        self.start_pattern(ram);
    }

    fn stop_music(&mut self) {
        self.music = None;
        for c in &mut self.channels {
            if c.music {
                c.stop();
            }
        }
    }

    fn start_pattern(&mut self, ram: &[u8; 0x10000]) {
        let Some(m) = &mut self.music else { return };
        let a = 0x3100 + m.pattern * 4;
        let (mut first_non_loop, mut longest, mut any) = (None, 0, false);
        let mask = m.mask;
        for i in 0..4 {
            let b = ram[a + i];
            let on = b & 0x40 == 0 && mask & (1 << i) != 0;
            if !on {
                if self.channels[i].music {
                    self.channels[i].stop();
                }
                continue;
            }
            any = true;
            let n = usize::from(b & 63);
            let s = sfx_at(ram, n);
            let len = s.speed * TICK * s.length() as u32;
            if !s.loops() && first_non_loop.is_none() {
                first_non_loop = Some(len);
            }
            longest = longest.max(s.speed * TICK * 32);
            let c = &mut self.channels[i];
            c.voice = Some(Voice::new(n, 0, 0, true));
            c.inner = None;
            c.music = true;
            c.note = 0;
            c.sfx = n as i32;
        }
        if !any {
            self.stop_music();
            return;
        }
        if let Some(m) = &mut self.music {
            m.length = first_non_loop.unwrap_or(longest).max(1);
            m.pos = 0;
        }
    }

    fn next_pattern(&mut self, ram: &[u8; 0x10000]) {
        let Some(m) = &mut self.music else { return };
        let a = 0x3100 + m.pattern * 4;
        let (loop_back, stop) = (ram[a + 1] & 0x80 != 0, ram[a + 2] & 0x80 != 0);
        if loop_back {
            let mut p = m.pattern;
            while p > 0 && ram[0x3100 + p * 4] & 0x80 == 0 {
                p -= 1;
            }
            m.pattern = p;
        } else if stop || m.pattern == 63 {
            self.stop_music();
            return;
        } else {
            m.pattern += 1;
        }
        m.count += 1;
        self.start_pattern(ram);
    }

    /// The samples of a frame at fps frames a second.
    pub fn render(&mut self, ram: &[u8; 0x10000], fps: u32, out: &mut Vec<i16>) {
        out.clear();
        let total = SAMPLE_HZ as u32 + self.owed;
        let n = total / fps.max(1);
        self.owed = total % fps.max(1);
        let coeffs = [high_shelf(2400.0, -6.0), high_shelf(1000.0, -12.0)];
        for _ in 0..n {
            let music_volume = match &mut self.music {
                Some(m) => {
                    m.volume = (m.volume + m.fade).clamp(0.0, 1.0);
                    if m.fade > 0.0 && m.volume >= 1.0 {
                        m.fade = 0.0;
                    }
                    let v = m.volume;
                    if m.stop_at_zero && v <= 0.0 {
                        self.stop_music();
                    }
                    v
                }
                None => 1.0,
            };
            let mut mix = 0.0f32;
            for c in 0..4 {
                mix += self.sample(ram, c, music_volume, &coeffs);
            }
            if let Some(&b) = self.pcm.front() {
                // Each PCM sample lasts four of ours (5512.5 Hz).
                mix += (f32::from(b) - 128.0) * 128.0;
                self.pcm_step += 1;
                if self.pcm_step == 4 {
                    self.pcm_step = 0;
                    self.pcm.pop_front();
                }
            }
            out.push(mix.clamp(-32767.0, 32767.0) as i16);
            let advance = match &mut self.music {
                Some(m) => {
                    m.pos += 1;
                    m.pos >= m.length
                }
                None => false,
            };
            if advance {
                self.next_pattern(ram);
            }
        }
    }

    /// What a channel plays now, if anything, moving it on a sample.
    fn synth(&mut self, ram: &[u8; 0x10000], c: usize) -> Option<Synth> {
        let half = ram[0x5f40];
        let ch = &mut self.channels[c];
        let v = ch.voice.as_mut()?;
        let Some((i, t, s)) = v.step(ram, half & (1 << c) != 0) else {
            ch.stop();
            return None;
        };
        ch.note = i as i32;
        let note = note_at(ram, v.sfx, i);
        if note.vol <= 0.0 {
            return Some(Synth { vol: 0.0, ..ch.last });
        }
        let (mut hz, vol) = effect(ram, v, &s, &note, i, t, 1.0);
        if half & (1 << (c + 4)) != 0 {
            hz *= 0.5;
        }
        if !note.custom {
            ch.inner = None;
            return Some(Synth { hz, vol, wave: note.wave, filters: s.filters, key: note.key });
        }
        // A custom instrument: effect `wave`, its notes moved by this
        // note's pitch from C2, at this note's volume; it starts again
        // when the instrument or the key changes.
        let fresh = !matches!(&ch.inner, Some((_, w, k)) if *w == note.wave && *k == note.key);
        if fresh {
            ch.inner = Some((Voice::new(usize::from(note.wave), 0, 0, false), note.wave, note.key));
        }
        let (iv, _, _) = ch.inner.as_mut().unwrap();
        let (ii, it, is) = iv.step(ram, false)?;
        let inote = note_at(ram, iv.sfx, ii);
        let (ihz, ivol) = effect(ram, iv, &is, &inote, ii, it, hz / key_hz(24.0));
        Some(Synth { hz: ihz, vol: ivol * vol, wave: inote.wave, filters: is.filters, key: inote.key })
    }

    /// The next sample of a channel, as a 16-bit level.
    fn sample(&mut self, ram: &[u8; 0x10000], c: usize, music_volume: f32, coeffs: &[Coeffs; 2]) -> f32 {
        let ch = &mut self.channels[c];
        if ch.voice.is_none() && ch.fade <= 0.0 && ch.quiet > REVERB as u32 {
            return 0.0;
        }
        let new = self.synth(ram, c);
        let (reverb_reg, distort, lowpass) = (ram[0x5f41], ram[0x5f42], ram[0x5f43]);
        let ch = &mut self.channels[c];
        let gain = if ch.music { music_volume } else { 1.0 };
        let mut value = 0.0;
        let current = new.unwrap_or(Synth { vol: 0.0, ..ch.last });
        // A sharp change of note fades from the one before in 1/130 s.
        let last = ch.last;
        if (current.vol - last.vol).abs() > 0.1
            || (current.hz - last.hz).abs() > current.hz.min(last.hz) * 0.01
            || current.wave != last.wave
        {
            if ch.fade <= 0.0 {
                ch.fade_synth = last;
                ch.fade_osc = ch.osc;
            }
            ch.fade = 1.0;
        }
        ch.last = current;
        if current.vol > 0.0 {
            ch.osc.advance(current.hz);
            value = synth_sample(&Synth { vol: current.vol * gain, ..current }, &mut ch.osc);
        }
        let (mut reverb, mut damp) = ((current.filters / 24) % 3, (current.filters / 72) % 3);
        if ch.fade > 0.0 {
            if ch.fade_synth.vol > 0.0 {
                ch.fade_osc.advance(ch.fade_synth.hz);
                let faded = synth_sample(&Synth { vol: ch.fade_synth.vol * gain, ..ch.fade_synth }, &mut ch.fade_osc);
                value += (faded - value) * ch.fade;
            } else {
                value *= 1.0 - ch.fade;
            }
            if ch.fade > 0.5 {
                (reverb, damp) = ((ch.fade_synth.filters / 24) % 3, (ch.fade_synth.filters / 72) % 3);
            }
            ch.fade -= 130.0 / RATE;
        }
        if reverb_reg & (1 << (c + 4)) != 0 {
            reverb = 1;
        }
        if reverb_reg & (1 << c) != 0 {
            reverb = 2;
        }
        if lowpass & (1 << (c + 4)) != 0 {
            damp = 1;
        }
        if lowpass & (1 << c) != 0 {
            damp = 2;
        }
        // Reverb: the sound 366 or 732 samples ago, at half.
        match reverb {
            1 => value += ch.reverb[(ch.at + REVERB - 366) % REVERB] * 0.5,
            2 => value += ch.reverb[ch.at] * 0.5,
            _ => {}
        }
        ch.reverb[ch.at] = value;
        ch.at = (ch.at + 1) % REVERB;
        if damp == 1 {
            value = ch.damp[0].run(&coeffs[0], value);
        } else if damp == 2 {
            value = ch.damp[1].run(&coeffs[1], value);
        }
        ch.quiet = if value == 0.0 { ch.quiet.saturating_add(1) } else { 0 };
        let mut level = (32767.99 * value.clamp(-0.99, 0.99)) as i32;
        if distort & (1 << c) != 0 {
            level = level / 0x1000 * 0x1249;
        } else if distort & (1 << (c + 4)) != 0 {
            level = (level - if level < 0 { 0x1000 } else { 0 }) / 0x1000 * 0x1249;
        }
        level as f32
    }

    pub fn queue_pcm(&mut self, bytes: &[u8]) {
        if self.pcm.len() < 1 << 16 {
            self.pcm.extend(bytes);
        }
    }
    pub fn pcm_queued(&self) -> usize {
        self.pcm.len()
    }

    /// stat(16..19): the effect on a channel, or -1.
    pub fn channel_sfx(&self, c: usize) -> i32 {
        self.channels[c].sfx
    }
    pub fn channel_note(&self, c: usize) -> i32 {
        self.channels[c].note
    }
    pub fn pattern(&self) -> i32 {
        self.music.as_ref().map_or(-1, |m| m.pattern as i32)
    }
    pub fn patterns_played(&self) -> i32 {
        self.music.as_ref().map_or(0, |m| m.count)
    }
    pub fn music_ticks(&self) -> i32 {
        self.music.as_ref().map_or(0, |m| (m.pos / TICK) as i32)
    }
}

// Save states.

use crate::state::{R, Res, W};

fn save_voice(w: &mut W, v: &Voice) {
    for n in [v.sfx as u32, v.pos, v.time, v.start as u32, v.length, v.last_note as u32] {
        w.u32(n);
    }
    w.bool(v.can_loop);
    w.bool(v.music);
    w.f32(v.prev_key);
    w.f32(v.prev_vol);
    w.u32(v.sounding as u32);
}

fn load_voice(r: &mut R) -> Res<Voice> {
    let sfx = (r.u32()? & 63) as usize;
    let (pos, time) = (r.u32()?, r.u32()?);
    let start = (r.u32()? as usize).min(31);
    let length = r.u32()?.min(32);
    let last_note = (r.u32()? as usize).min(31);
    let (can_loop, music) = (r.bool()?, r.bool()?);
    let (prev_key, prev_vol) = (r.f32()?, r.f32()?);
    let sounding = (r.u32()? as usize).min(32);
    Ok(Voice { sfx, pos, time, start, length, can_loop, music, prev_key, prev_vol, last_note, sounding })
}

fn save_synth(w: &mut W, s: &Synth) {
    w.f32(s.hz);
    w.f32(s.vol);
    w.u8(s.wave);
    w.u8(s.filters);
    w.f32(s.key);
}

fn load_synth(r: &mut R) -> Res<Synth> {
    let finite = |x: f32| if x.is_finite() { x } else { 0.0 };
    Ok(Synth { hz: finite(r.f32()?), vol: finite(r.f32()?), wave: r.u8()? & 7, filters: r.u8()?, key: finite(r.f32()?) })
}

fn save_osc(w: &mut W, o: &Osc) {
    w.f32(o.phase);
    w.f32(o.cycles);
    w.f32(o.noise);
    w.u32(o.seed);
}

fn load_osc(r: &mut R) -> Res<Osc> {
    let phase = r.f32()?;
    let phase = if (0.0..1.0).contains(&phase) { phase } else { 0.0 };
    let cycles = r.f32()?;
    let cycles = if (0.0..110.0).contains(&cycles) { cycles.floor() } else { 0.0 };
    let noise = r.f32()?;
    let noise = if noise.is_finite() { noise.clamp(-2.0, 2.0) } else { 0.0 };
    Ok(Osc { phase, cycles, noise, seed: r.u32()?.max(1) })
}

impl Audio {
    pub fn save(&self, w: &mut W) {
        let pcm: Vec<u8> = self.pcm.iter().copied().collect();
        w.bytes(&pcm);
        w.u8(self.pcm_step);
        for c in &self.channels {
            w.bool(c.voice.is_some());
            if let Some(v) = &c.voice {
                save_voice(w, v);
            }
            w.bool(c.inner.is_some());
            if let Some((v, wave, key)) = &c.inner {
                save_voice(w, v);
                w.u8(*wave);
                w.f32(*key);
            }
            w.bool(c.music);
            w.i32(c.note);
            w.i32(c.sfx);
            save_osc(w, &c.osc);
            save_synth(w, &c.last);
            save_synth(w, &c.fade_synth);
            save_osc(w, &c.fade_osc);
            w.f32(c.fade);
            for &x in c.reverb.iter() {
                w.f32(x);
            }
            w.u32(c.at as u32);
            for s in &c.damp {
                for x in s.x.iter().chain(&s.y) {
                    w.f32(*x);
                }
            }
            w.u32(c.quiet);
        }
        w.bool(self.music.is_some());
        if let Some(m) = &self.music {
            w.u32(m.pattern as u32);
            w.i32(m.count);
            w.u32(m.pos);
            w.u32(m.length);
            w.u8(m.mask);
            w.f32(m.volume);
            w.f32(m.fade);
            w.bool(m.stop_at_zero);
        }
        w.u32(self.owed);
    }

    pub fn load(r: &mut R) -> Res<Audio> {
        let mut a = Audio::new();
        a.pcm = r.bytes()?.iter().copied().collect();
        a.pcm_step = r.u8()? & 3;
        let level = |x: f32| if x.is_finite() { x.clamp(-4.0, 4.0) } else { 0.0 };
        for c in &mut a.channels {
            c.voice = if r.bool()? { Some(load_voice(r)?) } else { None };
            c.inner = if r.bool()? { Some((load_voice(r)?, r.u8()? & 7, r.f32()?)) } else { None };
            c.music = r.bool()?;
            c.note = r.i32()?;
            c.sfx = r.i32()?;
            c.osc = load_osc(r)?;
            c.last = load_synth(r)?;
            c.fade_synth = load_synth(r)?;
            c.fade_osc = load_osc(r)?;
            c.fade = r.f32()?.clamp(0.0, 1.0);
            for x in c.reverb.iter_mut() {
                *x = level(r.f32()?);
            }
            c.at = r.u32()? as usize % REVERB;
            for s in &mut c.damp {
                for x in s.x.iter_mut().chain(s.y.iter_mut()) {
                    *x = level(r.f32()?);
                }
            }
            c.quiet = r.u32()?;
        }
        a.music = if r.bool()? {
            Some(Music {
                pattern: (r.u32()? & 63) as usize,
                count: r.i32()?,
                pos: r.u32()?,
                length: r.u32()?.max(1),
                mask: r.u8()?,
                volume: r.f32()?.clamp(0.0, 1.0),
                fade: r.f32()?,
                stop_at_zero: r.bool()?,
            })
        } else {
            None
        };
        a.owed = r.u32()? % 60;
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ram_with(notes: &[(u8, u8, u8, u8)], speed: u8) -> Box<[u8; 0x10000]> {
        let mut ram: Box<[u8; 0x10000]> = vec![0u8; 0x10000].into_boxed_slice().try_into().unwrap();
        for (i, &(key, wave, vol, fx)) in notes.iter().enumerate() {
            let w = u16::from(key) | u16::from(wave) << 6 | u16::from(vol) << 9 | u16::from(fx) << 12;
            ram[0x3200 + i * 2..0x3202 + i * 2].copy_from_slice(&w.to_le_bytes());
        }
        ram[0x3200 + 65] = speed;
        ram
    }

    fn play(a: &mut Audio, ram: &[u8; 0x10000], frames: usize) -> Vec<i16> {
        let mut all = Vec::new();
        let mut out = Vec::new();
        for _ in 0..frames {
            a.render(ram, 30, &mut out);
            all.extend_from_slice(&out);
        }
        all
    }

    #[test]
    fn a_frame_is_its_share_of_a_second() {
        let mut a = Audio::new();
        let ram = ram_with(&[], 1);
        let mut out = Vec::new();
        let n: usize = (0..60)
            .map(|_| {
                a.render(&ram, 60, &mut out);
                out.len()
            })
            .sum();
        assert_eq!(n, SAMPLE_HZ);
    }

    #[test]
    fn a_square_plays_its_pitch_then_stops() {
        let ram = ram_with(&[(33, 3, 7, 0), (33, 3, 7, 0)], 16);
        let mut a = Audio::new();
        a.sfx(0, -1, 0, 0);
        let s = play(&mut a, &ram, 10);
        // Two notes of 16 * 183 samples at A4 (440 Hz): about 116 periods.
        let crossings = s[200..5800].windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
        assert!((108..=114).contains(&crossings), "{crossings}");
        assert!(s[..5856].iter().any(|&x| x > 7000));
        assert!(s[7000..].iter().all(|&x| x.abs() < 50));
        assert_eq!(a.channel_sfx(0), -1);
    }

    #[test]
    fn loops_play_until_released() {
        let mut ram = ram_with(&[(30, 0, 5, 0), (32, 0, 5, 0)], 4);
        ram[0x3200 + 66] = 0;
        ram[0x3200 + 67] = 2;
        let mut a = Audio::new();
        a.sfx(0, 2, 0, 0);
        play(&mut a, &ram, 60);
        assert_eq!(a.channel_sfx(2), 0);
        a.sfx(-2, 2, 0, 0);
        // Released, it plays on to its 32nd note (silent ones too).
        play(&mut a, &ram, 5);
        assert_eq!(a.channel_sfx(2), 0);
        play(&mut a, &ram, 30);
        assert_eq!(a.channel_sfx(2), -1);
    }

    #[test]
    fn every_wave_effect_and_filter_sounds() {
        for wave in 0..8u8 {
            for fx in 0..8u8 {
                let notes: Vec<_> = (0..8).map(|i| (20 + i * 4, wave, 7, fx)).collect();
                let mut ram = ram_with(&notes, 2);
                ram[0x3200 + 64] = 0b1110_1110;
                let mut a = Audio::new();
                a.sfx(0, 0, 0, 0);
                let s = play(&mut a, &ram, 3);
                assert!(s.iter().any(|&x| x != 0), "wave {wave} fx {fx}");
            }
        }
    }

    #[test]
    fn music_moves_through_its_patterns() {
        let mut ram = ram_with(&[(30, 1, 5, 0)], 1);
        // Pattern 0 and 1 play effect 0 on channel 0; 1 stops.
        ram[0x3100] = 0;
        ram[0x3101..0x3104].fill(0x40);
        ram[0x3104] = 0;
        ram[0x3105] = 0x40;
        ram[0x3106] = 0xc0;
        ram[0x3107] = 0x40;
        let mut a = Audio::new();
        a.music(&ram, 0, 0, 0);
        assert_eq!(a.pattern(), 0);
        // 32 notes of 183 samples: eight frames.
        play(&mut a, &ram, 9);
        assert_eq!(a.pattern(), 1);
        play(&mut a, &ram, 10);
        assert_eq!(a.pattern(), -1);
    }
}
