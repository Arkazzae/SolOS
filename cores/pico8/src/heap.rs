//! Lua's values and the heap they live in. A value is a small copyable
//! tag and number: a number, or the index of a string, table, function,
//! cell or coroutine in the heap's arenas. Strings are interned, so two
//! equal strings are the same index and a table looks a string key up by
//! that number alone.
//!
//! Nothing is reference counted: the heap is collected by marking what the
//! virtual machine can still reach and freeing the rest, and everything in
//! it is plain data, which a save state can write out whole.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

use crate::state::{BadState, R, Res, W};
use crate::vm::Proto;


#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Value {
    #[default]
    Nil,
    Bool(bool),
    /// 16.16 fixed point.
    Num(i32),
    Str(u32),
    Table(u32),
    /// A Lua function (a closure).
    Func(u32),
    /// A function of the console's own, by its number.
    Native(u16),
    Thread(u32),
    /// A local that a closure captured, kept in a cell; never seen by Lua.
    Cell(u32),
}

impl Value {
    pub fn truthy(self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }
    pub fn int(i: i32) -> Value {
        Value::Num(crate::fix::from_int(i))
    }
    /// The key's bits for a table's hash part.
    fn bits(self) -> u64 {
        let (tag, n) = match self {
            Value::Nil => (0, 0),
            Value::Bool(b) => (1, b as u32),
            Value::Num(n) => (2, n as u32),
            Value::Str(i) => (3, i),
            Value::Table(i) => (4, i),
            Value::Func(i) => (5, i),
            Value::Native(i) => (6, u32::from(i)),
            Value::Thread(i) => (7, i),
            Value::Cell(i) => (8, i),
        };
        (tag as u64) << 32 | u64::from(n)
    }
}

/// An array index this value is, if it is a whole number.
fn index(k: Value) -> Option<usize> {
    match k {
        Value::Num(n) if n & 0xffff == 0 && n > 0 => Some((n >> 16) as usize),
        _ => None,
    }
}

/// Where the last `next` was: the key it gave, and where to look on from
/// (an index of the array part, or an entry of the hash part).
#[derive(Clone, Copy, Default, PartialEq)]
enum Cursor {
    #[default]
    None,
    Arr(Value, usize),
    Hash(Value, usize),
}

/// A table: an array part for the keys 1..n, and the other keys in a hash
/// part kept in the order they came in, which is the order `pairs` goes in.
/// A removed key stays in the hash part with nil until it is rebuilt, and
/// the table remembers where the last `next` was, so a `pairs` loop that
/// clears fields and adds others as it goes keeps its place.
#[derive(Default, Clone)]
pub struct Table {
    pub arr: Vec<Value>,
    keys: Vec<Value>,
    vals: Vec<Value>,
    /// Open addressing: 0 is empty, otherwise an entry's index + 1.
    slots: Vec<u32>,
    pub meta: Option<u32>,
    cursor: std::cell::Cell<Cursor>,
}

fn spread(bits: u64) -> u64 {
    bits.wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

impl Table {
    pub fn with_capacity(arr: usize, hash: usize) -> Table {
        let mut t = Table { arr: Vec::with_capacity(arr), ..Table::default() };
        if hash > 0 {
            t.keys.reserve(hash);
            t.vals.reserve(hash);
        }
        t
    }

    fn find(&self, k: Value) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut i = (spread(k.bits()) >> 32) as usize & mask;
        loop {
            match self.slots[i] {
                0 => return None,
                s if self.keys[s as usize - 1] == k => return Some(s as usize - 1),
                _ => i = (i + 1) & mask,
            }
        }
    }

    pub fn get(&self, k: Value) -> Value {
        if let Some(i) = index(k)
            && i <= self.arr.len()
        {
            return self.arr[i - 1];
        }
        match self.find(k) {
            Some(e) => self.vals[e],
            None => Value::Nil,
        }
    }

    /// A string key's value, trying first the entry the hint names (the
    /// one this instruction found last time): objects made alike have
    /// their fields at the same entries. A string is never in the array
    /// part, so the hint cannot miss a value there.
    #[inline(always)]
    pub fn get_hinted(&self, k: Value, hint: &std::cell::Cell<u32>) -> Value {
        let h = hint.get() as usize;
        if h != 0 && h <= self.keys.len() && self.keys[h - 1] == k {
            return self.vals[h - 1];
        }
        self.get_and_hint(k, hint)
    }

    #[inline(never)]
    fn get_and_hint(&self, k: Value, hint: &std::cell::Cell<u32>) -> Value {
        match self.find(k) {
            Some(e) => {
                hint.set(e as u32 + 1);
                self.vals[e]
            }
            None => Value::Nil,
        }
    }

    /// Sets a string key that is already there (its value not nil), by
    /// the hint; false when it is not, for the slow way.
    #[inline]
    pub fn set_hinted(&mut self, k: Value, v: Value, hint: &std::cell::Cell<u32>) -> bool {
        let h = hint.get() as usize;
        let e = if h != 0 && h <= self.keys.len() && self.keys[h - 1] == k {
            h - 1
        } else {
            match self.find(k) {
                Some(e) => {
                    hint.set(e as u32 + 1);
                    e
                }
                None => return false,
            }
        };
        if self.vals[e] == Value::Nil {
            return false;
        }
        self.vals[e] = v;
        true
    }

    /// Sets a key that is not nil.
    pub fn set(&mut self, k: Value, v: Value) {
        if let Some(i) = index(k) {
            let n = self.arr.len();
            if i <= n {
                self.arr[i - 1] = v;
                if v == Value::Nil && i == n {
                    while self.arr.last() == Some(&Value::Nil) {
                        self.arr.pop();
                    }
                }
                return;
            }
            if i == n + 1 && v != Value::Nil {
                if let Some(e) = self.find(k) {
                    self.vals[e] = Value::Nil;
                }
                self.arr.push(v);
                self.migrate();
                return;
            }
        }
        match self.find(k) {
            Some(e) => self.vals[e] = v,
            None if v != Value::Nil => self.insert(k, v),
            None => {}
        }
    }

    /// Moves the keys that now continue the array from the hash part.
    fn migrate(&mut self) {
        if self.slots.is_empty() {
            return;
        }
        while let Some(e) = self.find(Value::int(self.arr.len() as i32 + 1)) {
            let v = std::mem::take(&mut self.vals[e]);
            if v == Value::Nil {
                break;
            }
            self.arr.push(v);
        }
    }

    fn insert(&mut self, k: Value, v: Value) {
        if (self.keys.len() + 1) * 4 > self.slots.len() * 3 {
            self.rebuild();
        }
        self.keys.push(k);
        self.vals.push(v);
        self.place(self.keys.len() - 1);
    }

    fn place(&mut self, e: usize) {
        let mask = self.slots.len() - 1;
        let mut i = (spread(self.keys[e].bits()) >> 32) as usize & mask;
        while self.slots[i] != 0 {
            i = (i + 1) & mask;
        }
        self.slots[i] = e as u32 + 1;
    }

    /// Drops the removed keys and resizes the hash part for one more.
    fn rebuild(&mut self) {
        let mut keys = Vec::with_capacity(self.keys.len() + 1);
        let mut vals = Vec::with_capacity(self.keys.len() + 1);
        let mut cursor = self.cursor.get();
        for (e, (&k, &v)) in self.keys.iter().zip(&self.vals).enumerate() {
            if let Cursor::Hash(key, at) = cursor
                && at == e
            {
                cursor = Cursor::Hash(key, keys.len());
            }
            if v != Value::Nil {
                keys.push(k);
                vals.push(v);
            }
        }
        if let Cursor::Hash(key, at) = cursor
            && at >= self.keys.len()
        {
            cursor = Cursor::Hash(key, keys.len());
        }
        self.cursor.set(cursor);
        let size = ((keys.len() + 1) * 2).next_power_of_two().max(4);
        self.keys = keys;
        self.vals = vals;
        self.slots = vec![0; size];
        for e in 0..self.keys.len() {
            self.place(e);
        }
    }

    /// A table constructor's items from position start on: all of them in
    /// the array part, nil or not, as Lua's constructor puts them, so `#`
    /// and unpack see a list with holes whole.
    pub fn set_list(&mut self, start: usize, vals: &[Value]) {
        let end = start - 1 + vals.len();
        if self.arr.len() < end {
            self.arr.resize(end, Value::Nil);
        }
        for (i, &v) in vals.iter().enumerate() {
            let at = start + i;
            self.arr[at - 1] = v;
            if !self.slots.is_empty()
                && let Some(e) = self.find(Value::int(at as i32))
            {
                self.vals[e] = Value::Nil;
            }
        }
        while self.arr.last() == Some(&Value::Nil) {
            self.arr.pop();
        }
        self.migrate();
    }

    /// Inserts v at position i of the sequence (1..=len+1), moving the rest up.
    pub fn insert_at(&mut self, i: usize, v: Value) {
        if v == Value::Nil {
            return;
        }
        let i = i.clamp(1, self.arr.len() + 1);
        self.arr.insert(i - 1, v);
        if let Some(e) = self.find(Value::int(self.arr.len() as i32)) {
            self.vals[e] = Value::Nil;
        }
        self.migrate();
    }

    /// Removes position i of the sequence, moving the rest down.
    pub fn remove_at(&mut self, i: usize) -> Value {
        if i == 0 || i > self.arr.len() {
            return Value::Nil;
        }
        let v = self.arr.remove(i - 1);
        while self.arr.last() == Some(&Value::Nil) {
            self.arr.pop();
        }
        v
    }

    /// The length: the array part's, a border as Lua's `#` allows.
    pub fn len(&self) -> usize {
        self.arr.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.arr.is_empty() && self.vals.iter().all(|&v| v == Value::Nil)
    }

    /// The key and value after k, in pairs' order; Err for a key not here.
    pub fn next(&self, k: Value) -> Result<Option<(Value, Value)>, ()> {
        let mut from_hash = 0;
        let mut a = match (k, self.cursor.get()) {
            (Value::Nil, _) => 0,
            (k, Cursor::Arr(c, at)) if c == k => at,
            (k, Cursor::Hash(c, at)) if c == k => {
                from_hash = at;
                usize::MAX
            }
            _ => match index(k) {
                Some(i) if i <= self.arr.len() => i,
                _ => {
                    from_hash = self.find(k).ok_or(())? + 1;
                    usize::MAX
                }
            },
        };
        while a < self.arr.len() {
            if self.arr[a] != Value::Nil {
                let key = Value::int(a as i32 + 1);
                self.cursor.set(Cursor::Arr(key, a + 1));
                return Ok(Some((key, self.arr[a])));
            }
            a += 1;
        }
        for e in from_hash..self.keys.len() {
            if self.vals[e] != Value::Nil {
                self.cursor.set(Cursor::Hash(self.keys[e], e + 1));
                return Ok(Some((self.keys[e], self.vals[e])));
            }
        }
        self.cursor.set(Cursor::None);
        Ok(None)
    }

    /// Every key and value, for the collector and save states.
    pub fn entries(&self) -> impl Iterator<Item = (Value, Value)> + '_ {
        let arr = self.arr.iter().enumerate().map(|(i, &v)| (Value::int(i as i32 + 1), v));
        arr.chain(self.keys.iter().copied().zip(self.vals.iter().copied())).filter(|&(_, v)| v != Value::Nil)
    }

    fn hash_len(&self) -> usize {
        self.keys.len()
    }
}

/// A Lua function with the cells of the locals it captured.
#[derive(Clone)]
pub struct Closure {
    pub proto: Rc<Proto>,
    pub upvals: Box<[u32]>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Suspended,
    Running,
    /// Resumed another, and waits for it.
    Normal,
    Dead,
}

/// A call in progress.
#[derive(Clone, Debug)]
pub struct Frame {
    pub closure: u32,
    pub pc: u32,
    pub base: u32,
    /// The results the caller wants, or -1 for all of them.
    pub want: i16,
    /// Whether returning from it leaves the machine's run loop: the call
    /// came from the console's own code (a callback, a metamethod).
    pub boundary: bool,
    pub varargs: Vec<Value>,
}

/// A thread of Lua calls: the main one, or a coroutine's.
#[derive(Clone)]
pub struct Thread {
    pub stack: Vec<Value>,
    pub frames: Vec<Frame>,
    pub status: Status,
    /// The function a coroutine starts with, until it starts.
    pub start: Option<Value>,
    /// Calls back into Lua from the console's functions now running in it:
    /// it cannot yield across them.
    pub nested: u32,
    /// Where the values the thread is resumed with go (the results of its
    /// yield or flip), and how many are wanted.
    pub resume_at: Option<(u32, i16)>,
}

impl Thread {
    pub fn new(start: Option<Value>) -> Thread {
        Thread { stack: Vec::new(), frames: Vec::new(), status: Status::Suspended, start, nested: 0, resume_at: None }
    }
}

#[derive(Default)]
pub struct FxHasher(u64);

impl Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(word)).wrapping_mul(0x51_7cc1_b727_220a);
        }
    }
    fn write_u8(&mut self, n: u8) {
        self.0 = (self.0.rotate_left(5) ^ u64::from(n)).wrapping_mul(0x51_7cc1_b727_220a);
    }
    fn write_usize(&mut self, n: usize) {
        self.0 = (self.0.rotate_left(5) ^ n as u64).wrapping_mul(0x51_7cc1_b727_220a);
    }
}

pub type Fx = BuildHasherDefault<FxHasher>;

/// Slots of one kind of object, with the free ones to reuse.
#[derive(Clone)]
pub struct Arena<T> {
    pub items: Vec<Option<T>>,
    free: Vec<u32>,
    marks: Vec<bool>,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Arena { items: Vec::new(), free: Vec::new(), marks: Vec::new() }
    }
}

impl<T> Arena<T> {
    fn add(&mut self, item: T) -> u32 {
        if let Some(i) = self.free.pop() {
            self.items[i as usize] = Some(item);
            i
        } else {
            self.items.push(Some(item));
            self.marks.push(false);
            self.items.len() as u32 - 1
        }
    }
    pub fn get(&self, i: u32) -> &T {
        self.items[i as usize].as_ref().expect("a live object")
    }
    pub fn get_mut(&mut self, i: u32) -> &mut T {
        self.items[i as usize].as_mut().expect("a live object")
    }
    /// Marks i, and whether it was not marked yet.
    fn mark(&mut self, i: u32) -> bool {
        let m = &mut self.marks[i as usize];
        !std::mem::replace(m, true)
    }
    /// Frees what is not marked, clears the marks; the number left.
    fn sweep(&mut self, mut freed: impl FnMut(u32, &T)) -> usize {
        let mut live = 0;
        for (i, (item, mark)) in self.items.iter_mut().zip(self.marks.iter_mut()).enumerate() {
            if item.is_some() {
                if *mark {
                    live += 1;
                } else {
                    freed(i as u32, item.as_ref().unwrap());
                    *item = None;
                    self.free.push(i as u32);
                }
            }
            *mark = false;
        }
        live
    }
    pub fn live(&self) -> usize {
        self.items.len() - self.free.len()
    }
}

#[derive(Default)]
pub struct Heap {
    pub strings: Arena<Rc<[u8]>>,
    interned: HashMap<Rc<[u8]>, u32, Fx>,
    /// Strings that are never freed: the constants in the code.
    pinned: Vec<bool>,
    pub tables: Arena<Table>,
    pub closures: Arena<Closure>,
    pub cells: Arena<Value>,
    /// Coroutines; the one running is out of its slot, in the machine.
    pub threads: Arena<Thread>,
    /// Objects made since the last collection.
    pub made: usize,
    /// Bytes made since the last collection, roughly.
    pub bytes: usize,
}

impl Heap {
    pub fn intern(&mut self, s: &[u8]) -> Value {
        if let Some(&i) = self.interned.get(s) {
            return Value::Str(i);
        }
        let rc: Rc<[u8]> = s.into();
        let i = self.strings.add(rc.clone());
        if self.pinned.len() <= i as usize {
            self.pinned.resize(i as usize + 1, false);
        }
        self.pinned[i as usize] = false;
        self.interned.insert(rc, i);
        self.made += 1;
        self.bytes += s.len() + 24;
        Value::Str(i)
    }
    /// A string's value if it was ever interned (no other can be a key).
    pub fn find(&self, s: &[u8]) -> Option<Value> {
        self.interned.get(s).map(|&i| Value::Str(i))
    }
    pub fn pin(&mut self, v: Value) {
        if let Value::Str(i) = v {
            self.pinned[i as usize] = true;
        }
    }
    pub fn str(&self, i: u32) -> &[u8] {
        self.strings.get(i)
    }

    pub fn table(&mut self, t: Table) -> Value {
        self.made += 1;
        self.bytes += 40 + 8 * (t.arr.capacity() + t.hash_len());
        Value::Table(self.tables.add(t))
    }
    pub fn closure(&mut self, c: Closure) -> Value {
        self.made += 1;
        self.bytes += 32 + 4 * c.upvals.len();
        Value::Func(self.closures.add(c))
    }
    pub fn cell(&mut self, v: Value) -> u32 {
        self.made += 1;
        self.bytes += 16;
        self.cells.add(v)
    }
    pub fn thread(&mut self, t: Thread) -> u32 {
        self.made += 1;
        self.bytes += 256;
        self.threads.add(t)
    }
    pub fn tab(&self, i: u32) -> &Table {
        self.tables.get(i)
    }
    pub fn tab_mut(&mut self, i: u32) -> &mut Table {
        self.tables.get_mut(i)
    }

    /// Marks everything reachable from these values.
    pub fn mark(&mut self, roots: impl IntoIterator<Item = Value>) {
        let mut work: Vec<Value> = roots.into_iter().collect();
        while let Some(v) = work.pop() {
            match v {
                Value::Str(i) => {
                    self.strings.mark(i);
                }
                Value::Table(i) => {
                    if self.tables.mark(i) {
                        let t = self.tables.get(i);
                        work.extend(t.arr.iter().copied());
                        work.extend(t.keys.iter().copied());
                        work.extend(t.vals.iter().copied());
                        if let Some(m) = t.meta {
                            work.push(Value::Table(m));
                        }
                    }
                }
                Value::Func(i) => {
                    if self.closures.mark(i) {
                        work.extend(self.closures.get(i).upvals.iter().map(|&c| Value::Cell(c)));
                    }
                }
                Value::Cell(i) => {
                    if self.cells.mark(i) {
                        work.push(*self.cells.get(i));
                    }
                }
                Value::Thread(i) => {
                    // A running coroutine is out of its slot; the machine
                    // marks it from there.
                    if self.threads.items[i as usize].is_some() && self.threads.mark(i) {
                        let t = self.threads.get(i);
                        work.extend(thread_values(t));
                    }
                }
                Value::Nil | Value::Bool(_) | Value::Num(_) | Value::Native(_) => {}
            }
        }
    }

    /// Frees what was not marked.
    pub fn sweep(&mut self) -> usize {
        let pinned = &self.pinned;
        for (i, s) in self.strings.items.iter().enumerate() {
            if s.is_some() && pinned[i] {
                self.strings.marks[i] = true;
            }
        }
        let interned = &mut self.interned;
        let mut live = self.strings.sweep(|_, s| {
            interned.remove(s);
        });
        live += self.tables.sweep(|_, _| {});
        live += self.closures.sweep(|_, _| {});
        live += self.cells.sweep(|_, _| {});
        live += self.threads.sweep(|_, _| {});
        self.made = 0;
        self.bytes = 0;
        live
    }

    /// Roughly how many bytes the live objects take, for stat(0).
    pub fn size(&self) -> usize {
        let strings: usize = self.strings.items.iter().flatten().map(|s| s.len() + 24).sum();
        let tables: usize = self.tables.items.iter().flatten().map(|t| 40 + 8 * (t.arr.len() + t.keys.len())).sum();
        strings + tables + self.closures.live() * 32 + self.cells.live() * 16 + self.threads.live() * 256
    }

}

/// The values a thread holds: its stack, its frames' functions and extra
/// arguments, and the function it starts with.
pub fn thread_values(t: &Thread) -> impl Iterator<Item = Value> + '_ {
    t.stack
        .iter()
        .copied()
        .chain(t.frames.iter().flat_map(|f| std::iter::once(Value::Func(f.closure)).chain(f.varargs.iter().copied())))
        .chain(t.start)
}

// Save states.

fn save_thread(w: &mut W, t: &Thread) {
    w.values(&t.stack);
    w.len(t.frames.len());
    for f in &t.frames {
        w.u32(f.closure);
        w.u32(f.pc);
        w.u32(f.base);
        w.i32(i32::from(f.want));
        w.bool(f.boundary);
        w.values(&f.varargs);
    }
    w.u8(match t.status {
        Status::Suspended => 0,
        Status::Running => 1,
        Status::Normal => 2,
        Status::Dead => 3,
    });
    w.bool(t.start.is_some());
    w.value(t.start.unwrap_or_default());
    w.u32(t.nested);
    w.bool(t.resume_at.is_some());
    let (at, want) = t.resume_at.unwrap_or((0, 0));
    w.u32(at);
    w.i32(i32::from(want));
}

fn load_thread(r: &mut R) -> Res<Thread> {
    let stack = r.values()?;
    let n = r.len()?;
    let mut frames = Vec::with_capacity(n);
    for _ in 0..n {
        frames.push(Frame {
            closure: r.u32()?,
            pc: r.u32()?,
            base: r.u32()?,
            want: r.i32()? as i16,
            boundary: r.bool()?,
            varargs: r.values()?,
        });
    }
    let status = match r.u8()? {
        0 => Status::Suspended,
        1 => Status::Running,
        2 => Status::Normal,
        3 => Status::Dead,
        _ => return Err(BadState("a thread of no status")),
    };
    let has_start = r.bool()?;
    let start = r.value()?;
    let nested = r.u32()?;
    let has_resume = r.bool()?;
    let at = r.u32()?;
    let want = r.i32()? as i16;
    Ok(Thread {
        stack,
        frames,
        status,
        start: has_start.then_some(start),
        nested,
        resume_at: has_resume.then_some((at, want)),
    })
}

pub fn save_main(w: &mut W, t: &Thread) {
    save_thread(w, t);
}
pub fn load_main(r: &mut R) -> Res<Thread> {
    load_thread(r)
}

fn save_arena<T>(w: &mut W, a: &Arena<T>, mut item: impl FnMut(&mut W, &T)) {
    w.len(a.items.len());
    for slot in &a.items {
        w.bool(slot.is_some());
        if let Some(x) = slot {
            item(w, x);
        }
    }
    // The free slots in their order: what is made next goes where it went.
    w.len(a.free.len());
    for &f in &a.free {
        w.u32(f);
    }
}

fn load_arena<T>(r: &mut R, mut item: impl FnMut(&mut R) -> Res<T>) -> Res<Arena<T>> {
    let n = r.len()?;
    let mut a = Arena::default();
    for _ in 0..n {
        let slot = if r.bool()? { Some(item(r)?) } else { None };
        a.items.push(slot);
        a.marks.push(false);
    }
    let n = r.len()?;
    a.free = (0..n).map(|_| r.u32()).collect::<Res<_>>()?;
    let empty = a.items.iter().filter(|s| s.is_none()).count();
    if a.free.len() != empty || a.free.iter().any(|&f| a.items.get(f as usize).is_none_or(Option::is_some)) {
        return Err(BadState("free slots that are not"));
    }
    Ok(a)
}

impl Heap {
    pub fn save(&self, w: &mut W) {
        save_arena(w, &self.strings, |w, s| w.bytes(s));
        save_arena(w, &self.tables, |w, t| {
            w.values(&t.arr);
            let live: Vec<(Value, Value)> =
                t.keys.iter().copied().zip(t.vals.iter().copied()).filter(|&(_, v)| v != Value::Nil).collect();
            w.len(live.len());
            for (k, v) in live {
                w.value(k);
                w.value(v);
            }
            w.opt_u32(t.meta);
        });
        save_arena(w, &self.closures, |w, c| {
            w.u32(c.proto.id);
            w.len(c.upvals.len());
            for &u in c.upvals.iter() {
                w.u32(u);
            }
        });
        save_arena(w, &self.cells, |w, v| w.value(*v));
        save_arena(w, &self.threads, save_thread);
        w.u64(self.made as u64);
        w.u64(self.bytes as u64);
    }

    /// Replaces the heap with a saved one. The strings the code pins must
    /// be where the cartridge's fresh compile put them.
    pub fn load(&mut self, r: &mut R, protos: &[Rc<Proto>]) -> Res<()> {
        let strings = load_arena(r, |r| Ok(Rc::<[u8]>::from(r.bytes()?)))?;
        for (i, &p) in self.pinned.iter().enumerate() {
            if p && strings.items.get(i).and_then(|s| s.as_deref()) != self.strings.items[i].as_deref() {
                return Err(BadState("made by another version of the cartridge or the core"));
            }
        }
        let tables = load_arena(r, |r| {
            let mut t = Table { arr: r.values()?, ..Table::default() };
            let n = r.len()?;
            for _ in 0..n {
                let (k, v) = (r.value()?, r.value()?);
                if k == Value::Nil {
                    return Err(BadState("a nil key"));
                }
                t.set(k, v);
            }
            t.meta = r.opt_u32()?;
            Ok(t)
        })?;
        let closures = load_arena(r, |r| {
            let id = r.u32()? as usize;
            let proto = protos.get(id).ok_or(BadState("a function the cartridge has not"))?.clone();
            let n = r.len()?;
            let upvals = (0..n).map(|_| r.u32()).collect::<Res<Box<[u32]>>>()?;
            if upvals.len() != proto.upvals.len() {
                return Err(BadState("a function with other captures"));
            }
            Ok(Closure { proto, upvals })
        })?;
        let cells = load_arena(r, |r| r.value())?;
        let threads = load_arena(r, load_thread)?;
        let (made, bytes) = (r.u64()? as usize, r.u64()? as usize);
        self.pinned.resize(strings.items.len(), false);
        self.interned.clear();
        for (i, s) in strings.items.iter().enumerate() {
            if let Some(s) = s {
                self.interned.insert(s.clone(), i as u32);
            }
        }
        self.strings = strings;
        self.tables = tables;
        self.closures = closures;
        self.cells = cells;
        self.threads = threads;
        self.made = made;
        self.bytes = bytes;
        Ok(())
    }

    /// Whether a value points at something that is there.
    pub fn valid(&self, v: Value, natives: usize) -> bool {
        let live = |items: usize, present: bool, i: u32| (i as usize) < items && present;
        match v {
            Value::Str(i) => live(self.strings.items.len(), self.strings.items.get(i as usize).is_some_and(Option::is_some), i),
            Value::Table(i) => live(self.tables.items.len(), self.tables.items.get(i as usize).is_some_and(Option::is_some), i),
            Value::Func(i) => live(self.closures.items.len(), self.closures.items.get(i as usize).is_some_and(Option::is_some), i),
            Value::Cell(i) => live(self.cells.items.len(), self.cells.items.get(i as usize).is_some_and(Option::is_some), i),
            // A thread's slot is empty only while it runs, never in a state.
            Value::Thread(i) => live(self.threads.items.len(), self.threads.items.get(i as usize).is_some_and(Option::is_some), i),
            Value::Native(n) => (n as usize) < natives,
            Value::Nil | Value::Bool(_) | Value::Num(_) => true,
        }
    }

    /// Checks that everything in the heap points at something there, so a
    /// damaged state is refused rather than crash the game later.
    pub fn check(&self, natives: usize) -> Res<()> {
        let ok = |v: Value| if self.valid(v, natives) { Ok(()) } else { Err(BadState("a value that points nowhere")) };
        for t in self.tables.items.iter().flatten() {
            for (k, v) in t.entries() {
                ok(k)?;
                ok(v)?;
            }
            if let Some(m) = t.meta {
                ok(Value::Table(m))?;
            }
        }
        for c in self.closures.items.iter().flatten() {
            for &u in c.upvals.iter() {
                ok(Value::Cell(u))?;
            }
        }
        for v in self.cells.items.iter().flatten() {
            ok(*v)?;
        }
        for t in self.threads.items.iter().flatten() {
            self.check_thread(t, natives)?;
        }
        Ok(())
    }

    pub fn check_thread(&self, t: &Thread, natives: usize) -> Res<()> {
        for v in thread_values(t) {
            if !self.valid(v, natives) {
                return Err(BadState("a thread with a value that points nowhere"));
            }
        }
        for f in &t.frames {
            let p = &self.closures.get(f.closure).proto;
            if f.pc as usize >= p.code.len() || f.base as usize + p.regs as usize > t.stack.len() {
                return Err(BadState("a call frame out of its function"));
            }
        }
        if let Some((at, _)) = t.resume_at
            && at as usize > t.stack.len()
        {
            return Err(BadState("a thread resumed past its stack"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_and_hash_parts() {
        let mut t = Table::default();
        t.set(Value::int(2), Value::int(20));
        assert_eq!(t.len(), 0);
        t.set(Value::int(1), Value::int(10));
        assert_eq!(t.len(), 2);
        t.set(Value::Str(5), Value::Bool(true));
        t.set(Value::Num(0x8000), Value::int(7));
        assert_eq!(t.get(Value::int(2)), Value::int(20));
        assert_eq!(t.get(Value::Num(0x8000)), Value::int(7));
        t.set(Value::int(2), Value::Nil);
        assert_eq!(t.len(), 1);
        let mut k = Value::Nil;
        let mut seen = Vec::new();
        while let Some((nk, v)) = t.next(k).unwrap() {
            seen.push(v);
            k = nk;
        }
        assert_eq!(seen, [Value::int(10), Value::Bool(true), Value::int(7)]);
    }

    #[test]
    fn removed_keys_keep_iteration_going_and_rebuild_later() {
        let mut t = Table::default();
        for i in 0..100 {
            t.set(Value::Str(i), Value::int(i as i32));
        }
        let mut k = Value::Nil;
        let mut n = 0;
        while let Some((nk, _)) = t.next(k).unwrap() {
            t.set(nk, Value::Nil);
            k = nk;
            n += 1;
        }
        assert_eq!(n, 100);
        assert!(t.is_empty());
        for i in 0..1000 {
            t.set(Value::Str(i), Value::int(1));
            t.set(Value::Str(i), Value::Nil);
        }
        assert!(t.slots.len() <= 64);
    }

    #[test]
    fn pairs_keeps_its_place_while_fields_go_and_come() {
        let mut t = Table::default();
        for i in 1..=5 {
            t.set(Value::int(i), Value::int(i));
        }
        for i in 0..20 {
            t.set(Value::Str(i), Value::int(1));
        }
        let (mut k, mut n, mut added) = (Value::Nil, 0, 100);
        while let Some((nk, _)) = t.next(k).unwrap() {
            t.set(nk, Value::Nil);
            if let Value::Str(_) = nk
                && added < 140
            {
                // New keys: the hash part is rebuilt under the loop.
                t.set(Value::Str(added), Value::int(2));
                added += 1;
            }
            k = nk;
            n += 1;
        }
        // Every key, the old and the new, once.
        assert_eq!(n, 5 + 20 + 40);
    }

    #[test]
    fn collects_cycles() {
        let mut h = Heap::default();
        let a = h.table(Table::default());
        let b = h.table(Table::default());
        let (Value::Table(ai), Value::Table(_)) = (a, b) else { unreachable!() };
        h.tab_mut(ai).set(Value::int(1), b);
        if let Value::Table(bi) = b {
            h.tab_mut(bi).set(Value::int(1), a);
        }
        let s = h.intern(b"kept");
        h.mark([s]);
        assert_eq!(h.sweep(), 1);
        assert_eq!(h.intern(b"kept"), s);
    }
}
