//! Runtime SDL2 window, controllers and PCM for the Mega Drive desktop host.
//! ABI: SDL2 SDL_events.h/SDL_audio.h; calls stay on the window's main thread.
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

// SDL_Quit owns global subsystem state. Keep this process's host singleton,
// so a second safe Window cannot invalidate a live window/audio device.
static HOST_IN_USE: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
unsafe extern "C" {
    fn dlopen(name: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn dlerror() -> *const c_char;
}

struct Library(*mut c_void);
impl Library {
    fn open(path: Option<&Path>) -> Result<Self, String> {
        #[cfg(not(unix))]
        {
            let _ = path;
            Err("the SDL2 desktop host currently supports Unix platforms".into())
        }
        #[cfg(unix)]
        {
            let paths: Vec<_> = match path {
                Some(path) => vec![path.to_string_lossy().into_owned()],
                None => if cfg!(target_os = "macos") {
                    vec![
                        "/opt/homebrew/lib/libSDL2.dylib",
                        "/usr/local/lib/libSDL2.dylib",
                        "/Library/Frameworks/SDL2.framework/SDL2",
                        "libSDL2.dylib",
                    ]
                } else {
                    vec!["libSDL2-2.0.so.0", "libSDL2.so"]
                }
                .into_iter()
                .map(str::to_owned)
                .collect(),
            };
            let mut errors = Vec::new();
            for name in paths {
                let c =
                    CString::new(name.as_str()).map_err(|_| "SDL2 library path contains NUL")?;
                // RTLD_NOW is 2 on the supported POSIX hosts. Resolve all
                // symbols now; the explicitly chosen library is host-trusted.
                let handle = unsafe { dlopen(c.as_ptr(), 2) };
                if !handle.is_null() {
                    return Ok(Self(handle));
                }
                let message = unsafe { dlerror() };
                errors.push(if message.is_null() {
                    name
                } else {
                    unsafe { CStr::from_ptr(message) }
                        .to_string_lossy()
                        .into_owned()
                });
            }
            Err(format!(
                "SDL2 could not be loaded (use --sdl2-library): {}",
                errors.join("; ")
            ))
        }
    }
    fn symbol(&self, name: &CStr) -> Result<*mut c_void, String> {
        #[cfg(not(unix))]
        {
            let _ = name;
            Err("SDL2 runtime loader unavailable".into())
        }
        #[cfg(unix)]
        {
            let value = unsafe { dlsym(self.0, name.as_ptr()) };
            if value.is_null() {
                Err(format!("SDL2 is missing {}", name.to_string_lossy()))
            } else {
                Ok(value)
            }
        }
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            dlclose(self.0);
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AudioSpec {
    pub freq: c_int,
    pub format: u16,
    pub channels: u8,
    pub silence: u8,
    pub samples: u16,
    pub padding: u16,
    pub size: u32,
    pub callback: Option<unsafe extern "C" fn(*mut c_void, *mut u8, c_int)>,
    pub userdata: *mut c_void,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Version {
    major: u8,
    minor: u8,
    patch: u8,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Keysym {
    scancode: c_int,
    sym: i32,
    modifiers: u16,
    unused: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KeyEvent {
    kind: u32,
    timestamp: u32,
    window: u32,
    state: u8,
    repeat: u8,
    pad: [u8; 2],
    keysym: Keysym,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WindowEvent {
    kind: u32,
    timestamp: u32,
    window: u32,
    event: u8,
    pad: [u8; 3],
    data1: i32,
    data2: i32,
}
#[repr(C)]
#[derive(Clone, Copy)]
union RawEvent {
    kind: u32,
    key: KeyEvent,
    window: WindowEvent,
    padding: [u64; 7],
}

macro_rules! api {
    ($($field:ident : $name:literal ($($arg:ty),*) -> $ret:ty;)+) => {
        struct Api { $($field: unsafe extern "C" fn($($arg),*) -> $ret,)+ _library: Library }
        impl Api {
            fn load(path: Option<&Path>) -> Result<Self, String> {
                let library = Library::open(path)?;
                // Each signature is declared from SDL2's C headers. SDL2 has
                // a stable C ABI; the version is checked before initialization.
                Ok(Self { $($field: unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) -> $ret>(library.symbol(CStr::from_bytes_with_nul(concat!($name, "\0").as_bytes()).unwrap())?) },)+ _library: library })
            }
        }
    }
}
api! {
    set_main_ready: "SDL_SetMainReady" () -> ();
    get_version: "SDL_GetVersion" (*mut Version) -> ();
    init: "SDL_Init" (u32) -> c_int;
    quit: "SDL_Quit" () -> ();
    error: "SDL_GetError" () -> *const c_char;
    hint: "SDL_SetHint" (*const c_char, *const c_char) -> c_int;
    create_window: "SDL_CreateWindow" (*const c_char, c_int, c_int, c_int, c_int, u32) -> *mut c_void;
    destroy_window: "SDL_DestroyWindow" (*mut c_void) -> ();
    window_title: "SDL_SetWindowTitle" (*mut c_void, *const c_char) -> ();
    window_flags: "SDL_GetWindowFlags" (*mut c_void) -> u32;
    create_renderer: "SDL_CreateRenderer" (*mut c_void, c_int, u32) -> *mut c_void;
    destroy_renderer: "SDL_DestroyRenderer" (*mut c_void) -> ();
    logical_size: "SDL_RenderSetLogicalSize" (*mut c_void, c_int, c_int) -> c_int;
    create_texture: "SDL_CreateTexture" (*mut c_void, u32, c_int, c_int, c_int) -> *mut c_void;
    destroy_texture: "SDL_DestroyTexture" (*mut c_void) -> ();
    update_texture: "SDL_UpdateTexture" (*mut c_void, *const c_void, *const c_void, c_int) -> c_int;
    draw_color: "SDL_SetRenderDrawColor" (*mut c_void, u8, u8, u8, u8) -> c_int;
    clear: "SDL_RenderClear" (*mut c_void) -> c_int;
    copy: "SDL_RenderCopy" (*mut c_void, *mut c_void, *const c_void, *const c_void) -> c_int;
    present: "SDL_RenderPresent" (*mut c_void) -> ();
    poll: "SDL_PollEvent" (*mut RawEvent) -> c_int;
    event_state: "SDL_EventState" (u32, c_int) -> u8;
    open_audio: "SDL_OpenAudioDevice" (*const c_char, c_int, *const AudioSpec, *mut AudioSpec, c_int) -> u32;
    close_audio: "SDL_CloseAudioDevice" (u32) -> ();
    clear_audio: "SDL_ClearQueuedAudio" (u32) -> ();
    fullscreen: "SDL_SetWindowFullscreen" (*mut c_void, u32) -> c_int;
    num_joysticks: "SDL_NumJoysticks" () -> c_int;
    is_controller: "SDL_IsGameController" (c_int) -> c_int;
    open_controller: "SDL_GameControllerOpen" (c_int) -> *mut c_void;
    close_controller: "SDL_GameControllerClose" (*mut c_void) -> ();
    controller_attached: "SDL_GameControllerGetAttached" (*mut c_void) -> c_int;
    controller_joystick: "SDL_GameControllerGetJoystick" (*mut c_void) -> *mut c_void;
    joystick_id: "SDL_JoystickInstanceID" (*mut c_void) -> i32;
    controller_update: "SDL_GameControllerUpdate" () -> ();
    controller_button: "SDL_GameControllerGetButton" (*mut c_void, c_int) -> u8;
    controller_axis: "SDL_GameControllerGetAxis" (*mut c_void, c_int) -> i16;
    pause_audio: "SDL_PauseAudioDevice" (u32, c_int) -> ();
    queue_audio: "SDL_QueueAudio" (u32, *const c_void, u32) -> c_int;
    audio_size: "SDL_GetQueuedAudioSize" (u32) -> u32;
    video_driver: "SDL_GetCurrentVideoDriver" () -> *const c_char;
    audio_driver: "SDL_GetCurrentAudioDriver" () -> *const c_char;
    output_size: "SDL_GetRendererOutputSize" (*mut c_void, *mut c_int, *mut c_int) -> c_int;
    read_pixels: "SDL_RenderReadPixels" (*mut c_void, *const c_void, u32, *mut c_void, c_int) -> c_int;
}

#[cfg(target_endian = "little")]
const RGBA32: u32 = 0x1676_2004;
#[cfg(target_endian = "big")]
const RGBA32: u32 = 0x1646_2004;
#[cfg(target_endian = "little")]
const S16SYS: u16 = 0x8010;
#[cfg(target_endian = "big")]
const S16SYS: u16 = 0x9010;

const RGB565: u32 = 0x1515_1002;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Quit,
    Key {
        scancode: i32,
        pressed: bool,
        repeat: bool,
    },
    Focus(bool),
}

pub struct Window {
    api: Api,
    window: *mut c_void,
    renderer: *mut c_void,
    texture: *mut c_void,
    audio: u32,
    initialized: bool,
    controllers: [(i32, *mut c_void); 2],
    fullscreen: bool,
    _main_thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl Window {
    pub fn open(library: Option<&Path>, scale: u32, audio: bool) -> Result<Self, String> {
        if !(1..=6).contains(&scale) {
            return Err("window scale must be 1..6".into());
        }
        let api = Api::load(library)?;
        let mut version = Version::default();
        unsafe {
            (api.get_version)(&mut version);
        }
        if version.major != 2 || (version.minor == 0 && version.patch < 4) {
            return Err("SDL2 2.0.4 or newer is required".into());
        }
        if HOST_IN_USE
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err("only one SDL2 Mega Drive host can own a process".into());
        }
        let mut s = Self {
            api,
            window: ptr::null_mut(),
            renderer: ptr::null_mut(),
            texture: ptr::null_mut(),
            audio: 0,
            initialized: false,
            controllers: [(-1, ptr::null_mut()); 2],
            fullscreen: false,
            _main_thread: std::marker::PhantomData,
        };
        unsafe {
            (s.api.set_main_ready)();
            let initialized = (s.api.init)(0x20 | 0x4000 | 0x2000 | if audio { 0x10 } else { 0 });
            s.initialized = true;
            if initialized != 0 {
                return Err(s.error("SDL_Init"));
            }
            // This host has no text editor or file-drop action. Disable SDL
            // events with allocated payloads instead of silently leaking them.
            for event in [0x305, 0x1000, 0x1001] {
                (s.api.event_state)(event, 0);
            }
            (s.api.hint)(c"SDL_RENDER_SCALE_QUALITY".as_ptr(), c"0".as_ptr());
            s.window = (s.api.create_window)(
                c"Sol Mega Drive".as_ptr(),
                0x2fff_0000,
                0x2fff_0000,
                (320 * scale) as c_int,
                (240 * scale) as c_int,
                4 | 0x20 | 0x2000,
            );
            if s.window.is_null() {
                return Err(s.error("SDL_CreateWindow"));
            }
            s.renderer = (s.api.create_renderer)(s.window, -1, 2);
            if s.renderer.is_null() {
                s.renderer = (s.api.create_renderer)(s.window, -1, 1);
            }
            if s.renderer.is_null() {
                return Err(s.error("SDL_CreateRenderer"));
            }
            s.check(
                (s.api.logical_size)(s.renderer, 320, 240),
                "SDL_RenderSetLogicalSize",
            )?;
            s.texture = (s.api.create_texture)(s.renderer, RGB565, 1, 320, 240);
            if s.texture.is_null() {
                return Err(s.error("SDL_CreateTexture"));
            }
            if audio {
                let desired = AudioSpec {
                    freq: 48_000,
                    format: S16SYS,
                    channels: 2,
                    samples: 1024,
                    ..AudioSpec::default()
                };
                let mut actual = AudioSpec::default();
                s.audio = (s.api.open_audio)(ptr::null(), 0, &desired, &mut actual, 0);
                if s.audio == 0 {
                    return Err(s.error("SDL_OpenAudioDevice"));
                }
                if (actual.freq, actual.format, actual.channels) != (48_000, S16SYS, 2) {
                    return Err(
                        "SDL audio did not preserve the requested 48kHz stereo S16 contract".into(),
                    );
                }
            }
        }
        eprintln!(
            "SDL2 {}.{}.{} video={} audio={}",
            version.major,
            version.minor,
            version.patch,
            s.driver(false),
            if audio {
                s.driver(true)
            } else {
                "disabled".into()
            }
        );
        Ok(s)
    }
    fn error(&self, operation: &str) -> String {
        let message = unsafe { (self.api.error)() };
        format!(
            "{operation}: {}",
            if message.is_null() {
                "unknown SDL2 error".into()
            } else {
                unsafe { CStr::from_ptr(message) }.to_string_lossy()
            }
        )
    }
    fn check(&self, result: c_int, operation: &str) -> Result<(), String> {
        if result < 0 {
            Err(self.error(operation))
        } else {
            Ok(())
        }
    }
    pub fn driver(&self, audio: bool) -> String {
        let p = unsafe {
            if audio {
                (self.api.audio_driver)()
            } else {
                (self.api.video_driver)()
            }
        };
        if p.is_null() {
            "none".into()
        } else {
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    }
    pub fn focused(&self) -> bool {
        unsafe { (self.api.window_flags)(self.window) & 0x200 != 0 }
    }
    pub fn title(&self, title: &str) {
        let value = CString::new(title.replace('\0', " ")).unwrap();
        unsafe {
            (self.api.window_title)(self.window, value.as_ptr());
        }
    }
    pub fn poll(&mut self) -> Option<Event> {
        loop {
            let mut raw = RawEvent { padding: [0; 7] };
            if unsafe { (self.api.poll)(&mut raw) } == 0 {
                return None;
            }
            match unsafe { raw.kind } {
                0x100 => return Some(Event::Quit),
                0x300 | 0x301 => {
                    let key = unsafe { raw.key };
                    return Some(Event::Key {
                        scancode: key.keysym.scancode,
                        pressed: key.kind == 0x300,
                        repeat: key.repeat != 0,
                    });
                }
                0x200 => match unsafe { raw.window.event } {
                    12 => return Some(Event::Focus(true)),
                    13 => return Some(Event::Focus(false)),
                    14 => return Some(Event::Quit),
                    _ => {}
                },
                _ => {}
            }
        }
    }
    pub fn render(&self, rgb: &[u16]) -> Result<(), String> {
        if rgb.len() != 320 * 240 {
            return Err("picture must be 320x240 RGB565".into());
        }
        unsafe {
            self.check(
                (self.api.update_texture)(self.texture, ptr::null(), rgb.as_ptr().cast(), 320 * 2),
                "SDL_UpdateTexture",
            )?;
        }
        self.draw()?;
        unsafe {
            (self.api.present)(self.renderer);
        }
        Ok(())
    }
    fn draw(&self) -> Result<(), String> {
        unsafe {
            self.check(
                (self.api.draw_color)(self.renderer, 0, 0, 0, 255),
                "SDL_SetRenderDrawColor",
            )?;
            self.check((self.api.clear)(self.renderer), "SDL_RenderClear")?;
            self.check(
                (self.api.copy)(self.renderer, self.texture, ptr::null(), ptr::null()),
                "SDL_RenderCopy",
            )
        }
    }
    pub fn queued_frames(&self) -> usize {
        if self.audio == 0 {
            0
        } else {
            unsafe { (self.api.audio_size)(self.audio) as usize / 4 }
        }
    }
    pub fn audio(&self, pcm: &[i16]) -> Result<(), String> {
        if self.audio == 0 {
            return Err("audio device was disabled".into());
        }
        if !pcm.len().is_multiple_of(2) {
            return Err("stereo PCM must contain complete pairs".into());
        }
        if pcm.len() / 2 > 9600usize.saturating_sub(self.queued_frames()) {
            return Err("SDL audio queue would exceed 200ms".into());
        }
        // SDL_QueueAudio copies these native-endian signed samples before
        // returning; the core's frame buffer stays owned by the emulator.
        unsafe {
            self.check(
                (self.api.queue_audio)(self.audio, pcm.as_ptr().cast(), (pcm.len() * 2) as u32),
                "SDL_QueueAudio",
            )
        }
    }
    pub fn clear_audio(&self) {
        if self.audio != 0 {
            unsafe {
                (self.api.clear_audio)(self.audio);
            }
        }
    }
    pub fn toggle_fullscreen(&mut self) -> Result<(), String> {
        let next = !self.fullscreen;
        unsafe {
            self.check(
                (self.api.fullscreen)(self.window, if next { 0x1001 } else { 0 }),
                "SDL_SetWindowFullscreen",
            )?;
        }
        self.fullscreen = next;
        Ok(())
    }
    /// Stable device identities keep a connected pad in its player slot.
    pub fn controllers(&mut self) -> [u16; 2] {
        for slot in &mut self.controllers {
            if !slot.1.is_null() && unsafe { (self.api.controller_attached)(slot.1) } == 0 {
                unsafe {
                    (self.api.close_controller)(slot.1);
                }
                *slot = (-1, ptr::null_mut());
            }
        }
        let count = unsafe { (self.api.num_joysticks)() }.max(0);
        for index in 0..count {
            if self.controllers.iter().all(|p| !p.1.is_null()) {
                break;
            }
            if unsafe { (self.api.is_controller)(index) } == 0 {
                continue;
            }
            let pad = unsafe { (self.api.open_controller)(index) };
            if pad.is_null() {
                continue;
            }
            let id = unsafe { (self.api.joystick_id)((self.api.controller_joystick)(pad)) };
            if self
                .controllers
                .iter()
                .any(|&(old, p)| !p.is_null() && old == id)
            {
                unsafe {
                    (self.api.close_controller)(pad);
                }
            } else if let Some(slot) = self.controllers.iter_mut().find(|p| p.1.is_null()) {
                *slot = (id, pad);
            }
        }
        unsafe {
            (self.api.controller_update)();
        }
        let mut pads = [0; 2];
        for (slot, &(_, pad)) in self.controllers.iter().enumerate() {
            if pad.is_null() {
                continue;
            }
            let mut bits = 0;
            for button in 0..15 {
                if unsafe { (self.api.controller_button)(pad, button) } != 0 {
                    bits |= 1 << button;
                }
            }
            let (x, y) = unsafe {
                (
                    (self.api.controller_axis)(pad, 0),
                    (self.api.controller_axis)(pad, 1),
                )
            };
            pads[slot] = super::desktop_input::controller(bits, x, y);
        }
        pads
    }
    pub fn pause_audio(&self, paused: bool) {
        if self.audio != 0 {
            unsafe {
                (self.api.pause_audio)(self.audio, c_int::from(paused));
            }
        }
    }

    /// Diagnostic snapshot from the actual renderer, before Present invalidates
    /// the back buffer. Bounded to 4096x4096; this is not the guest framebuffer.
    pub fn readback(&self) -> Result<(u32, u32, Vec<u8>), String> {
        let (mut w, mut h) = (0, 0);
        unsafe {
            self.check(
                (self.api.output_size)(self.renderer, &mut w, &mut h),
                "SDL_GetRendererOutputSize",
            )?;
        }
        if !(1..=4096).contains(&w) || !(1..=4096).contains(&h) {
            return Err("renderer readback exceeds 4096x4096".into());
        }
        let mut rgba = vec![0; w as usize * h as usize * 4];
        self.draw()?;
        unsafe {
            self.check(
                (self.api.read_pixels)(
                    self.renderer,
                    ptr::null(),
                    RGBA32,
                    rgba.as_mut_ptr().cast(),
                    w * 4,
                ),
                "SDL_RenderReadPixels",
            )?;
            (self.api.present)(self.renderer);
        }
        Ok((w as u32, h as u32, rgba))
    }
    #[cfg(test)]
    pub fn inject_key(&self, scancode: i32, pressed: bool, repeat: bool) -> Result<(), String> {
        let mut raw = RawEvent { padding: [0; 7] };
        raw.key = KeyEvent {
            kind: if pressed { 0x300 } else { 0x301 },
            state: u8::from(pressed),
            repeat: u8::from(repeat),
            keysym: Keysym {
                scancode,
                ..Keysym::default()
            },
            ..KeyEvent::default()
        };
        let push = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut RawEvent) -> c_int>(
                self.api._library.symbol(c"SDL_PushEvent")?,
            )
        };
        unsafe { self.check(push(&mut raw) - 1, "SDL_PushEvent") }
    }
    #[cfg(test)]
    pub fn abi() -> [usize; 6] {
        [
            std::mem::size_of::<AudioSpec>(),
            std::mem::size_of::<RawEvent>(),
            std::mem::align_of::<RawEvent>(),
            std::mem::size_of::<KeyEvent>(),
            std::mem::offset_of!(KeyEvent, keysym),
            std::mem::size_of::<WindowEvent>(),
        ]
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            for &(_, pad) in &self.controllers {
                if !pad.is_null() {
                    (self.api.close_controller)(pad);
                }
            }
            if self.audio != 0 {
                (self.api.close_audio)(self.audio);
            }
            if !self.texture.is_null() {
                (self.api.destroy_texture)(self.texture);
            }
            if !self.renderer.is_null() {
                (self.api.destroy_renderer)(self.renderer);
            }
            if !self.window.is_null() {
                (self.api.destroy_window)(self.window);
            }
            if self.initialized {
                (self.api.quit)();
            }
        }
        HOST_IN_USE.store(false, Ordering::Release);
    }
}
