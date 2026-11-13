//! The RDP drawing on a thread of its own, as the PlayStation's GPU does
//! (emu/psx/src/video.rs). The machine's thread keeps the RDP's command
//! interface, its whole state (what save states hold) and the texture
//! loads; it sends each command that sets state, TMEM after loads and each
//! draw, in order, to a copy of the RDP's state on the drawing thread,
//! which applies them the same way and draws into the machine's RDRAM.
//! Only drawing touches RDRAM there, in the color and depth images.
//!
//! Each draw marks the 4 KB pages of RDRAM it may reach: the rows of the
//! color image within the scissor box, and the same rows of the depth image
//! where the depth is compared or updated. Draws and pictures are numbered,
//! a page keeps the number of the last that may reach it, and each drawing
//! thread tells which it has finished. The machine waits for that one (not
//! for the rest of its batch) before it touches a marked page itself
//! (N64::touch_rdram: the CPU's reads and writes, DMA, the audio and video
//! interfaces, the RDP's command fetch and loads), and for everything
//! (`sync`) before a save state or a copy.
//!
//! The video interface's picture of each field is taken by the drawing
//! thread too, after everything sent before the field's end (the lines it
//! reads are marked, so nothing changes them first). The machine takes the
//! latest one done when its frame ends, so the pictures can be a field or
//! two behind the machine, unless it asks to wait for each (`exact`).
//!
//! There can be several drawing threads (`drawers`: all the cores but the
//! machine's, at most three): each gets everything and keeps its own copy
//! of the state, but draws only every n-th line, so each pixel of an image
//! has one thread, which draws it in the RDP's order. When the color or
//! depth image changes, the same RDRAM can belong to other lines, so the
//! threads meet first (each waits until all have drawn everything before);
//! they meet around a picture too, which the first takes.

use crate::raster::{Lines, Raster};
use crate::rdp::Rdp;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Barrier, Condvar, Mutex};

/// What the drawing threads are sent, in the RDP's order.
enum Item {
    /// The video interface's registers at a field's end, and the picture's
    /// number (`RdpThread::reaching`).
    Picture([u32; 16], u32),
    State(u64),
    /// TMEM's bytes from `at` that loads changed, from the batch's `tmem`.
    Tmem { at: u16, from: u32, len: u16 },
    /// A draw, and its number (`RdpThread::reaching`).
    Draw { words: [u64; 22], len: usize, cycles: u64, number: u32 },
}

/// Items, and the TMEM bytes they bring.
#[derive(Default)]
struct Batch {
    items: Vec<Item>,
    tmem: Vec<u8>,
}

/// Batches sent before the machine waits for the drawing to catch up, and
/// draws gathered before a batch is sent (command lists are short: the RSP
/// hands the RDP a few commands at a time).
const BATCHES: usize = 256;
const DRAWS: u32 = 32;

/// Drawing threads, unless the machine asks for another number: all the
/// cores but the machine's, at most three (three on the Pi Zero 2 W).
pub(crate) fn drawers() -> usize {
    std::thread::available_parallelism().map_or(2, |cores| cores.get().saturating_sub(1)).clamp(1, 3)
}

/// RDRAM, for the drawing thread: the machine's own buffer, which neither
/// moves nor is freed while the thread runs (the thread is joined first).
struct Rdram(*mut u8, usize);

// The pointer is only used by the drawing thread, on pages the machine
// waits for before it touches them.
unsafe impl Send for Rdram {}

struct Shared {
    /// Each drawing thread's copy of the RDP's state.
    rdps: Vec<Mutex<Rdp>>,
    /// Batches each thread has drawn, and those all have: to wait on.
    done: Mutex<Progress>,
    drawn: Condvar,
    /// The number of the last draw or picture each thread has finished, and
    /// the one the machine waits for (0 for none), which the threads wake it
    /// for (through `drawn`).
    reached: Vec<AtomicU32>,
    waiting: AtomicU32,
    /// Where the threads meet.
    meet: Barrier,
    /// The latest picture taken, and how many have been.
    picture: Mutex<(u64, Vec<u32>, usize, usize)>,
    /// The picture being taken: every thread converts its own lines.
    canvas: Canvas,
}

impl Shared {
    /// The last draw or picture all threads have finished.
    fn reached(&self, order: Ordering) -> u32 {
        self.reached.iter().map(|r| r.load(order)).min().unwrap_or(0)
    }

    /// Thread n has finished this draw or picture: wakes the machine if it
    /// waits for one up to it. The machine says it waits before it looks
    /// at `reached`, and each thread updates `reached` before it looks at
    /// `waiting` (all in one order), so either the machine sees this or the
    /// thread sees it waiting, and wakes it once it is (it holds `done`
    /// until then).
    fn reach(&self, n: usize, number: u32) {
        self.reached[n].store(number, Ordering::SeqCst);
        let waiting = self.waiting.load(Ordering::SeqCst);
        if waiting != 0 && number >= waiting {
            drop(self.done.lock().unwrap());
            self.drawn.notify_all();
        }
    }
}

/// The picture the drawing threads convert together, each its own lines
/// between two meetings, through the pointer to its pixels; only the first
/// sizes it (before the first) and hands it on (after the second).
struct Canvas(std::cell::UnsafeCell<Vec<u32>>, std::sync::atomic::AtomicPtr<u32>);

// Safety: see Canvas; no two threads touch the same element, nor the vector
// itself while another converts.
unsafe impl Sync for Canvas {}

struct Progress {
    each: Vec<u64>,
    all: u64,
}

struct Worker {
    senders: Vec<SyncSender<Arc<Batch>>>,
    shared: Arc<Shared>,
    threads: Vec<std::thread::JoinHandle<()>>,
    sent: Cell<u64>,
}

pub(crate) struct RdpThread {
    pub enabled: bool,
    /// Waits for the drawing after every command list, as when native CPU
    /// code reads RDRAM without looking at the pages.
    pub serial: bool,
    /// Debugging: drawing on the machine's thread checks that each draw
    /// changes only RDRAM pages it marks.
    pub check_marks: bool,
    /// Drawing threads to start.
    pub drawers: usize,
    worker: Option<Worker>,
    /// What is gathered for the next batch, and how many draws: sent when
    /// there are enough, or when anything waits for the drawing.
    batch: RefCell<Batch>,
    draws: Cell<u32>,
    /// The bytes of TMEM loads changed since the last draw was sent.
    tmem_changed: Option<std::ops::Range<usize>>,
    /// The draws and pictures gathered or sent, numbered from 1 (the next
    /// one gathered is `reaching + 1`), and those sent.
    reaching: u32,
    sent_reaching: Cell<u32>,
    /// For each of RDRAM's MARK-byte pieces, the number of the last draw or
    /// picture that may reach it, 0 for none; whether any is marked; and
    /// up to which all are known to be finished.
    pages: Box<[u32; MARKS]>,
    pub marked: bool,
    /// The pages the CPU's compiled code came from and, more closely, the
    /// CODE_CHUNK-byte pieces of them (both set by its JIT), and how many
    /// times a draw has been marked over one of those pages: the JIT
    /// compares a block's source again after a piece is written or a page
    /// drawn to.
    pub(crate) code: Box<[bool; PAGES]>,
    #[cfg_attr(not(any(target_arch = "arm", target_arch = "aarch64")), allow(dead_code))]
    pub(crate) code_chunks: Box<[bool; CODE_CHUNKS]>,
    pub(crate) code_drawn: u32,
    known_done: u32,
    /// Waits for each field's picture (for comparisons).
    pub exact: bool,
    /// Pictures taken from the thread.
    taken: u64,
    /// Times the machine waited for the drawing, batches sent, and sends
    /// that waited for room.
    pub waits: u64,
    pub batches: Cell<u64>,
    pub full: Cell<u64>,
}

/// RDRAM's 4 KB pages, as `code` counts them.
pub(crate) const PAGES: usize = crate::bus::RDRAM_SIZE >> 12;

/// The pieces of RDRAM draws are marked over, smaller than pages: a game may
/// keep a buffer it draws to in the page of code the CPU runs at every
/// interrupt (Paper Mario's depth buffer starts at 0x400, past the exception
/// vectors). log2 of their bytes, and how many there are.
pub(crate) const MARK: u32 = 10;
pub(crate) const MARKS: usize = crate::bus::RDRAM_SIZE >> MARK;

/// The pieces of RDRAM `code_chunks` keeps, smaller than pages: games keep
/// data next to code in a page (Ocarina of Time's overlays do), and a store
/// there is not one into code. log2 of their bytes, and how many there are.
pub(crate) const CODE_CHUNK: u32 = 8;
pub(crate) const CODE_CHUNKS: usize = crate::bus::RDRAM_SIZE >> CODE_CHUNK;

impl RdpThread {
    pub fn new(enabled: bool) -> RdpThread {
        RdpThread {
            enabled,
            serial: false,
            check_marks: false,
            drawers: drawers(),
            worker: None,
            batch: RefCell::new(Batch::default()),
            draws: Cell::new(0),
            tmem_changed: None,
            reaching: 0,
            sent_reaching: Cell::new(0),
            pages: Box::new([0; MARKS]),
            code: Box::new([false; PAGES]),
            code_chunks: Box::new([false; CODE_CHUNKS]),
            code_drawn: 0,
            marked: false,
            known_done: 0,
            exact: false,
            taken: 0,
            waits: 0,
            batches: Cell::new(0),
            full: Cell::new(0),
        }
    }

    /// The last draw or picture that may reach a byte of RDRAM in `len`
    /// from `addr` (RDRAM's addresses wrap at 8 MB, as drawing's do), 0 for
    /// none.
    #[inline]
    fn newest(&self, addr: usize, len: usize) -> u32 {
        let first = (addr & (crate::bus::RDRAM_SIZE - 1)) >> MARK;
        let count = ((addr & ((1 << MARK) - 1)) + len.max(1) - 1) / (1 << MARK) + 1;
        (0..count.min(MARKS)).map(|n| self.pages[(first + n) % MARKS]).max().unwrap_or(0)
    }

    /// For native code: the pieces' numbers (MARKS of them) and up to which
    /// draws and pictures are known to be finished. An access to piece p may
    /// go ahead when pages[p] <= known; otherwise it must take `touch`, which
    /// waits.
    pub fn marks(&self) -> (*const u32, *const u32) {
        (self.pages.as_ptr(), &self.known_done)
    }

    /// Whether a draw not yet drawn may reach these bytes.
    pub fn pending(&self, addr: usize, len: usize) -> bool {
        let newest = self.newest(addr, len);
        newest > self.known_done && self.worker.as_ref().is_none_or(|w| newest > w.shared.reached(Ordering::Acquire))
    }

    /// Before the machine touches these bytes of RDRAM: waits until every
    /// draw that may reach them is drawn.
    #[inline]
    pub fn touch(&mut self, addr: usize, len: usize) {
        if self.marked {
            // Mostly a single piece, as the CPU's accesses are.
            let (first, last) = (addr >> MARK & (MARKS - 1), (addr + len.max(1) - 1) >> MARK & (MARKS - 1));
            let newest = if first == last { self.pages[first] } else { self.newest(addr, len) };
            if newest > self.known_done {
                self.wait_for(newest);
            }
        }
    }

    #[inline(never)]
    fn wait_for(&mut self, number: u32) {
        let Some(worker) = &self.worker else { return };
        let mut reached = worker.shared.reached(Ordering::Acquire);
        if reached < number {
            if number > self.sent_reaching.get() {
                self.flush();
            }
            // The threads wake the machine once they see it waiting (see
            // Shared::reach), so it looks again after it says so.
            let shared = &worker.shared;
            let mut done = shared.done.lock().unwrap();
            shared.waiting.store(number, Ordering::SeqCst);
            loop {
                reached = shared.reached(Ordering::SeqCst);
                if reached >= number {
                    break;
                }
                done = shared.drawn.wait(done).unwrap();
            }
            shared.waiting.store(0, Ordering::Relaxed);
            drop(done);
            self.waits += 1;
        }
        self.known_done = reached;
    }

    /// Marks bytes of RDRAM that the next draw or picture gathered may
    /// reach.
    pub(crate) fn mark(&mut self, addr: usize, len: usize) {
        if len == 0 {
            return;
        }
        let number = self.reaching + 1;
        let first = (addr & (crate::bus::RDRAM_SIZE - 1)) >> MARK;
        let count = ((addr & ((1 << MARK) - 1)) + len - 1) / (1 << MARK) + 1;
        for n in 0..count.min(MARKS) {
            let piece = (first + n) % MARKS;
            self.pages[piece] = number;
            if self.code[piece >> (12 - MARK)] {
                self.code_drawn = self.code_drawn.wrapping_add(1);
            }
        }
        self.marked = true;
    }

    /// Marks what a draw may reach with the RDP's state as it is
    /// (`draw_reach`).
    fn mark_draw(&mut self, rdp: &Rdp, words: &[u64]) {
        for (addr, len) in draw_reach(rdp, words) {
            self.mark(addr, len);
        }
    }

    /// Sends what has gathered and waits for all of it to be drawn; no page
    /// is pending after.
    #[inline(never)]
    pub fn sync(&mut self) {
        self.flush();
        self.drain();
        self.pages.fill(0);
        self.marked = false;
        // Numbering starts again (the threads have finished everything).
        if let Some(worker) = &self.worker {
            worker.shared.reached.iter().for_each(|r| r.store(0, Ordering::Relaxed));
        }
        self.reaching = 0;
        self.sent_reaching.set(0);
        self.known_done = 0;
    }

    /// Whether the drawing thread is drawing (and takes the pictures).
    pub fn drawing(&self) -> bool {
        self.enabled && !self.serial && self.worker.is_some()
    }

    /// The picture of a field that ended, for the drawing thread to take.
    pub fn picture(&mut self, regs: [u32; 16]) {
        self.reaching += 1;
        self.batch.get_mut().items.push(Item::Picture(regs, self.reaching));
        self.flush();
    }

    /// The latest picture the thread has taken since the last, into
    /// `picture` (the buffers are swapped): its width and height.
    pub fn latest_picture(&mut self, picture: &mut Vec<u32>) -> Option<(usize, usize)> {
        let worker = self.worker.as_ref()?;
        if self.exact {
            self.drain();
        }
        let mut latest = worker.shared.picture.lock().unwrap();
        if latest.0 == self.taken {
            return None;
        }
        self.taken = latest.0;
        std::mem::swap(&mut latest.1, picture);
        Some((latest.2, latest.3))
    }

    /// A command that set state, for the drawing thread's copy.
    pub fn state(&mut self, w: u64) {
        if self.worker.is_some() {
            self.batch.get_mut().items.push(Item::State(w));
        }
    }

    /// A load changed these bytes of TMEM.
    pub fn loaded(&mut self, bytes: std::ops::Range<usize>) {
        self.tmem_changed = Some(match self.tmem_changed.take() {
            Some(changed) => changed.start.min(bytes.start)..changed.end.max(bytes.end),
            None => bytes,
        });
    }

    /// A draw for the thread, started with a copy of `rdp` if there is none.
    pub fn draw(&mut self, rdp: &Rdp, rdram: *mut u8, words: &[u64], cycles: u64) {
        if self.worker.is_none() {
            self.worker = Some(Worker::start(rdp, Rdram(rdram, crate::bus::RDRAM_SIZE), self.drawers.max(1)));
            self.tmem_changed = None;
            (self.reaching, self.known_done, self.taken) = (0, 0, 0);
            self.sent_reaching.set(0);
        } else if self.reaching >= 1 << 31 {
            // Long before the numbers wrap.
            self.sync();
        }
        self.mark_draw(rdp, words);
        self.reaching += 1;
        let batch = self.batch.get_mut();
        if let Some(changed) = self.tmem_changed.take() {
            let from = batch.tmem.len() as u32;
            batch.tmem.extend_from_slice(&rdp.tmem[changed.clone()]);
            batch.items.push(Item::Tmem { at: changed.start as u16, from, len: changed.len() as u16 });
        }
        let mut copy = [0; 22];
        copy[..words.len()].copy_from_slice(words);
        batch.items.push(Item::Draw { words: copy, len: words.len(), cycles, number: self.reaching });
        self.draws.set(self.draws.get() + 1);
    }

    /// Sends the batch if it has enough draws for the thread to start on.
    pub fn flush_some(&mut self) {
        if self.draws.get() >= DRAWS {
            self.flush();
        }
    }

    /// Sends what has gathered.
    pub fn flush(&self) {
        let mut batch = self.batch.borrow_mut();
        if batch.items.is_empty() {
            return;
        }
        let Some(worker) = &self.worker else {
            batch.items.clear();
            batch.tmem.clear();
            return;
        };
        // The next one starts as large as this one (growing it costs copies).
        let next = Batch { items: Vec::with_capacity(batch.items.len()), tmem: Vec::with_capacity(batch.tmem.len()) };
        let batch = Arc::new(std::mem::replace(&mut *batch, next));
        self.draws.set(0);
        for sender in &worker.senders {
            if let Err(std::sync::mpsc::TrySendError::Full(batch)) = sender.try_send(batch.clone()) {
                self.full.set(self.full.get() + 1);
                sender.send(batch).expect("the RDP's thread has stopped");
            }
        }
        worker.sent.set(worker.sent.get() + 1);
        self.sent_reaching.set(self.reaching);
        self.batches.set(self.batches.get() + 1);
    }

    /// Sends what has gathered and waits until all of it is drawn.
    pub fn drain(&self) {
        self.flush();
        if let Some(worker) = &self.worker {
            let mut done = worker.shared.done.lock().unwrap();
            while done.all < worker.sent.get() {
                done = worker.shared.drawn.wait(done).unwrap();
            }
        }
    }

    /// Stops the thread once it has drawn everything; the next draw starts
    /// a new one from the machine's state (after preferences change).
    pub fn stop(&mut self) {
        self.sync();
        self.worker = None;
    }

    /// The drawing threads' profiling counters, added to `rdp`'s.
    pub fn add_counters(&self, rdp: &mut Rdp) {
        if let Some(worker) = &self.worker {
            self.drain();
            for theirs in &worker.shared.rdps {
                let theirs = theirs.lock().unwrap();
                rdp.three_texel_samples += theirs.three_texel_samples;
                rdp.rgba16_samples += theirs.rgba16_samples;
                rdp.skipped_color_reads += theirs.skipped_color_reads;
                rdp.pixels += theirs.pixels;
                rdp.spans += theirs.spans;
                rdp.fast_span_pixels += theirs.fast_span_pixels;
            }
        }
    }
}

/// The byte ranges of RDRAM (start, length) a draw may reach with the
/// RDP's state as it is: the color image's rows the draw covers within the
/// scissor box, and the depth image's (at the color image's width) where it
/// is compared or updated in one- and two-cycle modes. Pixels' addresses
/// wrap at 8 MB.
pub(crate) fn draw_reach(rdp: &Rdp, words: &[u64]) -> [(usize, usize); 2] {
    let image = rdp.color_image;
    let width = image.width as usize;
    // The rows the command covers (a rectangle's last one included, as
    // fill and copy modes draw it), within the scissor box's.
    let w = words[0];
    let (top, bottom) = match w >> 56 & 0x3f {
        // YH and YL (s11.2).
        0x08..=0x0f => {
            let (yh, yl) = ((w as i32) << 18 >> 18, ((w >> 32) as i32) << 18 >> 18);
            (yh >> 2, (yl + 3) >> 2)
        }
        // A rectangle's ULY and LRY (10.2).
        _ => ((w & 0xfff) as i32 >> 2, ((w >> 32 & 0xfff) as i32 >> 2) + 1),
    };
    let y0 = top.max((rdp.scissor[1] >> 2) as i32) as usize;
    let y1 = bottom.min((rdp.scissor[3] >> 2) as i32).max(0) as usize;
    let rows = y1.saturating_sub(y0);
    let stride = width * [1, 1, 2, 4][image.size as usize & 3];
    let color = ((image.addr as usize).wrapping_add(y0 * stride), rows * stride);
    let depth = if rdp.cycle_type() < 2 && rdp.other_modes >> 4 & 3 != 0 {
        ((rdp.z_image as usize).wrapping_add(y0 * width * 2), rows * width * 2)
    } else {
        (0, 0)
    };
    [color, depth]
}

/// A copy of the machine starts without a drawing thread, once everything
/// sent to this one is drawn (so that its RDRAM is complete).
impl Clone for RdpThread {
    fn clone(&self) -> RdpThread {
        self.drain();
        let mut copy = RdpThread::new(self.enabled);
        (copy.serial, copy.check_marks, copy.exact, copy.drawers) = (self.serial, self.check_marks, self.exact, self.drawers);
        copy
    }
}

impl Worker {
    fn start(rdp: &Rdp, rdram: Rdram, drawers: usize) -> Worker {
        let shared = Arc::new(Shared {
            rdps: (0..drawers).map(|_| Mutex::new(rdp.clone())).collect(),
            done: Mutex::new(Progress { each: vec![0; drawers], all: 0 }),
            drawn: Condvar::new(),
            reached: (0..drawers).map(|_| AtomicU32::new(0)).collect(),
            waiting: AtomicU32::new(0),
            meet: Barrier::new(drawers),
            picture: Mutex::new((0, Vec::new(), 0, 0)),
            canvas: Canvas(std::cell::UnsafeCell::new(Vec::new()), std::sync::atomic::AtomicPtr::new(std::ptr::null_mut())),
        });
        let (mut senders, mut threads) = (Vec::new(), Vec::new());
        for line in 0..drawers {
            let (sender, receiver) = sync_channel(BATCHES);
            let theirs = shared.clone();
            let rdram = Rdram(rdram.0, rdram.1);
            let lines = Lines { first: line as u32, every: drawers as u32 };
            threads.push(std::thread::Builder::new()
                .name(if drawers == 1 { "rdp".into() } else { format!("rdp{line}") })
                .spawn(move || draw(receiver, &theirs, rdram, lines))
                .expect("cannot start the RDP's thread"));
            senders.push(sender);
        }
        Worker { senders, shared, threads, sent: Cell::new(0) }
    }
}

/// The threads are told to stop (their channels close) and joined, before
/// the machine's RDRAM can go.
impl Drop for Worker {
    fn drop(&mut self) {
        self.senders.clear();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn draw(receiver: Receiver<Arc<Batch>>, shared: &Shared, rdram: Rdram, lines: Lines) {
    let n = lines.first as usize;
    let alone = lines.every == 1;
    // The images' commands as last applied (none yet).
    let (mut color, mut depth) = (None, None);
    while let Ok(batch) = receiver.recv() {
        let mut rdp = shared.rdps[n].lock().unwrap();
        for item in &batch.items {
            match *item {
                Item::Picture(regs, number) => {
                    // Taken once all have drawn everything before, each
                    // thread converting its own lines; none draws until all
                    // are done, as it could draw into the picture.
                    let size = crate::vi::picture_size(&regs);
                    let canvas = shared.canvas.0.get();
                    if n == 0 {
                        // Safety: see Canvas (no one else touches it now).
                        let canvas = unsafe { &mut *canvas };
                        canvas.resize(size.map_or(0, |(width, height, _)| width * height), 0);
                        shared.canvas.1.store(canvas.as_mut_ptr(), Ordering::Relaxed);
                    }
                    shared.meet.wait();
                    if let Some((width, height, stride)) = size {
                        // Safety: as for drawing, these lines of RDRAM are
                        // marked; the canvas's lines are this thread's.
                        let rdram = unsafe { std::slice::from_raw_parts(rdram.0, rdram.1) };
                        let pixels = shared.canvas.1.load(Ordering::Relaxed);
                        for y in (lines.first as usize..height).step_by(lines.every as usize) {
                            let line = unsafe { std::slice::from_raw_parts_mut(pixels.add(y * width), width) };
                            crate::vi::convert_line(&regs, rdram, line, y, stride);
                        }
                    }
                    shared.meet.wait();
                    if n == 0 {
                        let mut latest = shared.picture.lock().unwrap();
                        let (serial, pixels, width, height) = &mut *latest;
                        // Safety: see Canvas (the others are past it).
                        std::mem::swap(pixels, unsafe { &mut *canvas });
                        (*width, *height) = size.map_or((0, 0), |(width, height, _)| (width, height));
                        *serial += 1;
                    }
                    shared.reach(n, number);
                }
                Item::State(w) => {
                    let image = match w >> 56 & 0x3f {
                        0x3f => Some(&mut color),
                        0x3e => Some(&mut depth),
                        _ => None,
                    };
                    if let Some(last) = image
                        && *last != Some(w)
                    {
                        *last = Some(w);
                        if !alone {
                            shared.meet.wait();
                        }
                    }
                    rdp.set_state(w);
                }
                Item::Tmem { at, from, len } => {
                    let (at, from, len) = (usize::from(at), from as usize, usize::from(len));
                    rdp.tmem[at..at + len].copy_from_slice(&batch.tmem[from..from + len]);
                }
                Item::Draw { ref words, len, cycles, number } => {
                    // Safety: see Rdram; the machine does not touch the
                    // pages this draws until it has waited for it.
                    let rdram = unsafe { std::slice::from_raw_parts_mut(rdram.0, rdram.1) };
                    Raster { rdp: &mut rdp, rdram, cycles, lines }.draw(&words[..len]);
                    shared.reach(n, number);
                }
            }
        }
        drop(rdp);
        let mut done = shared.done.lock().unwrap();
        done.each[n] += 1;
        let all = done.each.iter().copied().min().unwrap_or(0);
        if all > done.all {
            done.all = all;
            drop(done);
            shared.drawn.notify_all();
        }
    }
}
