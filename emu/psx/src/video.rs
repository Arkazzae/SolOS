//! The GPU as the rest of the machine sees it. Drawing is over half of a
//! frame's work, so it is done on a thread of its own: commands are
//! followed here for what the CPU reads back (GPUSTAT, GPUREAD's latch),
//! and sent on in batches to a copy of the GPU that draws them in the same
//! order. VRAM is only read for the machine (sent to the CPU, or saved)
//! once that copy has drawn everything sent and come back, so the machine
//! runs exactly as it does on one thread.
//!
//! Pictures are taken on the thread, as soon as none of the batches sent
//! before and not drawn yet draws in the display area (the VRAM each batch
//! draws in is noted as it is gathered). One is asked for when the display
//! is set up anew (a game showing the buffer it has just drawn) and at a
//! vertical blank, unless nothing has been drawn in the area since the last
//! one was asked for, which is then still exactly the one the blank shows.
//! Games draw the next frame in another buffer, so the CPU waits neither
//! for that drawing nor for the picture. Pictures are only taken so while
//! they are looked at: else `picture` waits for the thread, which is as
//! exact right after a frame. The thread is kept at most two frames behind.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};

use savestate::{BadState, State};

use crate::gpu::{Display, Gpu, add_rect};

/// The words gathered before they are sent on.
const BATCH: usize = 4096;

const STOPPED: &str = "the GPU thread stopped";

/// A picture's width, height, and pixels as RGB888.
type Picture = (usize, usize, Vec<u8>);

pub struct Video {
    /// Every command followed, for what the CPU reads; drawn too when
    /// there is no thread.
    front: Gpu,
    thread: Option<Thread>,
    /// The picture last taken, which `picture` lends.
    shown: Picture,
}

/// The thread that draws, and what is gathered for it.
struct Thread {
    jobs: Sender<Job>,
    back: Receiver<Box<Gpu>>,
    shared: Arc<Shared>,
    /// The copy that draws, while it is here rather than on the thread.
    drawn: Option<Box<Gpu>>,
    /// GP0 words not sent yet, and the GP1 writes among them (each before
    /// the word at its index).
    words: Vec<u32>,
    gp1: Vec<(usize, u32)>,
    /// Batches sent; the VRAM those not known to be drawn draw in.
    sent: u64,
    touching: VecDeque<(u64, [u32; 4])>,
    /// The batches sent before each of the last two vertical blanks.
    sent_at_blanks: [u64; 2],
    /// Pictures asked for (or taken here) so far; the one shown, and the
    /// one the last blank shows (0: none asked for).
    serial: u64,
    shown_serial: u64,
    blank_serial: u64,
    /// Whether a picture was looked at since the last blank, and before
    /// it.
    wanted: bool,
    looking: bool,
    /// What the last picture asked for is of, and the VRAM drawn in since.
    last_display: Option<Display>,
    touched_since: Vec<[u32; 4]>,
}

/// What the thread and the front share.
struct Shared {
    /// Batches drawn; the batches the front waits for (none: u64::MAX),
    /// told when they are.
    done: AtomicU64,
    waiting: AtomicU64,
    lock: Mutex<()>,
    caught_up: Condvar,
    /// The picture asked for, and the batches it waits for (none:
    /// u64::MAX), both changed under the lock.
    capture: Mutex<Option<Capture>>,
    after: AtomicU64,
    /// The picture last taken, with its serial.
    taken: Mutex<Option<(u64, Picture)>>,
    ready: Condvar,
}

/// A picture of the display as GP1 had it set up when it was asked for,
/// taken once the batches up to `after` are drawn.
struct Capture {
    serial: u64,
    after: u64,
    display: Display,
}

enum Job {
    /// Commands to draw, with the GPU when it was here.
    Draw(Option<Box<Gpu>>, Vec<u32>, Vec<(usize, u32)>),
    /// A picture asked for.
    Wake,
    /// The GPU wanted back, once it has drawn what came before.
    Return,
}

impl Video {
    pub fn new() -> Video {
        Video::start(Gpu::new())
    }

    /// The GPU with a thread drawing for it, or without one if no thread
    /// starts.
    fn start(gpu: Gpu) -> Video {
        let (jobs, inbox) = channel();
        let (outbox, back) = channel();
        let shared = Arc::new(Shared {
            done: AtomicU64::new(0),
            waiting: AtomicU64::new(u64::MAX),
            lock: Mutex::new(()),
            caught_up: Condvar::new(),
            capture: Mutex::new(None),
            after: AtomicU64::new(u64::MAX),
            taken: Mutex::new(None),
            ready: Condvar::new(),
        });
        let (theirs, converter) = (shared.clone(), shared.clone());
        let shown = (0, 0, Vec::new());
        // Pictures are converted on a thread of their own, off the one that
        // draws.
        let (to_convert, unconverted) = channel();
        let spawn = |name: &str| std::thread::Builder::new().name(name.into());
        let started = spawn("psx-picture")
            .spawn(move || convert(unconverted, &converter))
            .and_then(|_| spawn("psx-gpu").spawn(move || draw(inbox, outbox, to_convert, &theirs)));
        match started {
            Ok(_) => {
                let mut front = gpu.clone();
                front.draws = false;
                let thread = Thread {
                    jobs,
                    back,
                    shared,
                    drawn: Some(Box::new(gpu)),
                    words: Vec::with_capacity(BATCH),
                    gp1: Vec::new(),
                    sent: 0,
                    touching: VecDeque::new(),
                    sent_at_blanks: [0; 2],
                    serial: 0,
                    shown_serial: 0,
                    blank_serial: 0,
                    wanted: false,
                    looking: false,
                    last_display: None,
                    touched_since: Vec::new(),
                };
                Video { front, thread: Some(thread), shown }
            }
            Err(_) => Video { front: gpu, thread: None, shown },
        }
    }

    /// Draws on a thread of its own, or not.
    pub fn set_thread(&mut self, on: bool) {
        if on == self.thread.is_some() {
            return;
        }
        if on {
            *self = Video::start(self.front.clone());
        } else {
            let front = self.sync().clone();
            *self = Video { front, thread: None, shown: (0, 0, Vec::new()) };
        }
    }

    /// The GPU that draws, once it has drawn everything sent: the front
    /// itself without a thread.
    pub fn sync(&mut self) -> &mut Gpu {
        let Some(t) = &mut self.thread else {
            return &mut self.front;
        };
        t.flush(&mut self.front);
        if t.drawn.is_none() {
            t.jobs.send(Job::Return).expect(STOPPED);
            t.drawn = Some(t.back.recv().expect(STOPPED));
            t.touching.clear();
        }
        let drawn = t.drawn.as_mut().unwrap();
        // Only the front sees the lines go by.
        drawn.odd = self.front.odd;
        drawn
    }

    /// Sends on what is gathered.
    pub fn flush(&mut self) {
        if let Some(t) = &mut self.thread {
            t.flush(&mut self.front);
        }
    }

    pub fn gp0(&mut self, word: u32) {
        self.front.gp0(word);
        if let Some(t) = &mut self.thread {
            t.words.push(word);
            if t.words.len() >= BATCH {
                t.flush(&mut self.front);
            }
        }
    }

    /// GP0 words as RAM has them (little-endian), from a DMA list.
    pub fn gp0_words(&mut self, bytes: &[u8]) {
        let words = bytes.chunks_exact(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        let Some(t) = &mut self.thread else {
            words.for_each(|w| self.front.gp0(w));
            return;
        };
        let start = t.words.len();
        t.words.extend(words);
        self.front.gp0_all(&t.words[start..]);
        if t.words.len() >= BATCH {
            t.flush(&mut self.front);
        }
    }

    pub fn gp1(&mut self, value: u32) {
        self.front.gp1(value);
        let Some(t) = &mut self.thread else {
            return;
        };
        // A display set up anew is likely what the next blank shows: its
        // picture is asked for now, to be ready by then.
        let display = self.front.display();
        if t.looking && t.last_display != Some(display) {
            t.flush(&mut self.front);
            let after = t.last_touching(display.area());
            t.ask(display, after, &mut self.shown);
        }
        t.gp1.push((t.words.len(), value));
    }

    /// GPUREAD: VRAM comes from the copy that draws, the transfer's place
    /// kept in both.
    pub fn read(&mut self) -> u32 {
        if self.thread.is_none() || !self.front.storing() {
            return self.front.read();
        }
        let word = self.sync().read();
        self.front.read();
        word
    }

    pub fn status(&self, line: u32) -> u32 {
        self.front.status(line)
    }

    pub fn next_line(&mut self, line: u32) {
        self.front.next_line(line);
        // Words the CPU writes itself go on every few lines.
        if line.is_multiple_of(8) {
            self.flush();
        }
    }

    /// The vertical blank starts: the picture it shows is asked for,
    /// unless it is the one asked for last.
    pub fn blank(&mut self) {
        let Some(t) = &mut self.thread else {
            return;
        };
        t.flush(&mut self.front);
        let [two_ago, one_ago] = t.sent_at_blanks;
        t.sent_at_blanks = [one_ago, t.sent];
        t.wait_for(two_ago);
        t.looking = std::mem::take(&mut t.wanted);
        if !t.looking {
            t.blank_serial = 0;
            return;
        }
        let display = self.front.display();
        if t.last_display == Some(display) && !t.touched_since.iter().any(|r| meets(display.area(), r)) {
            t.blank_serial = t.serial;
            return;
        }
        let after = t.last_touching(display.area()).max(one_ago);
        t.blank_serial = t.ask(display, after, &mut self.shown);
    }

    pub fn vram(&mut self) -> &[u16] {
        self.sync().vram()
    }

    /// The picture of the last vertical blank when the thread took it,
    /// else of VRAM as it is (the same right after a frame); lent until the
    /// next call.
    pub fn picture(&mut self) -> (usize, usize, &[u8]) {
        let asked = self.thread.as_mut().is_some_and(|t| {
            t.wanted = true;
            t.blank_serial != 0
        });
        if !asked {
            self.shown = self.sync().picture();
            let display = self.front.display();
            if let Some(t) = &mut self.thread {
                t.serial += 1;
                (t.shown_serial, t.blank_serial) = (t.serial, t.serial);
                t.last_display = Some(display);
                t.touched_since.clear();
            }
        } else if let Some(t) = self.thread.as_mut().filter(|t| t.shown_serial != t.blank_serial) {
            let mut slot = t.shared.taken.lock().unwrap();
            // One asked for since and not wanted any more is passed over.
            while slot.as_ref().is_none_or(|&(serial, _)| serial != t.blank_serial) {
                let (s, waited) = t.shared.ready.wait_timeout(slot, std::time::Duration::from_millis(100)).unwrap();
                slot = s;
                // A thread that has stopped takes nothing: asking again finds it out.
                if waited.timed_out() {
                    t.jobs.send(Job::Wake).expect(STOPPED);
                }
            }
            self.shown = slot.take().unwrap().1;
            t.shown_serial = t.blank_serial;
        }
        let (width, height, pixels) = &self.shown;
        (*width, *height, pixels)
    }
}

/// Whether a rectangle of VRAM meets an area (none: the display is off).
fn meets(area: Option<[u32; 4]>, r: &[u32; 4]) -> bool {
    area.is_some_and(|a| a[0] <= r[2] && r[0] <= a[2] && a[1] <= r[3] && r[1] <= a[3])
}

impl Thread {
    /// The last batch, not known to be drawn, that draws in an area (0:
    /// none).
    fn last_touching(&mut self, area: Option<[u32; 4]>) -> u64 {
        let done = self.shared.done.load(Ordering::Acquire);
        while self.touching.front().is_some_and(|&(batch, _)| batch <= done) {
            self.touching.pop_front();
        }
        self.touching.iter().rev().find(|(_, r)| meets(area, r)).map_or(0, |&(batch, _)| batch)
    }

    /// Asks for a picture of the display as set up now, once the batches
    /// up to `after` are drawn (taken here at once, if all of them are);
    /// its serial.
    fn ask(&mut self, display: Display, after: u64, shown: &mut Picture) -> u64 {
        self.serial += 1;
        self.last_display = Some(display);
        self.touched_since.clear();
        if let Some(drawn) = &self.drawn {
            *shown = display.picture(drawn.vram());
            self.shown_serial = self.serial;
        } else {
            let mut capture = self.shared.capture.lock().unwrap();
            *capture = Some(Capture { serial: self.serial, after, display });
            self.shared.after.store(after, Ordering::Release);
            drop(capture);
            self.jobs.send(Job::Wake).expect(STOPPED);
        }
        self.serial
    }

    /// Waits for the thread to have drawn so many batches.
    fn wait_for(&self, batches: u64) {
        let s = &self.shared;
        if s.done.load(Ordering::Acquire) >= batches {
            return;
        }
        s.waiting.store(batches, Ordering::SeqCst);
        let mut lock = s.lock.lock().unwrap();
        while s.done.load(Ordering::SeqCst) < batches {
            let (l, waited) = s.caught_up.wait_timeout(lock, std::time::Duration::from_millis(100)).unwrap();
            lock = l;
            // A thread that has stopped draws nothing: asking finds it out.
            if waited.timed_out() {
                self.jobs.send(Job::Wake).expect(STOPPED);
            }
        }
        s.waiting.store(u64::MAX, Ordering::SeqCst);
    }

    fn flush(&mut self, front: &mut Gpu) {
        if self.words.is_empty() && self.gp1.is_empty() {
            return;
        }
        self.sent += 1;
        for r in front.touched.drain(..) {
            self.touching.push_back((self.sent, r));
            add_rect(&mut self.touched_since, r);
        }
        let words = std::mem::replace(&mut self.words, Vec::with_capacity(BATCH));
        let job = Job::Draw(self.drawn.take(), words, std::mem::take(&mut self.gp1));
        self.jobs.send(job).expect(STOPPED);
    }
}

/// The thread: draws what comes, keeping the GPU between batches, takes
/// the pictures asked for, and hands the GPU back when asked. It ends when
/// the front goes.
fn draw(jobs: Receiver<Job>, back: Sender<Box<Gpu>>, pictures: Sender<Unconverted>, shared: &Shared) {
    let mut gpu = None;
    let mut done = 0;
    while let Ok(job) = jobs.recv() {
        match job {
            Job::Draw(given, words, gp1) => {
                if given.is_some() {
                    gpu = given;
                }
                let g: &mut Gpu = gpu.as_mut().expect("drawing without the GPU");
                // The picture asked for is taken once it can be, before the
                // words of the next batch: a batch drawn was sent before it
                // was asked for, so taking it there or later in the batch
                // comes to the same.
                let run = |g: &mut Gpu, words: &[u32]| {
                    if shared.after.load(Ordering::Relaxed) <= done {
                        capture(shared, g, done, &pictures);
                    }
                    g.gp0_all(words);
                };
                let mut at = 0;
                for (i, value) in gp1 {
                    run(g, &words[at..i]);
                    g.gp1(value);
                    at = i;
                }
                run(g, &words[at..]);
                done += 1;
                shared.done.store(done, Ordering::SeqCst);
                if shared.waiting.load(Ordering::SeqCst) <= done {
                    let _lock = shared.lock.lock().unwrap();
                    shared.caught_up.notify_one();
                }
                capture(shared, g, done, &pictures);
            }
            Job::Wake => {
                if let Some(g) = &gpu {
                    capture(shared, g, done, &pictures);
                }
            }
            Job::Return => {
                let g = gpu.take().expect("the GPU wanted back twice");
                capture(shared, &g, done, &pictures);
                if back.send(g).is_err() {
                    return;
                }
            }
        }
    }
}

/// A picture taken but not converted: its serial, what it shows, and the
/// display area's halfwords.
type Unconverted = (u64, Display, Vec<u16>);

/// Takes the picture asked for, if the batches it waits for are drawn: the
/// display area's rows, sent to be converted.
fn capture(shared: &Shared, gpu: &Gpu, done: u64, pictures: &Sender<Unconverted>) {
    let mut slot = shared.capture.lock().unwrap();
    if slot.as_ref().is_none_or(|c| c.after > done) {
        return;
    }
    let c = slot.take().unwrap();
    shared.after.store(u64::MAX, Ordering::Relaxed);
    drop(slot);
    pictures.send((c.serial, c.display, c.display.rows(gpu.vram()))).expect("the picture thread stopped");
}

/// The thread converting pictures taken, one after another, into the slot
/// `picture` waits on. It ends when the thread drawing goes.
fn convert(pictures: Receiver<Unconverted>, shared: &Shared) {
    while let Ok((serial, display, rows)) = pictures.recv() {
        let picture = display.picture_of(&rows);
        *shared.taken.lock().unwrap() = Some((serial, picture));
        shared.ready.notify_one();
    }
}

/// A copy of the GPU once synced (as `Psx::load_state` does), with a
/// thread of its own.
impl Clone for Video {
    fn clone(&self) -> Video {
        match &self.thread {
            None => Video { front: self.front.clone(), thread: None, shown: (0, 0, Vec::new()) },
            Some(t) => Video::start((**t.drawn.as_ref().expect("the GPU copied while it draws")).clone()),
        }
    }
}

/// Saved once synced (as `Psx::save_state` does): the copy that draws then
/// has all of the state.
impl State for Video {
    fn save(&self, out: &mut Vec<u8>) {
        match &self.thread {
            None => self.front.save(out),
            Some(t) => t.drawn.as_ref().expect("the GPU saved while it draws").save(out),
        }
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        let mut again = *input;
        self.front.load(input)?;
        if self.thread.is_some() {
            self.sync().load(&mut again)?;
        }
        Ok(())
    }
}
