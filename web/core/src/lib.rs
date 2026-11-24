//! Sol OS's emulation cores in a browser. The page loads this crate as a
//! WebAssembly module (wasm32-unknown-unknown, no bindings generator) and
//! drives one machine through the plain functions below: a game in, a frame
//! run with the pads' buttons, the picture as RGBA and the sound out, saves
//! and save states both ways. Like the cores it does no I/O of its own: the
//! page keeps the games and the saves, and gives it the time.
//!
//! Bytes cross in the module's memory. The page asks for room with alloc,
//! writes into it and hands the pointer over; what comes back (a save state,
//! a save, an error) waits in one buffer, at out, until the next call that
//! returns bytes. Memory can grow at any call that allocates, which leaves
//! the page's views of it stale: it makes them again after each call.
//!
//! The functions keep their plain names only in the module, where they are
//! its exports. A native build (examples/check.rs) calls them as Rust: there
//! open, close and free under their own names would stand in for the C
//! library's, which std calls to read a file and to free memory.

mod cores;

use std::cell::RefCell;

pub use cores::{Clock, Core};

/// The pads' buttons, as bits of what run_frame takes: bit n is button n of
/// the browser's standard gamepad (the Gamepad API's "standard" mapping),
/// the Xbox pad's layout. Each core puts them where they sit on the
/// console's own pad: the bottom face button is the Nintendo pads' bottom
/// one (B), the right one their right one (A).
pub mod pad {
    pub const SOUTH: u32 = 1 << 0;
    pub const EAST: u32 = 1 << 1;
    pub const WEST: u32 = 1 << 2;
    pub const NORTH: u32 = 1 << 3;
    pub const L1: u32 = 1 << 4;
    pub const R1: u32 = 1 << 5;
    pub const L2: u32 = 1 << 6;
    pub const R2: u32 = 1 << 7;
    pub const SELECT: u32 = 1 << 8;
    pub const START: u32 = 1 << 9;
    pub const L3: u32 = 1 << 10;
    pub const R3: u32 = 1 << 11;
    pub const UP: u32 = 1 << 12;
    pub const DOWN: u32 = 1 << 13;
    pub const LEFT: u32 = 1 << 14;
    pub const RIGHT: u32 = 1 << 15;
}

/// The running machine, and its last picture as RGBA, 4 bytes a pixel.
struct Machine {
    core: Box<dyn Core>,
    picture: Vec<u8>,
}

thread_local! {
    static MACHINE: RefCell<Option<Machine>> = const { RefCell::new(None) };
    /// What the last call that returns bytes left for the page.
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// A message for the browser's console: the page passes in a function that
/// takes the UTF-8 bytes.
#[cfg(target_arch = "wasm32")]
mod console {
    #[link(wasm_import_module = "sol")]
    unsafe extern "C" {
        pub fn log(ptr: *const u8, len: usize);
    }
}

fn log(message: &str) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        console::log(message.as_ptr(), message.len())
    };
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("{message}");
}

/// The machine, for a call that needs one: without one (no game opened) the
/// call does nothing and gives back the default.
fn with<T: Default>(f: impl FnOnce(&mut Machine) -> T) -> T {
    MACHINE.with_borrow_mut(|machine| machine.as_mut().map(f).unwrap_or_default())
}

/// Leaves bytes for the page at out; their length.
fn put_out(bytes: &[u8]) -> usize {
    OUT.with_borrow_mut(|out| {
        out.clear();
        out.extend_from_slice(bytes);
        out.len()
    })
}

/// Bytes the page wrote at ptr.
///
/// # Safety
/// ptr must point at len bytes the page owns (from alloc).
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(ptr, len) } }
}

/// Room for len bytes, for the page to write into and pass to a call.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut buffer = std::mem::ManuallyDrop::new(Vec::<u8>::with_capacity(len));
    buffer.as_mut_ptr()
}

/// Gives back room alloc gave.
///
/// # Safety
/// ptr and len must be what alloc gave and took.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub unsafe extern "C" fn free(ptr: *mut u8, len: usize) {
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len) });
}

/// Where the bytes of the last call that returns some are, and how many.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn out() -> *const u8 {
    OUT.with_borrow(|out| out.as_ptr())
}

#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn out_len() -> usize {
    OUT.with_borrow(|out| out.len())
}

/// Opens a game on a console, by the console's id (cores::open) and the
/// game's file, read whole, and powers on. 0 if it runs; -1 if not, and why
/// at out (UTF-8).
///
/// # Safety
/// The pointers must point at the lengths given, from alloc.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub unsafe extern "C" fn open(sys: *const u8, sys_len: usize, rom: *const u8, rom_len: usize) -> i32 {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| std::panic::set_hook(Box::new(|info| log(&format!("solweb: {info}")))));
    let sys = String::from_utf8_lossy(unsafe { bytes(sys, sys_len) }).into_owned();
    let opened = cores::open(&sys, unsafe { bytes(rom, rom_len) });
    MACHINE.with_borrow_mut(|machine| match opened {
        Ok(core) => {
            let (width, height) = core.size();
            *machine = Some(Machine { core, picture: vec![0; width * height * 4] });
            0
        }
        Err(error) => {
            *machine = None;
            put_out(error.as_bytes());
            -1
        }
    })
}

/// Turns the machine off.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn close() {
    MACHINE.with_borrow_mut(|machine| *machine = None);
}

/// Runs a frame with each player's buttons held (bits of pad): a console
/// with fewer pads takes the first ones.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn run_frame(pad1: u32, pad2: u32, pad3: u32, pad4: u32) {
    with(|m| m.core.run_frame([pad1, pad2, pad3, pad4]))
}

/// The picture's width and height now, in the console's pixels.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn width() -> u32 {
    with(|m| m.core.size().0 as u32)
}

#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn height() -> u32 {
    with(|m| m.core.size().1 as u32)
}

/// The shape of the console's pixels on the screen it was made for: their
/// width over their height.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn pixel_aspect() -> f64 {
    with(|m| m.core.pixel_aspect())
}

/// The console's frames a second.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn frame_hz() -> f64 {
    with(|m| m.core.frame_hz())
}

/// The last frame as RGBA, width times height pixels, row after row.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn picture() -> *const u8 {
    MACHINE.with_borrow_mut(|machine| {
        let Some(m) = machine else { return std::ptr::null() };
        let (width, height) = m.core.size();
        m.picture.resize(width * height * 4, 0);
        m.core.picture(&mut m.picture);
        m.picture.as_ptr()
    })
}

/// The last frame's sound: audio_len samples at audio, sample_hz a second,
/// left and right in turns when audio_stereo is 1.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn audio() -> *const i16 {
    MACHINE.with_borrow(|machine| machine.as_ref().map_or(std::ptr::null(), |m| m.core.audio().0.as_ptr()))
}

#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn audio_len() -> usize {
    with(|m| m.core.audio().0.len())
}

#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn audio_stereo() -> u32 {
    with(|m| u32::from(m.core.audio().1))
}

#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn sample_hz() -> u32 {
    with(|m| m.core.sample_hz())
}

/// The whole machine, for Quick Resume: its length, the bytes at out.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn save_state() -> usize {
    with(|m| put_out(&m.core.save_state()))
}

/// Puts the machine back as a save state has it: 0, or -1 and why at out.
/// A state that does not load leaves the machine as it was.
///
/// # Safety
/// ptr must point at len bytes, from alloc.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub unsafe extern "C" fn load_state(ptr: *const u8, len: usize) -> i32 {
    let data = unsafe { bytes(ptr, len) };
    with(|m| match m.core.load_state(data) {
        Ok(()) => 0,
        Err(error) => {
            put_out(error.as_bytes());
            -1
        }
    })
}

/// The cartridge's own save (battery RAM, flash, EEPROM, PICO-8's cartdata):
/// its length, the bytes at out; -1 for a game without one.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn sram() -> i32 {
    MACHINE.with_borrow(|machine| match machine.as_ref().and_then(|m| m.core.sram()) {
        Some(data) => put_out(&data) as i32,
        None => -1,
    })
}

/// Puts back a save the page kept, before running.
///
/// # Safety
/// ptr must point at len bytes, from alloc.
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub unsafe extern "C" fn load_sram(ptr: *const u8, len: usize) {
    let data = unsafe { bytes(ptr, len) };
    with(|m| m.core.load_sram(data))
}

/// The time now: seconds since 1970 in UTC, and the player's time zone's
/// offset from UTC in seconds. The browser has the time; the module cannot
/// ask for it (std's clock panics on wasm32-unknown-unknown). The page gives
/// it after powering on, before the save goes in, after a state goes in, and
/// before it takes a save out; each core takes what its cartridge needs of it
/// (cores::Core::set_clock).
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
pub extern "C" fn set_clock(utc: f64, offset: i32) {
    let utc = utc.max(0.0) as u64;
    let clock = Clock { utc, local: utc.saturating_add_signed(i64::from(offset)) };
    with(|m| m.core.set_clock(clock))
}
