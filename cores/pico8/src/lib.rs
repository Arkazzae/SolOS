//! A PICO-8 compatible console in Rust: its own P8 Lua (parsed, compiled
//! to register instructions, run by a machine with 16.16 numbers,
//! metatables, coroutines and a collector), the drawing API on PICO-8's
//! memory, and the four-channel synthesiser.
//!
//! The core does no I/O: the host gives it the cartridge's bytes and the
//! pads each frame, and takes the picture (palette indices), the sound
//! and the 256 bytes of saved data. See docs/pico8.md.

mod api;
mod audio;
mod cart;
mod compile;
mod fix;
mod graphics;
mod heap;
mod parse;
mod png;
mod state;
mod vm;

use std::rc::Rc;

pub use audio::SAMPLE_HZ;
pub use cart::Cartridge;
pub use graphics::{HEIGHT, PALETTE, WIDTH};

use api::Machine;
use heap::{Closure, Value};
use vm::{Exit, Proto, Vm};

pub const LEFT: u8 = 1;
pub const RIGHT: u8 = 2;
pub const UP: u8 = 4;
pub const DOWN: u8 = 8;
/// 🅾️, the first action button.
pub const O: u8 = 16;
/// ❎, the second.
pub const X: u8 = 32;
/// Pause: opens the pause menu, unless the cartridge keeps it shut.
pub const PAUSE: u8 = 64;

/// Work slices (4096 back jumps and calls each) a frame may take before
/// the cartridge counts as stuck: seconds on the Zero 2 W, far beyond
/// what a playable frame takes. Starting up may take longer.
const FRAME_SLICES: u32 = 6_000;
const BOOT_SLICES: u32 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    pub index: usize,
    pub label: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// The cartridge's code from the top.
    Boot,
    Init,
    Update,
    Draw,
    /// The code ended with no _update or _draw, or stop(): the picture stays.
    Ended,
}

const STATE_MAGIC: &[u8; 4] = b"P8s3";

/// A sum of the code, to know a state is of this cartridge.
fn code_sum(code: &[u8]) -> u32 {
    code.iter().fold(0x811c_9dc5u32, |h, &b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
}

/// A function and all those inside it.
fn flatten(p: &Rc<Proto>, out: &mut Vec<Rc<Proto>>) {
    out.push(p.clone());
    for c in &p.protos {
        flatten(c, out);
    }
}

/// The pause menu, open: the item chosen, and the buttons held before.
struct Pause {
    at: usize,
    held: u8,
}

/// A cartridge's label, its cover: 128x128 indices into PALETTE, from
/// the picture of a .p8.png or the __label__ of a .p8.
pub fn label(bytes: &[u8]) -> Option<Box<[u8; WIDTH * HEIGHT]>> {
    if bytes.starts_with(b"\x89PNG") {
        return png::label(bytes).ok();
    }
    Cartridge::parse(bytes).ok()?.label
}

pub struct Pico8 {
    /// This frame runs _update and not _draw (a host behind time).
    skip_draw: bool,
    pause: Option<Pause>,
    /// The pause button as it was the frame before.
    pause_held: bool,
    vm: Vm,
    cart: Cartridge,
    /// The cartridge's functions by their number, for save states.
    protos: Vec<Rc<Proto>>,
    main: Value,
    phase: Phase,
    picture: Box<[u8; WIDTH * HEIGHT]>,
    samples: Vec<i16>,
    fault: Option<Error>,
    seed: i32,
}

impl Pico8 {
    pub fn new(bytes: &[u8]) -> Result<Pico8, Error> {
        let cart = Cartridge::parse(bytes).map_err(|e| Error(e.to_string()))?;
        Pico8::from_cart(cart, 0)
    }

    /// With saved data from an earlier run (cartdata() says it is there).
    pub fn with_persistent(bytes: &[u8], persistent: &[u8; 256]) -> Result<Pico8, Error> {
        let mut p = Pico8::new(bytes)?;
        p.set_persistent(persistent);
        Ok(p)
    }

    fn from_cart(cart: Cartridge, seed: i32) -> Result<Pico8, Error> {
        let chunk = parse::parse(&cart.code).map_err(|e| Error(format!("syntax error {}", e.0)))?;
        let mut vm = Vm::new(Machine::new(cart.rom.clone()));
        let proto: Rc<Proto> = compile::compile(&chunk, &mut vm.heap).map_err(|e| Error(format!("syntax error {e}")))?;
        for (byte, value) in api::GLYPHS {
            vm.set_global(&[byte], Value::Num(value));
        }
        let mut protos = Vec::new();
        flatten(&proto, &mut protos);
        protos.sort_by_key(|p| p.id);
        let main = vm.heap.closure(Closure { proto, upvals: Box::new([]) });
        api::srand(&mut vm.m.ram, seed);
        let mut p = Pico8 {
            skip_draw: false,
            pause: None,
            pause_held: false,
            vm,
            cart,
            protos,
            main,
            phase: Phase::Boot,
            picture: Box::new([0; WIDTH * HEIGHT]),
            samples: Vec::new(),
            fault: None,
            seed,
        };
        p.root();
        Ok(p)
    }

    /// Keeps the main function and the menu's callbacks from the collector.
    fn root(&mut self) {
        if self.vm.registry.len() < 6 {
            self.vm.registry.resize(6, Value::Nil);
        }
        self.vm.registry[5] = self.main;
    }

    /// Installs saved data before the cartridge starts.
    pub fn set_persistent(&mut self, data: &[u8]) {
        let n = data.len().min(256);
        self.vm.m.ram[0x5e00..0x5e00 + n].copy_from_slice(&data[..n]);
        self.vm.m.persistent_loaded = true;
    }

    /// Seeds rnd() before the cartridge starts (the host's clock, say).
    pub fn seed(&mut self, seed: u32) {
        self.seed = seed as i32;
        api::srand(&mut self.vm.m.ram, seed as i32);
    }

    /// The time of day for stat(80..95), seconds since 1970.
    pub fn set_clock(&mut self, unix: u64) {
        self.vm.m.clock = unix;
    }

    fn callback(&self, name: &str) -> Option<Value> {
        let v = self.vm.global(name);
        matches!(v, Value::Func(_) | Value::Native(_) | Value::Table(_)).then_some(v)
    }

    /// Runs a frame without _draw, the picture staying as it was: what
    /// PICO-8 does when a game cannot keep up, its updates kept at full
    /// speed. A host behind time runs every other frame this way.
    pub fn run_frame_without_draw(&mut self, buttons: [u8; 8]) -> Result<(), Error> {
        self.skip_draw = true;
        let r = self.run_frame(buttons);
        self.skip_draw = false;
        r
    }

    /// Runs one frame of the game with these pads held (LEFT … X bits,
    /// one byte a player).
    pub fn run_frame(&mut self, buttons: [u8; 8]) -> Result<(), Error> {
        self.samples.clear();
        if let Some(e) = &self.fault {
            return Err(e.clone());
        }
        let pause = buttons.iter().any(|b| b & PAUSE != 0);
        let pressed = pause && !self.pause_held;
        self.pause_held = pause;
        let mut buttons = buttons;
        if self.pause.is_some() {
            return self.pause_frame(buttons[0] | buttons[1]);
        }
        if pressed && self.phase != Phase::Ended {
            if self.vm.m.ram[0x5f30] & 1 != 0 {
                // Kept shut once: the cartridge sees btn(6) instead.
                self.vm.m.ram[0x5f30] = 0;
            } else {
                self.open_pause();
                // Held buttons are not presses in the menu.
                return self.pause_frame(buttons[0] | buttons[1]);
            }
        }
        if !pressed {
            for b in &mut buttons {
                *b &= !PAUSE;
            }
        }
        if self.phase != Phase::Ended {
            self.vm.m.input(buttons);
            self.vm.begin_frame(if matches!(self.phase, Phase::Boot | Phase::Init) { BOOT_SLICES } else { FRAME_SLICES });
            if let Err(e) = self.step() {
                if let Some(msg) = e.strip_prefix("\0stop") {
                    self.phase = Phase::Ended;
                    self.vm.thread.frames.clear();
                    if !msg.is_empty() {
                        let m = msg.as_bytes().to_vec();
                        let ram = &mut self.vm.m.ram;
                        let c = ram[0x5f25];
                        graphics::print_at_cursor(ram, &m, c);
                    }
                } else if e == "\0run" {
                    self.restart();
                } else {
                    let e = Error(format!("runtime error {e}"));
                    self.show_error(&e.0);
                    self.fault = Some(e.clone());
                    graphics::present(&self.vm.m.ram, &mut self.picture);
                    return Err(e);
                }
            }
        }
        let fps = self.vm.m.fps;
        self.vm.m.audio.render(&self.vm.m.ram, fps, &mut self.samples);
        graphics::present(&self.vm.m.ram, &mut self.picture);
        if self.vm.gc_due() {
            self.vm.collect();
        }
        Ok(())
    }

    /// Runs the phases until a frame is shown: the code at the top, _init,
    /// then each frame _update and _draw; a flip() shows a frame anywhere.
    fn step(&mut self) -> Result<(), String> {
        loop {
            let exit = if self.vm.suspended() {
                self.vm.resume_main()?
            } else {
                let f = match self.phase {
                    Phase::Boot => Some(self.main),
                    Phase::Init => self.callback("_init"),
                    Phase::Update => {
                        let sixty = self.callback("_update60");
                        self.vm.m.fps = if sixty.is_some() { 60 } else { 30 };
                        sixty.or_else(|| self.callback("_update"))
                    }
                    Phase::Draw => self.callback("_draw"),
                    Phase::Ended => return Ok(()),
                };
                match f {
                    Some(f) => self.vm.start(f, &[])?,
                    None => Exit::Return(Vec::new()),
                }
            };
            if let Exit::Flip = exit {
                self.vm.m.frames += 1;
                return Ok(());
            }
            match self.phase {
                Phase::Boot => {
                    self.phase = Phase::Init;
                    self.vm.m.fps = if self.callback("_update60").is_some() { 60 } else { 30 };
                }
                Phase::Init => {
                    if self.callback("_update").is_none() && self.callback("_update60").is_none() && self.callback("_draw").is_none() {
                        self.phase = Phase::Ended;
                        return Ok(());
                    }
                    self.phase = Phase::Update;
                }
                Phase::Update if self.skip_draw => {
                    self.vm.m.frames += 1;
                    return Ok(());
                }
                Phase::Update => self.phase = Phase::Draw,
                Phase::Draw => {
                    self.phase = Phase::Update;
                    self.vm.m.frames += 1;
                    return Ok(());
                }
                Phase::Ended => return Ok(()),
            }
        }
    }

    /// Opens PICO-8's pause menu, as its pause button does.
    pub fn open_pause(&mut self) {
        self.pause = Some(Pause { at: 0, held: 0xff });
    }
    pub fn paused(&self) -> bool {
        self.pause.is_some()
    }

    /// The menu's items: continue, the cartridge's own (menuitem), reset.
    fn pause_items(&self) -> Vec<(Option<usize>, String)> {
        let mut items = vec![(None, "continue".to_string())];
        for (i, m) in self.vm.m.menu.iter().enumerate() {
            if let Some((label, _)) = m {
                items.push((Some(i + 1), label.clone()));
            }
        }
        items.push((Some(0), "reset cart".to_string()));
        items
    }

    /// A frame with the pause menu open: the game stands still and is
    /// silent, the menu is drawn over its picture.
    fn pause_frame(&mut self, buttons: u8) -> Result<(), Error> {
        let pause = self.pause.as_mut().unwrap();
        let new = buttons & !pause.held;
        pause.held = buttons;
        let mut at = pause.at;
        let items = self.pause_items();
        at = at.min(items.len() - 1);
        if new & UP != 0 {
            at = (at + items.len() - 1) % items.len();
        }
        if new & DOWN != 0 {
            at = (at + 1) % items.len();
        }
        self.pause.as_mut().unwrap().at = at;
        let side = new & (LEFT | RIGHT);
        let confirm = new & (O | X) != 0;
        if new & PAUSE != 0 {
            self.pause = None;
        } else if confirm || side != 0 {
            match items[at].0 {
                None if confirm => self.pause = None,
                Some(0) if confirm => {
                    self.pause = None;
                    self.restart();
                    return Ok(());
                }
                Some(i) if i > 0 => {
                    // L 1, R 2, X 32, as PICO-8 gives a menu item.
                    let b = side | if confirm { 32 } else { 0 };
                    let keep = self.activate_menu_item(i, b)?;
                    if confirm && !keep {
                        self.pause = None;
                    }
                }
                _ => {}
            }
        }
        let n = SAMPLE_HZ / self.fps().max(1);
        self.samples.clear();
        self.samples.resize(n, 0);
        graphics::present(&self.vm.m.ram, &mut self.picture);
        if self.pause.is_some() {
            self.draw_pause();
        }
        Ok(())
    }

    /// The menu over the picture: a box with a white frame, an arrow at
    /// the item chosen.
    fn draw_pause(&mut self) {
        let items = self.pause_items();
        let at = self.pause.as_ref().map_or(0, |p| p.at);
        let mut ram: Box<graphics::Ram> = vec![0u8; 0x10000].into_boxed_slice().try_into().unwrap();
        graphics::reset(&mut ram);
        let h = items.len() as i32 * 8 + 6;
        let (x0, y0, x1) = (24, 64 - h / 2, 103);
        let y1 = y0 + h - 1;
        graphics::rect(&mut ram, x0, y0, x1, y1, 0, true);
        graphics::rect(&mut ram, x0 + 1, y0 + 1, x1 - 1, y1 - 1, 7, false);
        for (i, (_, label)) in items.iter().enumerate() {
            let y = y0 + 4 + i as i32 * 8;
            graphics::print(&mut ram, label.as_bytes(), x0 + 12, y, 7);
            if i == at {
                graphics::print(&mut ram, b"\x17", x0 + 5, y, 7);
            }
        }
        let mut menu = [0u8; WIDTH * HEIGHT];
        graphics::present(&ram, &mut menu);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let i = y as usize * WIDTH + x as usize;
                self.picture[i] = menu[i];
            }
        }
    }

    /// run(): the cartridge again from the start, its saved data kept.
    fn restart(&mut self) {
        let saved: Vec<u8> = self.vm.m.ram[0x5e00..0x5f00].to_vec();
        let loaded = self.vm.m.persistent_loaded;
        let clock = self.vm.m.clock;
        if let Ok(mut p) = Pico8::from_cart(self.cart.clone(), self.seed.wrapping_add(1)) {
            p.vm.m.ram[0x5e00..0x5f00].copy_from_slice(&saved);
            p.vm.m.persistent_loaded = loaded;
            p.vm.m.clock = clock;
            *self = p;
        }
    }

    /// PICO-8's way: the error at the bottom of the picture.
    fn show_error(&mut self, msg: &str) {
        let ram = &mut self.vm.m.ram;
        graphics::reset(ram);
        let mut lines: Vec<Vec<u8>> = Vec::new();
        for word_line in msg.split('\n') {
            let bytes: Vec<u8> = word_line.bytes().map(|b| if b.is_ascii_uppercase() { b.to_ascii_lowercase() } else { b }).collect();
            for chunk in bytes.chunks(31) {
                lines.push(chunk.to_vec());
            }
        }
        let lines = &lines[..lines.len().min(8)];
        let top = 128 - 6 * lines.len() as i32 - 2;
        graphics::rect(ram, 0, top - 1, 127, 127, 0, true);
        for (i, l) in lines.iter().enumerate() {
            graphics::print(ram, l, 1, top + 6 * i as i32, if i == 0 { 8 } else { 6 });
        }
    }

    /// The whole machine, for Quick Resume: the Lua heap and threads (a
    /// game stopped at a flip() goes on from there), the RAM, the sound.
    pub fn save_state(&self) -> Vec<u8> {
        let mut w = state::W::default();
        w.0.extend_from_slice(STATE_MAGIC);
        w.u32(code_sum(&self.cart.code));
        w.u8(self.phase as u8);
        w.bytes(self.fault.as_ref().map_or("", |e| e.0.as_str()).as_bytes());
        w.bool(self.fault.is_some());
        w.value(self.main);
        w.i32(self.seed);
        self.vm.save(&mut w);
        w.0
    }

    /// Goes back to a saved state of the same cartridge; on an error the
    /// game is as it was.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), Error> {
        let bad = |e: state::BadState| Error(format!("save state: {}", e.0));
        let mut r = state::R::new(data);
        if r.take(4).map_err(bad)? != STATE_MAGIC {
            return Err(Error("save state: not a PICO-8 state of this version".into()));
        }
        if r.u32().map_err(bad)? != code_sum(&self.cart.code) {
            return Err(Error("save state: made by another cartridge".into()));
        }
        let phase = match r.u8().map_err(bad)? {
            0 => Phase::Boot,
            1 => Phase::Init,
            2 => Phase::Update,
            3 => Phase::Draw,
            _ => Phase::Ended,
        };
        let fault_text = String::from_utf8_lossy(r.bytes().map_err(bad)?).into_owned();
        let fault = r.bool().map_err(bad)?.then_some(Error(fault_text));
        let main = r.value().map_err(bad)?;
        let seed = r.i32().map_err(bad)?;
        let mut fresh = Pico8::from_cart(self.cart.clone(), seed)?;
        fresh.vm.load(&mut r, &self.protos).map_err(bad)?;
        if !r.done() {
            return Err(Error("save state: more after its end".into()));
        }
        if !matches!(main, Value::Func(_)) || !fresh.vm.heap.valid(main, api::NATIVES.len()) {
            return Err(Error("save state: no main function".into()));
        }
        fresh.vm.m.clock = self.vm.m.clock;
        fresh.main = main;
        fresh.phase = phase;
        fresh.fault = fault;
        fresh.root();
        graphics::present(&fresh.vm.m.ram, &mut fresh.picture);
        *self = fresh;
        Ok(())
    }

    /// The picture: palette indices into PALETTE, row after row.
    pub fn frame(&self) -> &[u8; WIDTH * HEIGHT] {
        &self.picture
    }
    /// The frame's sound: one channel at SAMPLE_HZ.
    pub fn audio(&self) -> &[i16] {
        &self.samples
    }
    /// 30 or 60: how much time a frame is.
    pub fn fps(&self) -> usize {
        self.vm.m.fps as usize
    }
    pub fn frames(&self) -> u64 {
        self.vm.m.frames
    }
    pub fn error(&self) -> Option<&Error> {
        self.fault.as_ref()
    }
    /// Whether the cartridge ended (stop(), or no main loop).
    pub fn ended(&self) -> bool {
        self.phase == Phase::Ended
    }
    pub fn memory(&self) -> &[u8; 0x10000] {
        &self.vm.m.ram
    }
    pub fn memory_mut(&mut self) -> &mut [u8; 0x10000] {
        &mut self.vm.m.ram
    }
    /// The 256 bytes of saved data (cartdata).
    pub fn persistent(&self) -> &[u8] {
        &self.vm.m.ram[0x5e00..0x5f00]
    }
    pub fn cartdata_id(&self) -> Option<&str> {
        self.vm.m.cartdata.as_deref()
    }

    /// A global as a number, for tests and tools.
    pub fn global_number(&self, name: &str) -> Option<f64> {
        match self.vm.global(name) {
            Value::Num(n) => Some(fix::to_f64(n)),
            _ => None,
        }
    }
    /// A global as text (tostr's), for tests and tools.
    pub fn global_text(&self, name: &str) -> String {
        String::from_utf8_lossy(&self.vm.text(self.vm.global(name))).into_owned()
    }

    pub fn menu_items(&self) -> Vec<MenuItem> {
        self.vm
            .m
            .menu
            .iter()
            .enumerate()
            .filter_map(|(i, m)| m.as_ref().map(|(label, _)| MenuItem { index: i + 1, label: label.clone() }))
            .collect()
    }

    /// Runs a menu item's callback with the menu's buttons (1 left, 2
    /// right, 32 chosen); true keeps the menu open.
    pub fn activate_menu_item(&mut self, index: usize, buttons: u8) -> Result<bool, Error> {
        let Some(&cb) = self.vm.registry.get(index.wrapping_sub(1)).filter(|_| (1..=5).contains(&index)) else {
            return Ok(false);
        };
        if cb == Value::Nil {
            return Ok(false);
        }
        let mask = self.vm.m.menu[index - 1].as_ref().map_or(0, |m| m.1);
        self.vm.slices = FRAME_SLICES;
        self.vm.m.menu_active = Some(index);
        let r = self.vm.call(cb, &[Value::int(i32::from(buttons & !mask & 0x7f))]);
        self.vm.m.menu_active = None;
        match r {
            Ok(r) => Ok(r.first().is_some_and(|v| v.truthy())),
            Err(e) => {
                let e = Error(format!("runtime error {e}"));
                self.fault = Some(e.clone());
                Err(e)
            }
        }
    }

    /// printh()'s messages since the last call.
    pub fn take_debug_output(&mut self) -> Vec<String> {
        std::mem::take(&mut self.vm.m.debug)
    }
}
