//! The console a cartridge runs on, and its functions as Lua sees them:
//! PICO-8's API, from `add` to `yield`. Each takes the machine and its
//! arguments; numbers are 16.16 and floored where PICO-8 wants whole ones.
//!
//! PICO-8 is forgiving and so is this: a missing argument takes its
//! default, and what the console has no part for (the keyboard, the mouse,
//! other cartridges, files) does nothing, rather than stop the game.

use crate::audio::Audio;
use crate::fix::{self, ONE};
use crate::graphics::{self as g, Ram};
use crate::heap::{Heap, Status, Table, Thread, Value};
use crate::vm::{Ctl, Ret, Vm};

pub type Native = fn(&mut Vm, &[Value]) -> Result<Ret, Ctl>;

/// The console's hardware and what the host gives it.
pub struct Machine {
    pub ram: Box<Ram>,
    /// The cartridge as it was loaded (reload and cstore use it).
    pub rom: Box<[u8; 0x4300]>,
    pub audio: Audio,
    pub buttons: [u8; 8],
    pub pressed: [u8; 8],
    held: [[u16; 6]; 8],
    pub fps: u32,
    /// Frames the game ran, for time().
    pub frames: u64,
    pub cartdata: Option<String>,
    pub persistent_loaded: bool,
    pub menu: [Option<(String, u8)>; 5],
    /// The menu item whose callback runs now (1..5), for menuitem(nil, …).
    pub menu_active: Option<usize>,
    pub debug: Vec<String>,
    /// The time of day, seconds since 1970, for stat(80..95).
    pub clock: u64,
}

impl Machine {
    pub fn new(rom: Box<[u8; 0x4300]>) -> Machine {
        let mut ram: Box<Ram> = vec![0u8; 0x10000].into_boxed_slice().try_into().unwrap();
        ram[..0x4300].copy_from_slice(&rom[..]);
        g::reset(&mut ram);
        let mut m = Machine {
            ram,
            rom,
            audio: Audio::new(),
            buttons: [0; 8],
            pressed: [0; 8],
            held: [[0; 6]; 8],
            fps: 30,
            frames: 0,
            cartdata: None,
            persistent_loaded: false,
            menu: Default::default(),
            menu_active: None,
            debug: Vec::new(),
            clock: 0,
        };
        srand(&mut m.ram, 0);
        m
    }

    /// The buttons held this frame, by player; btnp's repeats follow.
    pub fn input(&mut self, buttons: [u8; 8]) {
        let scale = self.fps / 30;
        let (delay, period) = (self.ram[0x5f5c], self.ram[0x5f5d]);
        let delay = if delay == 0 { 15 } else { u16::from(delay) } * scale as u16;
        let period = if period == 0 { 4 } else { u16::from(period) } * scale as u16;
        for (p, b) in buttons.into_iter().enumerate() {
            // Button 6 is pause, which a cartridge sees only when it keeps
            // the pause menu shut (0x5f30).
            self.buttons[p] = b & 0x7f;
            self.pressed[p] = 0;
            for i in 0..6 {
                let t = &mut self.held[p][i];
                *t = if b & (1 << i) != 0 { t.saturating_add(1) } else { 0 };
                if *t == 1 || (self.ram[0x5f5c] != 255 && *t > delay && (*t - 1 - delay).is_multiple_of(period.max(1))) {
                    self.pressed[p] |= 1 << i;
                }
            }
            if b & 0x40 != 0 {
                self.pressed[p] |= 0x40;
            }
            self.ram[0x5f4c + p] = self.buttons[p] & 0x3f;
        }
    }
}

impl Machine {
    pub fn save(&self, w: &mut crate::state::W) {
        w.0.extend_from_slice(&self.ram[..]);
        w.0.extend_from_slice(&self.rom[..]);
        self.audio.save(w);
        w.0.extend_from_slice(&self.buttons);
        w.0.extend_from_slice(&self.pressed);
        for p in &self.held {
            for &t in p {
                w.u16(t);
            }
        }
        w.u32(self.fps);
        w.u64(self.frames);
        w.bytes(self.cartdata.as_deref().unwrap_or("").as_bytes());
        w.bool(self.cartdata.is_some());
        w.bool(self.persistent_loaded);
        for m in &self.menu {
            w.bool(m.is_some());
            let (label, mask) = m.clone().unwrap_or_default();
            w.bytes(label.as_bytes());
            w.u8(mask);
        }
    }

    pub fn load(&mut self, r: &mut crate::state::R) -> crate::state::Res<()> {
        self.ram.copy_from_slice(r.take(0x10000)?);
        self.rom.copy_from_slice(r.take(0x4300)?);
        self.audio = Audio::load(r)?;
        self.buttons.copy_from_slice(r.take(8)?);
        self.pressed.copy_from_slice(r.take(8)?);
        for p in &mut self.held {
            for t in p {
                *t = r.u16()?;
            }
        }
        self.fps = if r.u32()? == 60 { 60 } else { 30 };
        self.frames = r.u64()?;
        let id = String::from_utf8_lossy(r.bytes()?).into_owned();
        self.cartdata = r.bool()?.then_some(id);
        self.persistent_loaded = r.bool()?;
        for m in &mut self.menu {
            let some = r.bool()?;
            let label = String::from_utf8_lossy(r.bytes()?).into_owned();
            let mask = r.u8()?;
            *m = some.then_some((label, mask));
        }
        Ok(())
    }
}

fn rng(ram: &mut Ram) -> u32 {
    let mut hi = u32::from_le_bytes(ram[0x5f44..0x5f48].try_into().unwrap());
    let mut lo = u32::from_le_bytes(ram[0x5f48..0x5f4c].try_into().unwrap());
    hi = hi.rotate_left(16).wrapping_add(lo);
    lo = lo.wrapping_add(hi);
    ram[0x5f44..0x5f48].copy_from_slice(&hi.to_le_bytes());
    ram[0x5f48..0x5f4c].copy_from_slice(&lo.to_le_bytes());
    hi
}

/// srand, as zepto8 has PICO-8's: the seed without its top bit.
pub fn srand(ram: &mut Ram, seed: i32) {
    let seed = seed & 0x7fff_ffff;
    let lo = if seed == 0 { 0xdead_beef } else { seed as u32 };
    let hi = lo ^ 0xbead_29ba;
    ram[0x5f44..0x5f48].copy_from_slice(&hi.to_le_bytes());
    ram[0x5f48..0x5f4c].copy_from_slice(&lo.to_le_bytes());
    for _ in 0..32 {
        rng(ram);
    }
}

// Arguments.

fn num(vm: &Vm, a: &[Value], i: usize) -> Option<i32> {
    a.get(i).and_then(|&v| vm.number(v))
}
fn int(vm: &Vm, a: &[Value], i: usize, d: i32) -> i32 {
    num(vm, a, i).map_or(d, fix::floor)
}
fn fx(vm: &Vm, a: &[Value], i: usize, d: i32) -> i32 {
    num(vm, a, i).unwrap_or(d)
}
fn opt_int(vm: &Vm, a: &[Value], i: usize) -> Option<i32> {
    num(vm, a, i).map(fix::floor)
}
fn arg(a: &[Value], i: usize) -> Value {
    a.get(i).copied().unwrap_or_default()
}
fn truthy(a: &[Value], i: usize) -> bool {
    arg(a, i).truthy()
}
fn ok() -> Result<Ret, Ctl> {
    Ok(Ret::None)
}
fn one(v: Value) -> Result<Ret, Ctl> {
    Ok(Ret::One(v))
}
fn int_ret(i: i32) -> Result<Ret, Ctl> {
    one(Value::int(i))
}
fn many(vm: &mut Vm, vals: &[Value]) -> Result<Ret, Ctl> {
    vm.ret.clear();
    vm.ret.extend_from_slice(vals);
    Ok(Ret::Many)
}
fn table(a: &[Value], i: usize) -> Option<u32> {
    match arg(a, i) {
        Value::Table(t) => Some(t),
        _ => None,
    }
}

/// The text of a value as tostr() and print() give it, with __tostring.
pub fn tostr(vm: &mut Vm, v: Value) -> Result<Vec<u8>, Ctl> {
    if let Value::Table(t) = v
        && let Some(m) = vm.heap.tab(t).meta
    {
        let h = vm.heap.tab(m).get(vm.names.tostring);
        if h != Value::Nil {
            let r = vm.call(h, &[v])?;
            let r = r.first().copied().unwrap_or_default();
            return Ok(vm.text(r));
        }
    }
    Ok(vm.text(v))
}

/// A string argument (a number becomes its text).
fn bytes(vm: &Vm, a: &[Value], i: usize) -> Option<Vec<u8>> {
    match arg(a, i) {
        Value::Str(s) => Some(vm.heap.str(s).to_vec()),
        Value::Num(n) => Some(fix::to_string(n).into_bytes()),
        _ => None,
    }
}

// Lua's own.

/// all()'s iterator, from its state {table, index} and the last value:
/// moves on unless the last value was taken out (del) under it.
pub fn all_next(heap: &mut Heap, state: u32, last: Value) -> Value {
    let st = heap.tab(state);
    let (Some(&Value::Table(t)), Some(&Value::Num(i))) = (st.arr.first(), st.arr.get(1)) else { return Value::Nil };
    let mut i = fix::floor(i).max(1) as usize;
    let tab = heap.tab(t);
    let n = tab.len();
    if last != Value::Nil && tab.get(Value::int(i as i32)) == last {
        i += 1;
    }
    while i <= n && tab.get(Value::int(i as i32)) == Value::Nil {
        i += 1;
    }
    let v = if i <= n { tab.get(Value::int(i as i32)) } else { Value::Nil };
    heap.tab_mut(state).arr[1] = Value::int(i as i32);
    v
}

fn next(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let Some(t) = table(a, 0) else { return Err("bad argument to 'next' (table expected)".into()) };
    match vm.heap.tab(t).next(arg(a, 1)) {
        Ok(Some((k, v))) => Ok(Ret::Two(k, v)),
        Ok(None) => one(Value::Nil),
        Err(()) => Err("invalid key to 'next'".into()),
    }
}

fn inext(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let i = int(vm, a, 1, 0) + 1;
    let v = match arg(a, 0) {
        Value::Table(t) => vm.heap.tab(t).get(Value::int(i)),
        _ => Value::Nil,
    };
    if v == Value::Nil { one(Value::Nil) } else { Ok(Ret::Two(Value::int(i), v)) }
}

fn all_native(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let Some(s) = table(a, 0) else { return one(Value::Nil) };
    one(all_next(&mut vm.heap, s, arg(a, 1)))
}

fn pairs(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let t = arg(a, 0);
    if let Value::Table(id) = t
        && let Some(m) = vm.heap.tab(id).meta
    {
        let h = vm.heap.tab(m).get(vm.names.pairs);
        if h != Value::Nil {
            let r = vm.call(h, &[t])?;
            let r: Vec<Value> = (0..3).map(|i| r.get(i).copied().unwrap_or_default()).collect();
            return many(vm, &r);
        }
    }
    let next = Value::Native(vm.names.next);
    if !matches!(t, Value::Table(_)) {
        // pairs(nil) loops over nothing.
        let empty = vm.heap.table(Table::default());
        return many(vm, &[next, empty, Value::Nil]);
    }
    many(vm, &[next, t, Value::Nil])
}

fn ipairs(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let f = Value::Native(vm.names.inext);
    many(vm, &[f, arg(a, 0), Value::int(0)])
}

fn all(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let mut st = Table::default();
    st.arr = vec![arg(a, 0), Value::int(1)];
    let s = vm.heap.table(st);
    let f = Value::Native(vm.names.all_next);
    many(vm, &[f, s, Value::Nil])
}

fn foreach(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (t, f) = (arg(a, 0), arg(a, 1));
    let Value::Table(_) = t else { return ok() };
    let mut st = Table::default();
    st.arr = vec![t, Value::int(1)];
    let Value::Table(s) = vm.heap.table(st) else { unreachable!() };
    // The state stays reachable for the collector through this argument.
    vm.registry.push(Value::Table(s));
    let mut last = Value::Nil;
    let r = loop {
        let v = all_next(&mut vm.heap, s, last);
        if v == Value::Nil {
            break Ok(());
        }
        if let Err(e) = vm.call(f, &[v]) {
            break Err(e);
        }
        last = v;
    };
    vm.registry.pop();
    r?;
    ok()
}

fn count(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let Some(t) = table(a, 0) else { return int_ret(0) };
    let tab = vm.heap.tab(t);
    if a.len() < 2 {
        return int_ret(tab.arr.iter().filter(|&&v| v != Value::Nil).count() as i32);
    }
    let v = arg(a, 1);
    int_ret(tab.arr.iter().filter(|&&x| x == v).count() as i32)
}

fn add(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let Some(t) = table(a, 0) else { return one(Value::Nil) };
    let v = arg(a, 1);
    match opt_int(vm, a, 2) {
        Some(i) => vm.heap.tab_mut(t).insert_at(i.max(1) as usize, v),
        None => {
            let n = vm.heap.tab(t).len();
            vm.heap.tab_mut(t).set(Value::int(n as i32 + 1), v);
        }
    }
    one(v)
}

fn del(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let Some(t) = table(a, 0) else { return one(Value::Nil) };
    let v = arg(a, 1);
    let found = vm.heap.tab(t).arr.iter().position(|&x| x == v);
    match found {
        Some(i) => {
            vm.heap.tab_mut(t).remove_at(i + 1);
            one(v)
        }
        None => one(Value::Nil),
    }
}

fn deli(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let Some(t) = table(a, 0) else { return one(Value::Nil) };
    let n = vm.heap.tab(t).len() as i32;
    // The index kept within 1..#t, as PICO-8 does: deli(t, 99) takes the last.
    let i = int(vm, a, 1, n).clamp(1, n.max(1));
    one(vm.heap.tab_mut(t).remove_at(i as usize))
}

fn type_(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let name: &[u8] = match arg(a, 0) {
        Value::Nil => b"nil",
        Value::Bool(_) => b"boolean",
        Value::Num(_) => b"number",
        Value::Str(_) => b"string",
        Value::Table(_) => b"table",
        Value::Func(_) | Value::Native(_) => b"function",
        Value::Thread(_) => b"thread",
        Value::Cell(_) => b"userdata",
    };
    if a.is_empty() {
        return Err("bad argument #1 to 'type' (value expected)".into());
    }
    let s = vm.str(name);
    one(s)
}

fn tostr_native(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    if a.is_empty() {
        let s = vm.str(b"[no value]");
        return one(s);
    }
    let v = arg(a, 0);
    let flags = match arg(a, 1) {
        Value::Bool(true) => 1,
        Value::Num(n) => fix::floor(n),
        _ => 0,
    };
    let text = match v {
        Value::Num(n) if flags & 1 != 0 => {
            if flags & 2 != 0 {
                format!("0x{:08x}", n as u32).into_bytes()
            } else {
                fix::to_hex(n).into_bytes()
            }
        }
        Value::Num(n) if flags & 2 != 0 => n.to_string().into_bytes(),
        _ => tostr(vm, v)?,
    };
    let s = vm.str(&text);
    one(s)
}

fn tonum(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let flags = int(vm, a, 1, 0);
    let fail = || if flags & 4 != 0 { one(Value::Num(0)) } else { Ok(Ret::None) };
    match arg(a, 0) {
        Value::Num(n) => one(Value::Num(n)),
        Value::Bool(b) => one(Value::int(b as i32)),
        Value::Str(s) => {
            let text = vm.heap.str(s).to_vec();
            let parsed = if flags & 1 != 0 {
                let mut t = b"0x".to_vec();
                t.extend_from_slice(&text);
                fix::parse(&t)
            } else {
                fix::parse(&text)
            };
            match parsed {
                Some(n) if flags & 2 != 0 => one(Value::Num(n >> 16)),
                Some(n) => one(Value::Num(n)),
                None => fail(),
            }
        }
        _ => fail(),
    }
}

fn chr(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let s: Vec<u8> = (0..a.len()).map(|i| int(vm, a, i, 0) as u8).collect();
    let v = vm.str(&s);
    one(v)
}

fn ord(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (i, n) = (int(vm, a, 1, 1), int(vm, a, 2, 1).clamp(0, 256));
    // The common case, one byte of a string, without copying it.
    if let (Value::Str(id), 1) = (arg(a, 0), n) {
        let s = vm.heap.str(id);
        return one(if i >= 1 && i as usize <= s.len() { Value::int(i32::from(s[i as usize - 1])) } else { Value::Nil });
    }
    let Some(s) = bytes(vm, a, 0) else { return ok() };
    let vals: Vec<Value> = (0..n)
        .map(|k| {
            let at = i + k;
            if at >= 1 && at as usize <= s.len() { Value::int(i32::from(s[at as usize - 1])) } else { Value::Nil }
        })
        .collect();
    if n == 1 {
        return one(vals[0]);
    }
    many(vm, &vals)
}

fn sub(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let owned;
    let s: &[u8] = match arg(a, 0) {
        Value::Str(id) => vm.heap.str(id),
        _ => match bytes(vm, a, 0) {
            Some(b) => {
                owned = b;
                &owned
            }
            None => return one(Value::Nil),
        },
    };
    let len = s.len() as i32;
    let fix_i = |i: i32| if i < 0 { len + i + 1 } else { i };
    let i = fix_i(int(vm, a, 1, 1)).max(1);
    let j = match arg(a, 2) {
        Value::Nil => len,
        _ => fix_i(int(vm, a, 2, -1)).min(len),
    };
    let piece: Vec<u8> = if i > j { Vec::new() } else { s[(i - 1) as usize..j as usize].to_vec() };
    let v = vm.str(&piece);
    one(v)
}

fn split(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let Some(s) = bytes(vm, a, 0) else { return one(Value::Nil) };
    let convert = a.len() < 3 || truthy(a, 2);
    let pieces: Vec<Vec<u8>> = match arg(a, 1) {
        Value::Num(n) => {
            let n = fix::floor(n).max(1) as usize;
            s.chunks(n).map(<[u8]>::to_vec).collect()
        }
        Value::Str(sep) => {
            let sep = vm.heap.str(sep).to_vec();
            if sep.is_empty() {
                s.iter().map(|&c| vec![c]).collect()
            } else {
                let mut out = Vec::new();
                let mut start = 0;
                let mut i = 0;
                while i + sep.len() <= s.len() {
                    if s[i..i + sep.len()] == sep[..] {
                        out.push(s[start..i].to_vec());
                        i += sep.len();
                        start = i;
                    } else {
                        i += 1;
                    }
                }
                out.push(s[start..].to_vec());
                out
            }
        }
        _ => {
            let mut out: Vec<Vec<u8>> = s.split(|&c| c == b',').map(<[u8]>::to_vec).collect();
            if s.is_empty() {
                out = vec![Vec::new()];
            }
            out
        }
    };
    let mut t = Table::with_capacity(pieces.len(), 0);
    for (i, p) in pieces.iter().enumerate() {
        let v = match (convert, fix::parse(p)) {
            (true, Some(n)) if !p.is_empty() => Value::Num(n),
            _ => vm.heap.intern(p),
        };
        t.set(Value::int(i as i32 + 1), v);
    }
    one(vm.heap.table(t))
}

fn select(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let rest = &a[a.len().min(1)..];
    if let Value::Str(s) = arg(a, 0)
        && vm.heap.str(s) == b"#"
    {
        return int_ret(rest.len() as i32);
    }
    let n = int(vm, a, 0, 1);
    let from = if n < 0 { (rest.len() as i32 + n).max(0) as usize } else { (n.max(1) - 1) as usize };
    let vals = rest.get(from..).unwrap_or(&[]).to_vec();
    many(vm, &vals)
}

fn unpack(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let Some(t) = table(a, 0) else { return ok() };
    let n = vm.heap.tab(t).len() as i32;
    let i = int(vm, a, 1, 1);
    let j = int(vm, a, 2, n);
    if j - i > 8000 {
        return Err("too many results to unpack".into());
    }
    let vals: Vec<Value> = (i..=j).map(|k| vm.heap.tab(t).get(Value::int(k))).collect();
    many(vm, &vals)
}

fn pack(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let mut t = Table::with_capacity(a.len(), 1);
    for (i, &v) in a.iter().enumerate() {
        t.set(Value::int(i as i32 + 1), v);
    }
    let n = vm.str(b"n");
    t.set(n, Value::int(a.len() as i32));
    one(vm.heap.table(t))
}

fn rawget(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let Some(t) = table(a, 0) else { return Err("bad argument to 'rawget' (table expected)".into()) };
    one(vm.heap.tab(t).get(arg(a, 1)))
}
fn rawset(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let Some(t) = table(a, 0) else { return Err("bad argument to 'rawset' (table expected)".into()) };
    if arg(a, 1) == Value::Nil {
        return Err("table index is nil".into());
    }
    vm.heap.tab_mut(t).set(arg(a, 1), arg(a, 2));
    one(arg(a, 0))
}
fn rawequal(_: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    one(Value::Bool(arg(a, 0) == arg(a, 1)))
}
fn rawlen(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    match arg(a, 0) {
        Value::Table(t) => int_ret(vm.heap.tab(t).len() as i32),
        Value::Str(s) => int_ret(vm.heap.str(s).len() as i32),
        _ => int_ret(0),
    }
}
fn setmetatable(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let Some(t) = table(a, 0) else { return Err("bad argument to 'setmetatable' (table expected)".into()) };
    vm.heap.tab_mut(t).meta = table(a, 1);
    one(arg(a, 0))
}
fn getmetatable(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    match table(a, 0).and_then(|t| vm.heap.tab(t).meta) {
        Some(m) => one(Value::Table(m)),
        None => one(Value::Nil),
    }
}
fn assert(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    if !truthy(a, 0) {
        let msg = match arg(a, 1) {
            Value::Nil => "assertion failed!".to_string(),
            v => String::from_utf8_lossy(&vm.text(v)).into_owned(),
        };
        return Err(Ctl::Error(msg));
    }
    many(vm, a)
}
fn error(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    Err(Ctl::Error(String::from_utf8_lossy(&vm.text(arg(a, 0))).into_owned()))
}

fn cocreate(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let f = arg(a, 0);
    if !matches!(f, Value::Func(_) | Value::Native(_)) {
        return Err("bad argument to 'cocreate' (function expected)".into());
    }
    one(Value::Thread(vm.heap.thread(Thread::new(Some(f)))))
}
fn coresume(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let Value::Thread(id) = arg(a, 0) else { return Err("bad argument to 'coresume' (coroutine expected)".into()) };
    match vm.resume(id, &a[1..]) {
        Ok(vals) => {
            let mut out = vec![Value::Bool(true)];
            out.extend(vals);
            many(vm, &out)
        }
        Err(e) => {
            let s = vm.str(e.as_bytes());
            Ok(Ret::Two(Value::Bool(false), s))
        }
    }
}
fn costatus(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let Value::Thread(id) = arg(a, 0) else { return Err("bad argument to 'costatus' (coroutine expected)".into()) };
    let status = if vm.co == Some(id) {
        Status::Running
    } else {
        vm.heap.threads.items[id as usize].as_ref().map_or(Status::Normal, |t| t.status)
    };
    let s: &[u8] = match status {
        Status::Suspended => b"suspended",
        Status::Running => b"running",
        Status::Normal => b"normal",
        Status::Dead => b"dead",
    };
    let v = vm.str(s);
    one(v)
}
fn yield_(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    vm.ret.clear();
    vm.ret.extend_from_slice(a);
    Err(Ctl::Yield)
}

// The console.

fn print(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let s = tostr(vm, arg(a, 0))?;
    let ram = &mut vm.m.ram;
    if a.len() <= 2 {
        let col = if a.len() == 2 { opt_int_raw(&vm.heap, a, 1) } else { None };
        let col = g::pen(ram, col);
        let (x, y) = g::print_at_cursor(ram, &s, col);
        return Ok(Ret::Two(Value::int(x), Value::int(y)));
    }
    let (x, y) = (int_h(&vm.heap, a, 1, 0), int_h(&vm.heap, a, 2, 0));
    let col = g::pen(ram, opt_int_raw(&vm.heap, a, 3));
    let (end, next_y) = g::print(ram, &s, x, y, col);
    ram[0x5f26] = x as u8;
    ram[0x5f27] = next_y.clamp(0, 255) as u8;
    // The x after the text and the y of the line after it.
    Ok(Ret::Two(Value::int(end), Value::int(next_y)))
}

// Argument helpers for when the machine is borrowed apart from the heap.
fn num_h(h: &Heap, a: &[Value], i: usize) -> Option<i32> {
    match arg(a, i) {
        Value::Num(n) => Some(n),
        Value::Str(s) => fix::parse(h.str(s)),
        _ => None,
    }
}
fn int_h(h: &Heap, a: &[Value], i: usize, d: i32) -> i32 {
    num_h(h, a, i).map_or(d, fix::floor)
}
fn opt_int_raw(h: &Heap, a: &[Value], i: usize) -> Option<i32> {
    num_h(h, a, i).map(fix::floor)
}

fn printh(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let s = tostr(vm, arg(a, 0))?;
    if vm.m.debug.len() < 1000 {
        vm.m.debug.push(String::from_utf8_lossy(&s).into_owned());
    }
    ok()
}

fn stop(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let s = match arg(a, 0) {
        Value::Nil => Vec::new(),
        v => tostr(vm, v)?,
    };
    Err(Ctl::Error(format!("\0stop{}", String::from_utf8_lossy(&s))))
}
fn run(_: &mut Vm, _: &[Value]) -> Result<Ret, Ctl> {
    Err(Ctl::Error("\0run".into()))
}
fn reset(vm: &mut Vm, _: &[Value]) -> Result<Ret, Ctl> {
    g::reset(&mut vm.m.ram);
    ok()
}
fn flip(_: &mut Vm, _: &[Value]) -> Result<Ret, Ctl> {
    Err(Ctl::Flip)
}
fn nothing(_: &mut Vm, _: &[Value]) -> Result<Ret, Ctl> {
    ok()
}
/// serial(0x808, addr, len): 8-bit PCM to the sound, 5512 Hz.
fn serial(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (ch, addr, len) = (int(vm, a, 0, 0), int(vm, a, 1, 0) as u16 as usize, int(vm, a, 2, 0).max(0) as usize);
    if ch == 0x808 {
        let bytes: Vec<u8> = (0..len.min(0x10000)).map(|i| vm.m.ram[(addr + i) & 0xffff]).collect();
        vm.m.audio.queue_pcm(&bytes);
        return int_ret(len as i32);
    }
    int_ret(0)
}
fn set_fps(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    vm.m.fps = if int(vm, a, 0, 30) >= 60 { 60 } else { 30 };
    ok()
}
fn time(vm: &mut Vm, _: &[Value]) -> Result<Ret, Ctl> {
    let fps = u64::from(vm.m.fps.max(1));
    let f = vm.m.frames;
    one(Value::Num(((f * 65536) / fps) as i32))
}

fn stat(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let n = int(vm, a, 0, 0);
    let audio = &vm.m.audio;
    let v = match n {
        0 => Value::Num(((vm.heap.size() as u64 * 65536) / 1024) as i32),
        1 | 2 => Value::Num(fix::from_f64(vm.cpu())),
        4 | 6 => vm.str(b""),
        5 => Value::int(41),
        7..=9 => Value::int(vm.m.fps as i32),
        11 => Value::int(1),
        16..=19 => Value::int(audio.channel_sfx((n - 16) as usize)),
        20..=23 => Value::int(audio.channel_note((n - 20) as usize)),
        24 => Value::int(audio.pattern()),
        25 => Value::int(audio.patterns_played()),
        26 => Value::int(audio.music_ticks()),
        46..=49 => Value::int(audio.channel_sfx((n - 46) as usize)),
        50..=53 => Value::int(audio.channel_note((n - 50) as usize)),
        54 => Value::int(audio.pattern()),
        55 => Value::int(audio.patterns_played()),
        56 => Value::int(audio.music_ticks()),
        57 => Value::Bool(audio.pattern() >= 0),
        28 | 30 | 120 | 121 => Value::Bool(false),
        31 => vm.str(b""),
        80..=85 | 90..=95 => {
            let (y, mo, d, h, mi, s) = civil(vm.m.clock);
            Value::int([y, mo, d, h, mi, s][(n % 10) as usize])
        }
        100 | 101 => Value::Nil,
        102 => Value::Num(0),
        108 => Value::int(audio.pcm_queued() as i32),
        109 => Value::int(5512),
        _ => Value::Num(0),
    };
    one(v)
}

/// Seconds since 1970 to the year, month, day, hour, minute and second.
fn civil(t: u64) -> (i32, i32, i32, i32, i32, i32) {
    let days = (t / 86400) as i64;
    let secs = (t % 86400) as i32;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as i32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as i32;
    let y = (yoe + era * 400 + i64::from(m <= 2)) as i32;
    (y, m, d, secs / 3600, secs / 60 % 60, secs % 60)
}

fn btn_common(vm: &mut Vm, a: &[Value], pressed: bool) -> Result<Ret, Ctl> {
    let state = if pressed { vm.m.pressed } else { vm.m.buttons };
    if a.is_empty() || arg(a, 0) == Value::Nil {
        return int_ret(i32::from(state[0]) | i32::from(state[1]) << 8);
    }
    let b = int(vm, a, 0, 0);
    let p = int(vm, a, 1, 0);
    one(Value::Bool((0..7).contains(&b) && (0..8).contains(&p) && state[p as usize] & (1 << b) != 0))
}
fn btn(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    btn_common(vm, a, false)
}
fn btnp(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    btn_common(vm, a, true)
}

// Maths.

fn abs(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    one(Value::Num(fx(vm, a, 0, 0).wrapping_abs()))
}
fn flr(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    one(Value::Num(fx(vm, a, 0, 0) & !0xffff))
}
fn ceil(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    one(Value::Num(fx(vm, a, 0, 0).wrapping_neg() & !0xffff).map_num(i32::wrapping_neg))
}
fn sqrt(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let x = fx(vm, a, 0, 0);
    if x <= 0 {
        return one(Value::Num(0));
    }
    // The root of a 16.16 number is that of x << 16, read as 16.16.
    one(Value::Num(((i64::from(x) << 16) as f64).sqrt() as i32))
}
fn sgn(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    int_ret(if fx(vm, a, 0, 0) < 0 { -1 } else { 1 })
}
fn min(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    one(Value::Num(fx(vm, a, 0, 0).min(fx(vm, a, 1, 0))))
}
fn max(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    one(Value::Num(fx(vm, a, 0, 0).max(fx(vm, a, 1, 0))))
}
fn mid(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let mut v = [fx(vm, a, 0, 0), fx(vm, a, 1, 0), fx(vm, a, 2, 0)];
    v.sort_unstable();
    one(Value::Num(v[1]))
}
fn sin_fx(x: i32) -> i32 {
    fix::from_f64(-(fix::to_f64(x) * std::f64::consts::TAU).sin())
}
fn cos_fx(x: i32) -> i32 {
    fix::from_f64((fix::to_f64(x) * std::f64::consts::TAU).cos())
}
fn sin(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    one(Value::Num(sin_fx(fx(vm, a, 0, 0))))
}
fn cos(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    one(Value::Num(cos_fx(fx(vm, a, 0, 0))))
}
fn atan2(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (x, y) = (fix::to_f64(fx(vm, a, 0, 0)), fix::to_f64(fx(vm, a, 1, 0)));
    let mut r = 0.75 + x.atan2(y) / std::f64::consts::TAU;
    if r >= 1.0 {
        r -= 1.0;
    }
    one(Value::Num(fix::from_f64(r)))
}
fn rnd(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    if let Some(t) = table(a, 0) {
        let n = vm.heap.tab(t).len() as u32;
        if n == 0 {
            return one(Value::Nil);
        }
        // t[flr(rnd(#t)) + 1], as PICO-8 picks.
        let r = rng(&mut vm.m.ram) % (n << 16);
        return one(vm.heap.tab(t).get(Value::int((r >> 16) as i32 + 1)));
    }
    let x = fx(vm, a, 0, ONE) as u32;
    // rnd(0) is 0 and leaves the generator as it was.
    if x == 0 {
        return one(Value::Num(0));
    }
    let r = rng(&mut vm.m.ram);
    one(Value::Num((r % x) as i32))
}
fn srand_native(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let s = fx(vm, a, 0, 0);
    srand(&mut vm.m.ram, s);
    ok()
}
macro_rules! bitop {
    ($name:ident, $f:expr) => {
        fn $name(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
            let f: fn(i32, i32) -> i32 = $f;
            one(Value::Num(f(fx(vm, a, 0, 0), fx(vm, a, 1, 0))))
        }
    };
}
bitop!(band, |x, y| x & y);
bitop!(bor, |x, y| x | y);
bitop!(bxor, |x, y| x ^ y);
bitop!(bnot, |x, _| !x);
bitop!(shl, fix::shl);
bitop!(shr, fix::shr);
bitop!(lshr, fix::lshr);
bitop!(rotl, fix::rotl);
bitop!(rotr, fix::rotr);

trait MapNum {
    fn map_num(self, f: fn(i32) -> i32) -> Value;
}
impl MapNum for Value {
    fn map_num(self, f: fn(i32) -> i32) -> Value {
        match self {
            Value::Num(n) => Value::Num(f(n)),
            v => v,
        }
    }
}

// Memory.

fn peek_n(vm: &mut Vm, a: &[Value], width: usize) -> Result<Ret, Ctl> {
    let addr = int(vm, a, 0, 0) as u16 as usize;
    let n = int(vm, a, 1, 1).clamp(0, 8192) as usize;
    let ram = &vm.m.ram;
    let read = |at: usize| -> Value {
        let mut raw = 0u32;
        for b in 0..width {
            raw |= u32::from(ram[(at + b) & 0xffff]) << (8 * b);
        }
        match width {
            1 => Value::int(raw as i32),
            2 => Value::int(raw as u16 as i32),
            _ => Value::Num(raw as i32),
        }
    };
    if n == 1 {
        return one(read(addr));
    }
    let vals: Vec<Value> = (0..n).map(|i| read(addr + i * width)).collect();
    many(vm, &vals)
}
fn peek(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    peek_n(vm, a, 1)
}
fn peek2(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    peek_n(vm, a, 2)
}
fn peek4(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    peek_n(vm, a, 4)
}
fn poke_n(vm: &mut Vm, a: &[Value], width: usize) -> Result<Ret, Ctl> {
    let addr = int(vm, a, 0, 0) as u16 as usize;
    for i in 1..a.len().min(8193) {
        let v = fx(vm, a, i, 0);
        let raw = if width == 4 { v as u32 } else { fix::floor(v) as u32 };
        for b in 0..width {
            vm.m.ram[(addr + (i - 1) * width + b) & 0xffff] = (raw >> (8 * b)) as u8;
        }
    }
    ok()
}
fn poke(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    poke_n(vm, a, 1)
}
fn poke2(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    poke_n(vm, a, 2)
}
fn poke4(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    poke_n(vm, a, 4)
}
fn memcpy(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (dst, src, len) = (int(vm, a, 0, 0), int(vm, a, 1, 0), int(vm, a, 2, 0));
    if len <= 0 {
        return ok();
    }
    let (dst, src, len) = (dst as u16 as usize, src as u16 as usize, (len as usize).min(0x10000));
    let ram = &mut vm.m.ram;
    if dst + len <= 0x10000 && src + len <= 0x10000 {
        ram.copy_within(src..src + len, dst);
    } else {
        let tmp: Vec<u8> = (0..len).map(|i| ram[(src + i) & 0xffff]).collect();
        for (i, b) in tmp.into_iter().enumerate() {
            ram[(dst + i) & 0xffff] = b;
        }
    }
    ok()
}
fn memset(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (dst, val, len) = (int(vm, a, 0, 0) as u16 as usize, int(vm, a, 1, 0) as u8, int(vm, a, 2, 0));
    for i in 0..len.clamp(0, 0x10000) as usize {
        vm.m.ram[(dst + i) & 0xffff] = val;
    }
    ok()
}
fn reload(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    if arg(a, 3) != Value::Nil {
        // Another cartridge: there are none to read.
        return ok();
    }
    let (dst, src, len) = (int(vm, a, 0, 0) as u16 as usize, int(vm, a, 1, 0) as u16 as usize, int(vm, a, 2, 0x4300));
    for i in 0..len.clamp(0, 0x10000) as usize {
        let s = src + i;
        if s >= 0x4300 {
            break;
        }
        vm.m.ram[(dst + i) & 0xffff] = vm.m.rom[s];
    }
    ok()
}
fn cstore(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    if arg(a, 3) != Value::Nil {
        return ok();
    }
    let (dst, src, len) = (int(vm, a, 0, 0) as u16 as usize, int(vm, a, 1, 0) as u16 as usize, int(vm, a, 2, 0x4300));
    for i in 0..len.clamp(0, 0x10000) as usize {
        let d = dst + i;
        if d >= 0x4300 {
            break;
        }
        vm.m.rom[d] = vm.m.ram[(src + i) & 0xffff];
    }
    ok()
}

// Saved data and the pause menu.

fn cartdata(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let id = match arg(a, 0) {
        Value::Str(s) => String::from_utf8_lossy(vm.heap.str(s)).into_owned(),
        _ => String::new(),
    };
    if vm.m.cartdata.is_none() {
        vm.m.cartdata = Some(id);
    }
    one(Value::Bool(vm.m.persistent_loaded))
}
fn dget(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let i = int(vm, a, 0, 0);
    if !(0..64).contains(&i) {
        return int_ret(0);
    }
    let at = 0x5e00 + i as usize * 4;
    one(Value::Num(i32::from_le_bytes(vm.m.ram[at..at + 4].try_into().unwrap())))
}
fn dset(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let i = int(vm, a, 0, 0);
    if (0..64).contains(&i) {
        let at = 0x5e00 + i as usize * 4;
        let v = fx(vm, a, 1, 0);
        vm.m.ram[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    ok()
}
fn menuitem(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    // From inside an item's callback, a nil index is that item.
    let inside = vm.m.menu_active;
    let raw = match (arg(a, 0), inside) {
        (Value::Nil, Some(i)) => i as i32 | i32::from(vm.m.menu[i - 1].as_ref().map_or(0, |m| m.1)) << 8,
        _ => int(vm, a, 0, 0),
    };
    let i = (raw & 0xff) as usize;
    if !(1..=5).contains(&i) {
        return ok();
    }
    if vm.registry.len() < 5 {
        vm.registry.resize(5, Value::Nil);
    }
    match arg(a, 1) {
        Value::Nil => {
            vm.m.menu[i - 1] = None;
            vm.registry[i - 1] = Value::Nil;
        }
        v => {
            let label = String::from_utf8_lossy(&vm.text(v)).chars().take(16).collect();
            vm.m.menu[i - 1] = Some((label, (raw >> 8) as u8));
            // No callback: none, except from inside the item's own, which
            // keeps it.
            if a.len() > 2 || inside != Some(i) {
                vm.registry[i - 1] = arg(a, 2);
            }
        }
    }
    ok()
}

// Sound.

fn sfx(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (n, ch, off, len) = (int(vm, a, 0, -1), int(vm, a, 1, -1), int(vm, a, 2, 0), int(vm, a, 3, 0));
    vm.m.audio.sfx(n, ch, off, len);
    ok()
}
fn music(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (n, fade, mask) = (int(vm, a, 0, -1), int(vm, a, 1, 0), int(vm, a, 2, 0));
    vm.m.audio.music(&vm.m.ram, n, fade, mask);
    ok()
}

// Drawing.

/// The colour argument at i, which becomes the pen.
fn colour(vm: &mut Vm, a: &[Value], i: usize) -> u8 {
    let c = num(vm, a, i);
    g::pen_bits(&mut vm.m.ram, c)
}

fn cls(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let c = int(vm, a, 0, 0) as u8;
    g::cls(&mut vm.m.ram, c);
    ok()
}
fn pset(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (x, y) = (int(vm, a, 0, 0), int(vm, a, 1, 0));
    let c = colour(vm, a, 2);
    g::pset(&mut vm.m.ram, x, y, c);
    ok()
}
fn pget(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    int_ret(i32::from(g::pget(&vm.m.ram, int(vm, a, 0, 0), int(vm, a, 1, 0))))
}
fn sget(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    int_ret(i32::from(g::sget(&vm.m.ram, int(vm, a, 0, 0), int(vm, a, 1, 0))))
}
fn sset(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (x, y) = (int(vm, a, 0, 0), int(vm, a, 1, 0));
    let c = colour(vm, a, 2);
    g::sset(&mut vm.m.ram, x, y, c);
    ok()
}
fn fget(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let n = int(vm, a, 0, 0);
    let flags = if (0..256).contains(&n) { vm.m.ram[0x3000 + n as usize] } else { 0 };
    // A flag given, even as nil (flag 0), is one flag; none is all of them.
    if a.len() > 1 {
        let f = int(vm, a, 1, 0);
        return one(Value::Bool((0..8).contains(&f) && flags & (1 << f) != 0));
    }
    int_ret(i32::from(flags))
}
fn fset(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let n = int(vm, a, 0, 0);
    if !(0..256).contains(&n) {
        return ok();
    }
    let at = 0x3000 + n as usize;
    if a.len() >= 3 {
        let f = int(vm, a, 1, 0);
        if (0..8).contains(&f) {
            if truthy(a, 2) {
                vm.m.ram[at] |= 1 << f;
            } else {
                vm.m.ram[at] &= !(1 << f);
            }
        }
    } else {
        vm.m.ram[at] = int(vm, a, 1, 0) as u8;
    }
    ok()
}
fn mget(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    int_ret(i32::from(g::mget(&vm.m.ram, int(vm, a, 0, 0), int(vm, a, 1, 0))))
}
fn mset(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (x, y, v) = (int(vm, a, 0, 0), int(vm, a, 1, 0), int(vm, a, 2, 0));
    g::mset(&mut vm.m.ram, x, y, v as u8);
    ok()
}
fn line(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    match a.len() {
        // line() or line(c): the next line(x, y) starts a polyline.
        0 | 1 => {
            if a.len() == 1 {
                colour(vm, a, 0);
            }
            g::line_reset(&mut vm.m.ram)
        }
        2 | 3 => {
            let (x, y) = (int(vm, a, 0, 0), int(vm, a, 1, 0));
            let c = colour(vm, a, 2);
            match g::line_last(&vm.m.ram) {
                Some((x0, y0)) => g::line(&mut vm.m.ram, x0, y0, x, y, c),
                // The first point only: the next line starts there.
                None => g::line_start(&mut vm.m.ram, x, y),
            }
        }
        _ => {
            let (x0, y0, x1, y1) = (int(vm, a, 0, 0), int(vm, a, 1, 0), int(vm, a, 2, 0), int(vm, a, 3, 0));
            let c = colour(vm, a, 4);
            g::line(&mut vm.m.ram, x0, y0, x1, y1, c);
        }
    }
    ok()
}
fn rect_common(vm: &mut Vm, a: &[Value], fill: bool) -> Result<Ret, Ctl> {
    let (x0, y0, x1, y1) = (int(vm, a, 0, 0), int(vm, a, 1, 0), int(vm, a, 2, 0), int(vm, a, 3, 0));
    let c = colour(vm, a, 4);
    g::rect(&mut vm.m.ram, x0, y0, x1, y1, c, fill);
    ok()
}
fn rect(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    rect_common(vm, a, false)
}
fn rectfill(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    rect_common(vm, a, true)
}
fn rrect_common(vm: &mut Vm, a: &[Value], fill: bool) -> Result<Ret, Ctl> {
    let (x, y, w, h, r) = (int(vm, a, 0, 0), int(vm, a, 1, 0), int(vm, a, 2, 0), int(vm, a, 3, 0), int(vm, a, 4, 0));
    let c = colour(vm, a, 5);
    g::rrect(&mut vm.m.ram, x, y, w, h, r, c, fill);
    ok()
}
fn rrect(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    rrect_common(vm, a, false)
}
fn rrectfill(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    rrect_common(vm, a, true)
}
fn circ_common(vm: &mut Vm, a: &[Value], fill: bool) -> Result<Ret, Ctl> {
    let (x, y, r) = (int(vm, a, 0, 0), int(vm, a, 1, 0), int(vm, a, 2, 4));
    let c = colour(vm, a, 3);
    g::circ(&mut vm.m.ram, x, y, r, c, fill);
    ok()
}
fn circ(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    circ_common(vm, a, false)
}
fn circfill(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    circ_common(vm, a, true)
}
fn oval_common(vm: &mut Vm, a: &[Value], fill: bool) -> Result<Ret, Ctl> {
    let (x0, y0, x1, y1) = (int(vm, a, 0, 0), int(vm, a, 1, 0), int(vm, a, 2, 0), int(vm, a, 3, 0));
    let c = colour(vm, a, 4);
    g::oval(&mut vm.m.ram, x0, y0, x1, y1, c, fill);
    ok()
}
fn oval(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    oval_common(vm, a, false)
}
fn ovalfill(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    oval_common(vm, a, true)
}
fn spr(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (n, x, y) = (int(vm, a, 0, 0), int(vm, a, 1, 0), int(vm, a, 2, 0));
    let (w, h) = (fix::to_f64(fx(vm, a, 3, ONE)), fix::to_f64(fx(vm, a, 4, ONE)));
    g::spr(&mut vm.m.ram, n, x, y, w, h, truthy(a, 5), truthy(a, 6));
    ok()
}
fn sspr(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let v: Vec<i32> = (0..6).map(|i| int(vm, a, i, 0)).collect();
    let dw = int(vm, a, 6, v[2]);
    let dh = int(vm, a, 7, v[3]);
    g::sspr(&mut vm.m.ram, v[0], v[1], v[2], v[3], v[4], v[5], dw, dh, truthy(a, 8), truthy(a, 9));
    ok()
}
fn map(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    // With no size, the map's width and its own rows: 32 where its lower
    // half is the sprites', all of them where it is moved past 0x8000.
    let ram = &vm.m.ram;
    let width = if ram[0x5f57] == 0 { 256 } else { i32::from(ram[0x5f57]) };
    let rows = if ram[0x5f56] >= 0x80 { (0x10000 - (i32::from(ram[0x5f56]) << 8)) / width } else { 32 };
    let v: Vec<i32> = [0, 0, 0, 0, width, rows].iter().enumerate().map(|(i, &d)| int(vm, a, i, d)).collect();
    let layers = int(vm, a, 6, 0) as u8;
    g::map(&mut vm.m.ram, v[0], v[1], v[2], v[3], v[4], v[5], layers);
    ok()
}
fn tline(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let v: Vec<i32> = (0..4).map(|i| int(vm, a, i, 0)).collect();
    let (mx, my) = (fx(vm, a, 4, 0), fx(vm, a, 5, 0));
    let (mdx, mdy) = (fx(vm, a, 6, ONE / 8), fx(vm, a, 7, 0));
    let layers = int(vm, a, 8, 0) as u8;
    g::tline(&mut vm.m.ram, v[0], v[1], v[2], v[3], mx, my, mdx, mdy, layers);
    ok()
}
fn cursor(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (x, y) = (int(vm, a, 0, 0), int(vm, a, 1, 0));
    let old_c = vm.m.ram[0x5f25];
    if a.len() > 2 {
        colour(vm, a, 2);
    }
    let (ox, oy) = g::set_cursor(&mut vm.m.ram, x, y);
    many(vm, &[Value::int(ox), Value::int(oy), Value::int(i32::from(old_c))])
}
fn color(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let old = vm.m.ram[0x5f25];
    vm.m.ram[0x5f25] = int(vm, a, 0, 6) as u8;
    int_ret(i32::from(old))
}
fn camera(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let (x, y) = (int(vm, a, 0, 0), int(vm, a, 1, 0));
    let (ox, oy) = g::set_camera(&mut vm.m.ram, x, y);
    Ok(Ret::Two(Value::int(ox), Value::int(oy)))
}
fn clip(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let r = if a.len() >= 4 { Some((int(vm, a, 0, 0), int(vm, a, 1, 0), int(vm, a, 2, 128), int(vm, a, 3, 128))) } else { None };
    let old = g::clip(&mut vm.m.ram, r, truthy(a, 4));
    let vals = old.map(Value::int);
    many(vm, &vals)
}
fn pal(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    // pal() and pal(0) reset (the fill pattern too); pal(c) alone does nothing.
    if a.len() <= 1 && table(a, 0).is_none() {
        if fx(vm, a, 0, 0) != 0 {
            return ok();
        }
        g::reset_palettes(&mut vm.m.ram);
        vm.m.ram[0x5f31..0x5f35].fill(0);
        return ok();
    }
    if let Some(t) = table(a, 0) {
        let p = int(vm, a, 1, 0);
        let pairs: Vec<(Value, Value)> = vm.heap.tab(t).entries().collect();
        for (k, v) in pairs {
            if let (Some(c0), Some(c1)) = (vm.number(k), vm.number(v)) {
                set_pal(&mut vm.m.ram, fix::floor(c0), fix::floor(c1), p);
            }
        }
        return ok();
    }
    let (c0, c1, p) = (int(vm, a, 0, 0), int(vm, a, 1, 0), int(vm, a, 2, 0));
    let old = set_pal(&mut vm.m.ram, c0, c1, p);
    int_ret(old)
}
fn set_pal(ram: &mut Ram, c0: i32, c1: i32, p: i32) -> i32 {
    let i = (c0 & 15) as usize;
    match p {
        1 => {
            let old = ram[0x5f10 + i];
            ram[0x5f10 + i] = c1 as u8 & 0x8f;
            i32::from(old)
        }
        2 => {
            let old = ram[0x5f60 + i];
            ram[0x5f60 + i] = c1 as u8;
            i32::from(old)
        }
        _ => {
            let old = ram[0x5f00 + i];
            ram[0x5f00 + i] = (old & 0x10) | (c1 as u8 & 15);
            i32::from(old & 15)
        }
    }
}
fn palt(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let ram = &mut vm.m.ram;
    let bits = |r: &Ram| (0..16).fold(0i32, |b, c| b | i32::from(r[0x5f00 + c] & 0x10 != 0) << (15 - c));
    let old = bits(ram);
    match a.len() {
        0 => {
            for c in 0..16 {
                ram[0x5f00 + c] &= 15;
            }
            ram[0x5f00] |= 0x10;
        }
        1 => {
            let b = int_h(&vm.heap, a, 0, 0);
            for c in 0..16 {
                ram[0x5f00 + c] = (ram[0x5f00 + c] & 15) | if b >> (15 - c) & 1 != 0 { 0x10 } else { 0 };
            }
        }
        _ => {
            let c = (int_h(&vm.heap, a, 0, 0) & 15) as usize;
            ram[0x5f00 + c] = (ram[0x5f00 + c] & 15) | if truthy(a, 1) { 0x10 } else { 0 };
        }
    }
    int_ret(old)
}
fn fillp(vm: &mut Vm, a: &[Value]) -> Result<Ret, Ctl> {
    let p = fx(vm, a, 0, 0);
    let ram = &mut vm.m.ram;
    let old = i32::from(u16::from_le_bytes([ram[0x5f31], ram[0x5f32]])) << 16 | i32::from(ram[0x5f33] & 1) << 15;
    let bits = (p >> 16) as u16;
    ram[0x5f31..0x5f33].copy_from_slice(&bits.to_le_bytes());
    ram[0x5f33] = ((p >> 15) & 1) as u8 | ((p >> 13) & 2) as u8 | ((p >> 11) & 4) as u8;
    one(Value::Num(old))
}

/// The functions a call to a global compiles to an instruction of its own
/// for, by name and number of arguments: the instruction does what the
/// native does, in place, while the global still is that native.
pub static INTRINSICS: [(&str, u8); 20] = [
    ("flr", 1),
    ("abs", 1),
    ("sgn", 1),
    ("min", 2),
    ("max", 2),
    ("mid", 3),
    ("band", 2),
    ("bor", 2),
    ("bxor", 2),
    ("sin", 1),
    ("cos", 1),
    ("sget", 2),
    ("mget", 2),
    ("fget", 2),
    ("fget", 1),
    ("pget", 2),
    ("peek", 1),
    ("poke", 2),
    ("pset", 3),
    ("pset", 2),
];

/// The native an intrinsic stands for.
pub fn intrinsic_native(id: u8) -> u16 {
    static IDS: std::sync::LazyLock<Vec<u16>> = std::sync::LazyLock::new(|| {
        INTRINSICS.iter().map(|(name, _)| NATIVES.iter().position(|(n, _)| n == name).expect("an intrinsic's native") as u16).collect()
    });
    IDS[id as usize]
}

/// An intrinsic on numbers: the same as its native, or None when an
/// argument is no number (the native then converts it, as it does).
#[inline]
pub fn intrinsic(m: &mut Machine, id: u8, a: &[Value]) -> Option<Value> {
    let mut n = [0i32; 3];
    for (o, v) in n.iter_mut().zip(a) {
        match v {
            Value::Num(x) => *o = *x,
            _ => return None,
        }
    }
    let [x, y, z] = n;
    let int = |v: i32| Value::Num(fix::from_int(v));
    Some(match id {
        0 => Value::Num(x & !0xffff),
        1 => Value::Num(x.wrapping_abs()),
        2 => int(if x < 0 { -1 } else { 1 }),
        3 => Value::Num(x.min(y)),
        4 => Value::Num(x.max(y)),
        5 => {
            let mut v = [x, y, z];
            v.sort_unstable();
            Value::Num(v[1])
        }
        6 => Value::Num(x & y),
        7 => Value::Num(x | y),
        8 => Value::Num(x ^ y),
        9 => Value::Num(sin_fx(x)),
        10 => Value::Num(cos_fx(x)),
        11 => int(i32::from(g::sget(&m.ram, fix::floor(x), fix::floor(y)))),
        12 => int(i32::from(g::mget(&m.ram, fix::floor(x), fix::floor(y)))),
        13 | 14 => {
            let s = fix::floor(x);
            let flags = if (0..256).contains(&s) { m.ram[0x3000 + s as usize] } else { 0 };
            if id == 14 {
                int(i32::from(flags))
            } else {
                let f = fix::floor(y);
                Value::Bool((0..8).contains(&f) && flags & (1 << f) != 0)
            }
        }
        15 => int(i32::from(g::pget(&m.ram, fix::floor(x), fix::floor(y)))),
        16 => int(i32::from(m.ram[fix::floor(x) as u16 as usize])),
        17 => {
            m.ram[fix::floor(x) as u16 as usize] = fix::floor(y) as u8;
            Value::Nil
        }
        18 | 19 => {
            let c = g::pen_bits(&mut m.ram, (id == 18).then_some(z));
            g::pset(&mut m.ram, fix::floor(x), fix::floor(y), c);
            Value::Nil
        }
        _ => return None,
    })
}

/// Whether a native can run Lua (a callback, a metamethod, a coroutine):
/// the others borrow their arguments from the stack instead of a copy.
pub fn calls_lua(n: u16) -> bool {
    static LUA: std::sync::LazyLock<Vec<bool>> = std::sync::LazyLock::new(|| {
        const NAMES: [&str; 9] = ["foreach", "pairs", "tostr", "tostring", "print", "printh", "stop", "coresume", "error"];
        NATIVES.iter().map(|(name, _)| NAMES.contains(name)).collect()
    });
    LUA[n as usize]
}

/// Every native, by name; the ones starting with `__` are not globals.
pub static NATIVES: &[(&str, Native)] = &[
    ("next", next),
    ("inext", inext),
    ("__all_next", all_native),
    ("pairs", pairs),
    ("ipairs", ipairs),
    ("all", all),
    ("foreach", foreach),
    ("count", count),
    ("add", add),
    ("del", del),
    ("deli", deli),
    ("type", type_),
    ("tostr", tostr_native),
    ("tostring", tostr_native),
    ("tonum", tonum),
    ("tonumber", tonum),
    ("chr", chr),
    ("ord", ord),
    ("sub", sub),
    ("split", split),
    ("select", select),
    ("unpack", unpack),
    ("pack", pack),
    ("rawget", rawget),
    ("rawset", rawset),
    ("rawequal", rawequal),
    ("rawlen", rawlen),
    ("setmetatable", setmetatable),
    ("getmetatable", getmetatable),
    ("assert", assert),
    ("error", error),
    ("cocreate", cocreate),
    ("coresume", coresume),
    ("costatus", costatus),
    ("yield", yield_),
    ("print", print),
    ("printh", printh),
    ("stop", stop),
    ("run", run),
    ("reset", reset),
    ("flip", flip),
    ("extcmd", nothing),
    ("holdframe", nothing),
    ("serial", serial),
    ("_set_fps", set_fps),
    ("_update_buttons", nothing),
    ("_mark_cpu", nothing),
    ("load", nothing),
    ("trace", nothing),
    ("time", time),
    ("t", time),
    ("stat", stat),
    ("btn", btn),
    ("btnp", btnp),
    ("abs", abs),
    ("flr", flr),
    ("ceil", ceil),
    ("sqrt", sqrt),
    ("sgn", sgn),
    ("min", min),
    ("max", max),
    ("mid", mid),
    ("sin", sin),
    ("cos", cos),
    ("atan2", atan2),
    ("rnd", rnd),
    ("srand", srand_native),
    ("band", band),
    ("bor", bor),
    ("bxor", bxor),
    ("bnot", bnot),
    ("shl", shl),
    ("shr", shr),
    ("lshr", lshr),
    ("rotl", rotl),
    ("rotr", rotr),
    ("peek", peek),
    ("peek2", peek2),
    ("peek4", peek4),
    ("poke", poke),
    ("poke2", poke2),
    ("poke4", poke4),
    ("memcpy", memcpy),
    ("memset", memset),
    ("reload", reload),
    ("cstore", cstore),
    ("cartdata", cartdata),
    ("dget", dget),
    ("dset", dset),
    ("menuitem", menuitem),
    ("sfx", sfx),
    ("music", music),
    ("cls", cls),
    ("pset", pset),
    ("pget", pget),
    ("sget", sget),
    ("sset", sset),
    ("fget", fget),
    ("fset", fset),
    ("mget", mget),
    ("mset", mset),
    ("line", line),
    ("rect", rect),
    ("rectfill", rectfill),
    ("rrect", rrect),
    ("rrectfill", rrectfill),
    ("circ", circ),
    ("circfill", circfill),
    ("oval", oval),
    ("ovalfill", ovalfill),
    ("spr", spr),
    ("sspr", sspr),
    ("map", map),
    ("mapdraw", map),
    ("tline", tline),
    ("cursor", cursor),
    ("color", color),
    ("camera", camera),
    ("clip", clip),
    ("pal", pal),
    ("palt", palt),
    ("fillp", fillp),
];

/// The glyphs PICO-8 sets as globals: the buttons, and fill patterns.
pub const GLYPHS: [(u8, i32); 26] = [
    (128, 0x0000_0000),
    (129, 0x5a5a_8000),
    (130, 0x511f_8000),
    (131, 3 << 16),
    (132, 0x7d7d_8000),
    (133, 0xb81d_8000u32 as i32),
    (134, 0xf99f_8000u32 as i32),
    (135, 0x51bf_8000),
    (136, 0xb5bf_8000u32 as i32),
    (137, 0x999f_8000u32 as i32),
    (138, 0xb11f_8000u32 as i32),
    (139, 0),
    (140, 0xa0e0_8000u32 as i32),
    (141, 0x9b3f_8000u32 as i32),
    (142, 4 << 16),
    (143, 0xb1bf_8000u32 as i32),
    (144, 0xf5ff_8000u32 as i32),
    (145, 1 << 16),
    (146, 0xb15f_8000u32 as i32),
    (147, 0x1b1f_8000),
    (148, 2 << 16),
    (149, 0xf5bf_8000u32 as i32),
    (150, 0x7adf_8000),
    (151, 5 << 16),
    (152, 0x0f0f_8000),
    (153, 0x5555_8000),
];
