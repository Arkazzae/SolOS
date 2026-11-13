//! The virtual machine that runs P8 Lua: compiled functions of register
//! instructions, run in a loop that calls Lua functions without recursing
//! (a call is a frame on the thread, a return pops it), so a coroutine is
//! only another thread of frames, and `flip()` can stop the main thread in
//! the middle of a frame and go on from there the next one.
//!
//! The console's own functions (natives) run in Rust; the few that call
//! back into Lua (foreach, a metamethod) run a nested loop, which a
//! coroutine cannot yield across, as in Lua 5.2.

use std::rc::Rc;

use crate::api::{self, Machine, NATIVES};
use crate::fix;
use crate::heap::{Closure, Frame, Heap, Status, Table, Thread, Value};

/// A register, or a constant (high bit set).
pub type Rk = u16;
pub const K: u16 = 0x8000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arith {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    BAnd,
    BOr,
    BXor,
    Shl,
    Shr,
    LShr,
    RotL,
    RotR,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unary {
    Neg,
    Not,
    Len,
    BNot,
    Peek,
    Peek2,
    Peek4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Lt,
    Le,
}

#[derive(Clone, Copy, Debug)]
pub enum Op {
    Move(u8, u8),
    LoadK(u8, u16),
    /// Registers a.. a+n-1 to nil.
    LoadNil(u8, u8),
    LoadBool(u8, bool),
    GetUp(u8, u16),
    SetUp(u16, Rk),
    /// A captured local, through the cell in its register.
    GetCell(u8, u8),
    SetCell(u8, Rk),
    /// Puts the register's value in a new cell, which the register then holds.
    NewCell(u8),
    GetGlobal(u8, u16),
    SetGlobal(u16, Rk),
    GetTable(u8, u8, Rk),
    SetTable(u8, Rk, Rk),
    NewTable(u8, u16, u16),
    /// t = R[a]; t[start + i] = R[a + 1 + i] for n values (0: up to the top).
    SetList(u8, u8, u32),
    /// R[a + 1] = R[b]; R[a] = R[b][key]: a method call's start.
    Method(u8, u8, Rk),
    Arith(Arith, u8, Rk, Rk),
    /// The commonest of them, an instruction each: one dispatch, not two.
    Add(u8, Rk, Rk),
    Sub(u8, Rk, Rk),
    Mul(u8, Rk, Rk),
    Div(u8, Rk, Rk),
    Mod(u8, Rk, Rk),
    Un(Unary, u8, u8),
    Concat(u8, Rk, Rk),
    /// R[a] = (b op c) == true.
    Compare(Cmp, u8, Rk, Rk),
    /// Jumps when (b op c) is the flag.
    Branch(Cmp, bool, Rk, Rk, u32),
    Jmp(u32),
    /// Jumps when R[a]'s truth is the flag.
    Test(u8, bool, u32),
    /// The function in R[a], the arguments after it: n + 1 of them (0:
    /// up to the top), the results wanted + 1 (0: all, setting the top).
    Call(u8, u8, u8),
    Return(u8, u8),
    Closure(u8, u16),
    VarArg(u8, u8),
    ForPrep(u8, u32),
    ForLoop(u8, u32),
    /// R[a](R[a+1]...) for one of api::INTRINSICS (its number), with n
    /// arguments and the results wanted + 1 as Call's: done in place while
    /// R[a] is still that native, else called as Call does.
    Intrinsic(u8, u8, u8, u8),
    /// Calls R[a](R[a + 1], R[a + 2]) for a generic for's n variables.
    TForCall(u8, u8),
    TForLoop(u8, u32),
}

#[derive(Clone, Copy, Debug)]
pub enum Up {
    /// The cell in a register of the function that makes the closure.
    Reg(u8),
    /// One of that function's own captured cells.
    Up(u16),
}

pub struct Proto {
    pub code: Vec<Op>,
    pub lines: Vec<u32>,
    pub consts: Vec<Value>,
    pub protos: Vec<Rc<Proto>>,
    pub upvals: Vec<Up>,
    pub params: u8,
    pub vararg: bool,
    pub regs: u16,
    /// For each instruction that looks a string key up: the table entry it
    /// found last time, plus one.
    pub hints: Box<[std::cell::Cell<u32>]>,
    /// For messages.
    #[allow(dead_code)]
    pub name: String,
    /// Its place among all of a cartridge's functions, for save states.
    pub id: u32,
}

/// What a native returns, apart from errors.
pub enum Ret {
    None,
    One(Value),
    Two(Value, Value),
    /// The values are in `Vm::ret`.
    Many,
}

/// Why a native stops the Lua code that called it.
pub enum Ctl {
    Error(String),
    Yield,
    Flip,
}

impl From<String> for Ctl {
    fn from(s: String) -> Ctl {
        Ctl::Error(s)
    }
}
impl From<&str> for Ctl {
    fn from(s: &str) -> Ctl {
        Ctl::Error(s.into())
    }
}

/// How the run loop stopped.
pub enum Exit {
    Return(Vec<Value>),
    Yield(Vec<Value>),
    Flip,
}

/// Interned names the machine looks up itself.
pub struct Names {
    pub index: Value,
    pub newindex: Value,
    pub call: Value,
    pub len: Value,
    pub concat: Value,
    pub eq: Value,
    pub lt: Value,
    pub le: Value,
    pub unm: Value,
    pub bnot: Value,
    pub tostring: Value,
    pub pairs: Value,
    pub arith: [Value; 15],
    pub next: u16,
    pub inext: u16,
    pub all_next: u16,
}

const MAX_FRAMES: usize = 8000;
const MAX_NESTED: u32 = 120;
/// Back jumps and calls between checks of the time and the heap.
const SLICE: u32 = 4096;

pub struct Vm {
    pub heap: Heap,
    pub globals: u32,
    /// The thread running now.
    pub thread: Thread,
    /// The coroutine running now; None for the main thread.
    pub co: Option<u32>,
    /// The threads that resumed the one running, innermost last.
    parked: Vec<(Option<u32>, Thread)>,
    pub ret: Vec<Value>,
    /// Where the values of the last call with all its results end.
    top: usize,
    pub names: Names,
    pub m: Machine,
    /// Values the console keeps (menu callbacks), for the collector.
    pub registry: Vec<Value>,
    /// Slices of work left before the frame counts as stuck.
    pub slices: u32,
    ticks: u32,
    /// Slices run in this frame, for stat(1).
    pub work: u32,
    gc_at: usize,
    nested_total: u32,
}

fn type_name(v: Value) -> &'static str {
    match v {
        Value::Nil => "nil",
        Value::Bool(_) => "boolean",
        Value::Num(_) => "number",
        Value::Str(_) => "string",
        Value::Table(_) => "table",
        Value::Func(_) | Value::Native(_) => "function",
        Value::Thread(_) => "thread",
        Value::Cell(_) => "cell",
    }
}

impl Vm {
    pub fn new(m: Machine) -> Vm {
        let mut heap = Heap::default();
        let mut name = |s: &str| {
            let v = heap.intern(s.as_bytes());
            heap.pin(v);
            v
        };
        let names = Names {
            index: name("__index"),
            newindex: name("__newindex"),
            call: name("__call"),
            len: name("__len"),
            concat: name("__concat"),
            eq: name("__eq"),
            lt: name("__lt"),
            le: name("__le"),
            unm: name("__unm"),
            bnot: name("__bnot"),
            tostring: name("__tostring"),
            pairs: name("__pairs"),
            arith: [
                name("__add"),
                name("__sub"),
                name("__mul"),
                name("__div"),
                name("__idiv"),
                name("__mod"),
                name("__pow"),
                name("__and"),
                name("__or"),
                name("__xor"),
                name("__shl"),
                name("__shr"),
                name("__lshr"),
                name("__rotl"),
                name("__rotr"),
            ],
            next: native("next"),
            inext: native("inext"),
            all_next: native("__all_next"),
        };
        let Value::Table(globals) = heap.table(Table::default()) else { unreachable!() };
        let mut vm = Vm {
            heap,
            globals,
            thread: Thread::new(None),
            co: None,
            parked: Vec::new(),
            ret: Vec::new(),
            top: 0,
            names,
            m,
            registry: Vec::new(),
            slices: u32::MAX,
            ticks: SLICE,
            work: 0,
            gc_at: 20_000,
            nested_total: 0,
        };
        vm.thread.status = Status::Running;
        // _ENV, where a function has no local of that name: the globals.
        let env = vm.heap.intern(b"_ENV");
        vm.heap.pin(env);
        vm.heap.tab_mut(globals).set(env, Value::Table(globals));
        for (i, (n, _)) in NATIVES.iter().enumerate() {
            if !n.starts_with("__") {
                let k = vm.heap.intern(n.as_bytes());
                vm.heap.pin(k);
                vm.heap.tab_mut(globals).set(k, Value::Native(i as u16));
            }
        }
        vm
    }

    pub fn global(&self, name: &str) -> Value {
        // Only an interned name can be a key.
        match self.heap_lookup(name.as_bytes()) {
            Some(k) => self.heap.tab(self.globals).get(k),
            None => Value::Nil,
        }
    }
    fn heap_lookup(&self, s: &[u8]) -> Option<Value> {
        self.heap.find(s)
    }

    /// The share of a PICO-8 frame's time the work so far took, for
    /// stat(1): about 16 000 back jumps and calls fill a 30 Hz frame.
    pub fn cpu(&self) -> f64 {
        let done = f64::from(self.work) * f64::from(SLICE) + f64::from(SLICE - self.ticks);
        done / (16_000.0 * 30.0 / f64::from(self.m.fps.max(1)))
    }
    pub fn set_global(&mut self, name: &[u8], v: Value) {
        let k = self.heap.intern(name);
        self.heap.pin(k);
        self.heap.tab_mut(self.globals).set(k, v);
    }

    pub fn str(&mut self, s: &[u8]) -> Value {
        self.heap.intern(s)
    }

    // Values to text and numbers.

    /// tostr's text of a value (without __tostring).
    pub fn text(&self, v: Value) -> Vec<u8> {
        match v {
            Value::Nil => b"[nil]".to_vec(),
            Value::Bool(b) => if b { b"true".to_vec() } else { b"false".to_vec() },
            Value::Num(n) => fix::to_string(n).into_bytes(),
            Value::Str(s) => self.heap.str(s).to_vec(),
            Value::Table(_) => b"[table]".to_vec(),
            Value::Func(_) | Value::Native(_) => b"[function]".to_vec(),
            Value::Thread(_) => b"[thread]".to_vec(),
            Value::Cell(_) => b"[cell]".to_vec(),
        }
    }

    /// A number, or a string that reads as one.
    pub fn number(&self, v: Value) -> Option<i32> {
        match v {
            Value::Num(n) => Some(n),
            Value::Str(s) => fix::parse(self.heap.str(s)),
            _ => None,
        }
    }

    fn meta(&self, v: Value, name: Value) -> Value {
        match v {
            Value::Table(t) => match self.heap.tab(t).meta {
                Some(m) => self.heap.tab(m).get(name),
                None => Value::Nil,
            },
            _ => Value::Nil,
        }
    }

    // Tables, with their metamethods.

    pub fn index(&mut self, obj: Value, key: Value) -> Result<Value, String> {
        let mut obj = obj;
        for _ in 0..100 {
            let handler = match obj {
                Value::Table(t) => {
                    let tab = self.heap.tab(t);
                    let v = tab.get(key);
                    if v != Value::Nil {
                        return Ok(v);
                    }
                    match tab.meta {
                        None => return Ok(Value::Nil),
                        Some(m) => {
                            let h = self.heap.tab(m).get(self.names.index);
                            if h == Value::Nil {
                                return Ok(Value::Nil);
                            }
                            h
                        }
                    }
                }
                Value::Str(s) => {
                    // PICO-8 0.2.x: s[i] is sub(s, i, i).
                    let Some(i) = self.number(key) else { return Ok(Value::Nil) };
                    let bytes = self.heap.str(s);
                    let i = fix::floor(i);
                    let i = if i < 0 { bytes.len() as i32 + i + 1 } else { i };
                    if i < 1 || i as usize > bytes.len() {
                        return Ok(self.heap.intern(b""));
                    }
                    let c = [bytes[i as usize - 1]];
                    return Ok(self.heap.intern(&c));
                }
                _ => return Err(format!("attempt to index a {} value", type_name(obj))),
            };
            if let Value::Table(_) = handler {
                obj = handler;
            } else {
                return Ok(self.call(handler, &[obj, key])?.first().copied().unwrap_or_default());
            }
        }
        Err("'__index' chain too long; possible loop".into())
    }

    pub fn set_index(&mut self, obj: Value, key: Value, val: Value) -> Result<(), String> {
        let mut obj = obj;
        for _ in 0..100 {
            let handler = match obj {
                Value::Table(t) => {
                    let tab = self.heap.tab(t);
                    let meta = tab.meta;
                    let handler = match meta {
                        Some(m) if tab.get(key) == Value::Nil => self.heap.tab(m).get(self.names.newindex),
                        _ => Value::Nil,
                    };
                    if handler == Value::Nil {
                        if key == Value::Nil {
                            return Err("table index is nil".into());
                        }
                        self.heap.tab_mut(t).set(key, val);
                        return Ok(());
                    }
                    handler
                }
                _ => return Err(format!("attempt to index a {} value", type_name(obj))),
            };
            if let Value::Table(_) = handler {
                obj = handler;
            } else {
                self.call(handler, &[obj, key, val])?;
                return Ok(());
            }
        }
        Err("'__newindex' chain too long; possible loop".into())
    }

    pub fn len(&mut self, v: Value) -> Result<Value, String> {
        match v {
            Value::Str(s) => Ok(Value::int(self.heap.str(s).len() as i32)),
            Value::Table(t) => {
                let h = self.meta(v, self.names.len);
                if h != Value::Nil {
                    return Ok(self.call(h, &[v])?.first().copied().unwrap_or_default());
                }
                Ok(Value::int(self.heap.tab(t).len() as i32))
            }
            _ => Err(format!("attempt to get length of a {} value", type_name(v))),
        }
    }

    pub fn equal(&mut self, a: Value, b: Value) -> Result<bool, String> {
        if a == b {
            return Ok(true);
        }
        if let (Value::Table(_), Value::Table(_)) = (a, b) {
            let mut h = self.meta(a, self.names.eq);
            if h == Value::Nil {
                h = self.meta(b, self.names.eq);
            }
            if h != Value::Nil {
                return Ok(self.call(h, &[a, b])?.first().is_some_and(|v| v.truthy()));
            }
        }
        Ok(false)
    }

    pub fn less(&mut self, a: Value, b: Value, or_equal: bool) -> Result<bool, String> {
        match (a, b) {
            (Value::Num(x), Value::Num(y)) => return Ok(if or_equal { x <= y } else { x < y }),
            (Value::Str(x), Value::Str(y)) => {
                let (x, y) = (self.heap.str(x), self.heap.str(y));
                return Ok(if or_equal { x <= y } else { x < y });
            }
            _ => {}
        }
        let name = if or_equal { self.names.le } else { self.names.lt };
        let mut h = self.meta(a, name);
        if h == Value::Nil {
            h = self.meta(b, name);
        }
        if h != Value::Nil {
            return Ok(self.call(h, &[a, b])?.first().is_some_and(|v| v.truthy()));
        }
        if or_equal {
            // a <= b as not (b < a), as Lua 5.2 does without __le.
            let lt = self.names.lt;
            let mut h = self.meta(b, lt);
            if h == Value::Nil {
                h = self.meta(a, lt);
            }
            if h != Value::Nil {
                return Ok(!self.call(h, &[b, a])?.first().is_some_and(|v| v.truthy()));
            }
        }
        let (ta, tb) = (type_name(a), type_name(b));
        Err(if ta == tb { format!("attempt to compare two {ta} values") } else { format!("attempt to compare {ta} with {tb}") })
    }

    pub fn arith(&mut self, op: Arith, a: Value, b: Value) -> Result<Value, String> {
        if let (Some(x), Some(y)) = (self.number(a), self.number(b)) {
            return Ok(Value::Num(arith(op, x, y)));
        }
        let name = self.names.arith[op as usize];
        let mut h = self.meta(a, name);
        if h == Value::Nil {
            h = self.meta(b, name);
        }
        if h != Value::Nil {
            return Ok(self.call(h, &[a, b])?.first().copied().unwrap_or_default());
        }
        let bad = if self.number(a).is_none() { a } else { b };
        Err(format!("attempt to perform arithmetic on a {} value", type_name(bad)))
    }

    pub fn concat(&mut self, a: Value, b: Value) -> Result<Value, String> {
        let text = |vm: &Vm, v: Value| match v {
            Value::Str(s) => Some(vm.heap.str(s).to_vec()),
            Value::Num(n) => Some(fix::to_string(n).into_bytes()),
            _ => None,
        };
        if let (Some(mut x), Some(y)) = (text(self, a), text(self, b)) {
            x.extend_from_slice(&y);
            return Ok(self.heap.intern(&x));
        }
        let mut h = self.meta(a, self.names.concat);
        if h == Value::Nil {
            h = self.meta(b, self.names.concat);
        }
        if h != Value::Nil {
            return Ok(self.call(h, &[a, b])?.first().copied().unwrap_or_default());
        }
        let bad = if text(self, a).is_none() { a } else { b };
        Err(format!("attempt to concatenate a {} value", type_name(bad)))
    }

    fn unary(&mut self, op: Unary, v: Value) -> Result<Value, String> {
        Ok(match op {
            Unary::Not => Value::Bool(!v.truthy()),
            Unary::Len => self.len(v)?,
            Unary::Neg | Unary::BNot => {
                if let Some(n) = self.number(v) {
                    Value::Num(if op == Unary::Neg { n.wrapping_neg() } else { !n })
                } else {
                    let name = if op == Unary::Neg { self.names.unm } else { self.names.bnot };
                    let h = self.meta(v, name);
                    if h == Value::Nil {
                        return Err(format!("attempt to perform arithmetic on a {} value", type_name(v)));
                    }
                    self.call(h, &[v, v])?.first().copied().unwrap_or_default()
                }
            }
            Unary::Peek | Unary::Peek2 | Unary::Peek4 => {
                let a = self.number(v).map_or(0, fix::floor) as u16 as usize;
                let ram = &self.m.ram;
                let byte = |i: usize| u32::from(ram[(a + i) & 0xffff]);
                match op {
                    Unary::Peek => Value::int(byte(0) as i32),
                    Unary::Peek2 => Value::int((byte(0) | byte(1) << 8) as u16 as i16 as i32),
                    _ => Value::Num((byte(0) | byte(1) << 8 | byte(2) << 16 | byte(3) << 24) as i32),
                }
            }
        })
    }

    // Calls.

    /// Calls f with these arguments from the console's own code, in a
    /// nested run loop, and gives all its results.
    pub fn call(&mut self, f: Value, args: &[Value]) -> Result<Vec<Value>, String> {
        if self.thread.nested >= MAX_NESTED || self.nested_total >= MAX_NESTED {
            return Err("stack overflow (calls within the console's functions)".into());
        }
        let func = self.thread.stack.len();
        let frames = self.thread.frames.len();
        self.thread.stack.push(f);
        self.thread.stack.extend_from_slice(args);
        self.thread.nested += 1;
        self.nested_total += 1;
        let r = match self.call_at(func, args.len(), -1) {
            Ok(true) => {
                self.thread.frames.last_mut().unwrap().boundary = true;
                match self.run() {
                    Ok(Exit::Return(v)) => Ok(v),
                    Ok(_) => Err("attempt to yield across a call from the console".into()),
                    Err(e) => Err(e),
                }
            }
            Ok(false) => Ok(self.thread.stack[func..self.top].to_vec()),
            Err(Ctl::Error(e)) => Err(e),
            Err(Ctl::Yield) => Err("attempt to yield across a call from the console".into()),
            Err(Ctl::Flip) => Ok(Vec::new()),
        };
        self.thread.nested -= 1;
        self.nested_total -= 1;
        self.thread.frames.truncate(frames);
        self.thread.stack.truncate(func);
        r
    }

    /// Starts f on the main thread for the console's frame loop: it may
    /// stop at a flip(), to go on with `resume_main`.
    pub fn start(&mut self, f: Value, args: &[Value]) -> Result<Exit, String> {
        self.thread.frames.clear();
        self.thread.stack.clear();
        self.thread.stack.push(f);
        self.thread.stack.extend_from_slice(args);
        match self.call_at(0, args.len(), -1) {
            Ok(true) => {
                self.thread.frames.last_mut().unwrap().boundary = true;
                let r = self.run();
                if !matches!(r, Ok(Exit::Flip)) {
                    self.thread.frames.clear();
                }
                r
            }
            Ok(false) => Ok(Exit::Return(self.thread.stack[..self.top].to_vec())),
            Err(Ctl::Error(e)) => Err(e),
            Err(_) => Ok(Exit::Return(Vec::new())),
        }
    }

    /// Goes on with the main thread after its flip().
    pub fn resume_main(&mut self) -> Result<Exit, String> {
        if let Some((at, want)) = self.thread.resume_at.take() {
            self.put(at as usize, want, &[]);
        }
        let r = self.run();
        if !matches!(r, Ok(Exit::Flip)) {
            self.thread.frames.clear();
        }
        r
    }

    pub fn suspended(&self) -> bool {
        !self.thread.frames.is_empty()
    }

    /// Resumes a coroutine: its results, or the error that killed it.
    pub fn resume(&mut self, id: u32, args: &[Value]) -> Result<Vec<Value>, String> {
        let status = self.heap.threads.items[id as usize].as_ref().map(|t| t.status);
        match status {
            Some(Status::Suspended) => {}
            Some(Status::Dead) => return Err("cannot resume dead coroutine".into()),
            _ => return Err("cannot resume non-suspended coroutine".into()),
        }
        if self.parked.len() >= 64 {
            return Err("too many coroutines within each other".into());
        }
        let mut th = self.heap.threads.items[id as usize].take().unwrap();
        th.status = Status::Running;
        self.thread.status = Status::Normal;
        let outer = std::mem::replace(&mut self.thread, th);
        self.parked.push((self.co, outer));
        self.co = Some(id);
        let r = self.resume_inner(args);
        let (co, outer) = self.parked.pop().unwrap();
        let mut th = std::mem::replace(&mut self.thread, outer);
        self.co = co;
        self.thread.status = Status::Running;
        let out = match r {
            Ok(Exit::Yield(v)) => {
                th.status = Status::Suspended;
                Ok(v)
            }
            Ok(Exit::Return(v)) => {
                th.status = Status::Dead;
                th.frames.clear();
                th.stack.clear();
                Ok(v)
            }
            Ok(Exit::Flip) => {
                th.status = Status::Dead;
                Ok(Vec::new())
            }
            Err(e) => {
                th.status = Status::Dead;
                th.frames.clear();
                th.stack.clear();
                Err(e)
            }
        };
        self.heap.threads.items[id as usize] = Some(th);
        out
    }

    fn resume_inner(&mut self, args: &[Value]) -> Result<Exit, String> {
        if let Some(f) = self.thread.start.take() {
            self.thread.stack.clear();
            self.thread.stack.push(f);
            self.thread.stack.extend_from_slice(args);
            return match self.call_at(0, args.len(), -1) {
                Ok(true) => {
                    self.thread.frames.last_mut().unwrap().boundary = true;
                    self.run()
                }
                Ok(false) => Ok(Exit::Return(self.thread.stack[..self.top].to_vec())),
                Err(Ctl::Error(e)) => Err(e),
                Err(Ctl::Yield) => Ok(Exit::Yield(std::mem::take(&mut self.ret))),
                Err(Ctl::Flip) => Ok(Exit::Return(Vec::new())),
            };
        }
        if let Some((at, want)) = self.thread.resume_at.take() {
            self.put(at as usize, want, args);
        }
        self.run()
    }

    /// Writes values where a call's results go: `want` of them, or all
    /// (setting the top).
    fn put(&mut self, at: usize, want: i16, values: &[Value]) {
        let n = if want < 0 { values.len() } else { want as usize };
        if self.thread.stack.len() < at + n {
            self.thread.stack.resize(at + n, Value::Nil);
        }
        for i in 0..n {
            self.thread.stack[at + i] = values.get(i).copied().unwrap_or_default();
        }
        if want < 0 {
            self.top = at + n;
        }
    }

    /// Calls the function at stack[func] with the nargs values after it.
    /// For a Lua function, pushes its frame (true); a native runs now and
    /// its results are in place (false).
    fn call_at(&mut self, func: usize, nargs: usize, want: i16) -> Result<bool, Ctl> {
        let f = self.thread.stack[func];
        match f {
            Value::Func(id) => {
                let proto = &self.heap.closures.get(id).proto;
                let (np, vararg, regs) = (proto.params as usize, proto.vararg, proto.regs as usize);
                let base = func + 1;
                let varargs = if vararg && nargs > np { self.thread.stack[base + np..base + nargs].to_vec() } else { Vec::new() };
                let need = base + regs.max(nargs) + 1;
                if self.thread.stack.len() < need {
                    self.thread.stack.resize(need, Value::Nil);
                }
                for i in nargs..np {
                    self.thread.stack[base + i] = Value::Nil;
                }
                if self.thread.frames.len() >= MAX_FRAMES {
                    return Err("stack overflow".into());
                }
                self.thread.frames.push(Frame { closure: id, pc: 0, base: base as u32, want, boundary: false, varargs });
                Ok(true)
            }
            Value::Native(n) => {
                let native = NATIVES[n as usize].1;
                let r = if !api::calls_lua(n) {
                    // A native that runs no Lua (nor the collector) reads its
                    // arguments where they are, the stack lent to it.
                    let stack = std::mem::take(&mut self.thread.stack);
                    let r = native(self, &stack[func + 1..func + 1 + nargs]);
                    self.thread.stack = stack;
                    r
                } else {
                    self.call_native_copy(native, func, nargs)
                }?;
                match r {
                    Ret::None if want == 0 => {}
                    Ret::None => self.put(func, want, &[]),
                    Ret::One(v) if want == 1 => self.thread.stack[func] = v,
                    Ret::One(v) => self.put(func, want, &[v]),
                    Ret::Two(a, b) => self.put(func, want, &[a, b]),
                    Ret::Many => {
                        let v = std::mem::take(&mut self.ret);
                        self.put(func, want, &v);
                        self.ret = v;
                        self.ret.clear();
                    }
                }
                Ok(false)
            }
            _ => {
                let h = self.meta(f, self.names.call);
                if h == Value::Nil {
                    return Err(format!("attempt to call a {} value", type_name(f)).into());
                }
                let end = func + 1 + nargs;
                if self.thread.stack.len() <= end {
                    self.thread.stack.resize(end + 1, Value::Nil);
                }
                self.thread.stack.copy_within(func..end, func + 1);
                self.thread.stack[func] = h;
                self.call_at(func, nargs + 1, want)
            }
        }
    }

    /// Calls a native that may run Lua: its arguments copied off the
    /// stack, which the Lua it runs grows.
    fn call_native_copy(&mut self, native: api::Native, func: usize, nargs: usize) -> Result<Ret, Ctl> {
        let mut buf = [Value::Nil; 12];
        if nargs <= buf.len() {
            buf[..nargs].copy_from_slice(&self.thread.stack[func + 1..func + 1 + nargs]);
            native(self, &buf[..nargs])
        } else {
            let args = self.thread.stack[func + 1..func + 1 + nargs].to_vec();
            native(self, &args)
        }
    }

    /// The name a value had where it was loaded, for an error message.
    fn describe(&self, proto: &Proto, pc: usize, reg: u8) -> String {
        let name = |k: u16| match proto.consts.get(k as usize) {
            Some(&Value::Str(s)) => String::from_utf8_lossy(self.heap.str(s)).into_owned(),
            _ => "?".into(),
        };
        for op in proto.code[..pc].iter().rev().take(16) {
            match *op {
                Op::GetGlobal(a, k) if a == reg => return format!(" (global '{}')", name(k)),
                Op::GetTable(a, _, key) | Op::Method(a, _, key) if a == reg && key & K != 0 => {
                    return format!(" (field '{}')", name(key & !K));
                }
                Op::GetUp(a, _) | Op::GetCell(a, _) | Op::Move(a, _) if a == reg => return " (upvalue or local)".into(),
                Op::GetTable(a, ..) | Op::Call(a, ..) | Op::LoadK(a, _) | Op::LoadNil(a, _) if a == reg => return String::new(),
                _ => {}
            }
        }
        String::new()
    }

    /// The machine between frames, for a save state: the heap, the
    /// globals, the main thread (maybe stopped at a flip), the console.
    pub fn save(&self, w: &mut crate::state::W) {
        self.heap.save(w);
        w.u32(self.globals);
        crate::heap::save_main(w, &self.thread);
        w.values(&self.registry);
        w.u64(self.gc_at as u64);
        self.m.save(w);
    }

    /// Loads a state into a machine fresh from the same cartridge; all of
    /// it is checked before any is kept.
    pub fn load(&mut self, r: &mut crate::state::R, protos: &[Rc<Proto>]) -> crate::state::Res<()> {
        use crate::state::BadState;
        self.heap.load(r, protos)?;
        let globals = r.u32()?;
        let thread = crate::heap::load_main(r)?;
        let registry = r.values()?;
        let gc_at = r.u64()? as usize;
        self.m.load(r)?;
        let natives = NATIVES.len();
        if !self.heap.valid(Value::Table(globals), natives) {
            return Err(BadState("no globals"));
        }
        self.heap.check(natives)?;
        self.heap.check_thread(&thread, natives)?;
        if registry.iter().any(|&v| !self.heap.valid(v, natives)) {
            return Err(BadState("a kept value that points nowhere"));
        }
        if self.heap.threads.items.iter().flatten().any(|t| t.status == Status::Running || t.status == Status::Normal) {
            return Err(BadState("a coroutine left running"));
        }
        self.globals = globals;
        self.thread = thread;
        self.thread.status = Status::Running;
        self.registry = registry;
        self.co = None;
        self.parked.clear();
        self.ret.clear();
        self.top = 0;
        // Collected when it was, as the game it was saved from.
        self.gc_at = gc_at.max(1);
        Ok(())
    }

    /// Marks and sweeps.
    pub fn collect(&mut self) {
        let mut roots = vec![Value::Table(self.globals)];
        roots.extend(self.registry.iter().copied());
        roots.extend(self.ret.iter().copied());
        roots.extend(crate::heap::thread_values(&self.thread));
        for (_, t) in &self.parked {
            roots.extend(crate::heap::thread_values(t));
        }
        self.heap.mark(roots);
        let live = self.heap.sweep();
        self.gc_at = (live * 2).max(20_000);
    }

    /// At a back jump or a call: counts the work, and collects when the
    /// heap grew.
    #[inline(always)]
    fn tick(&mut self) -> Result<(), String> {
        self.ticks -= 1;
        if self.ticks == 0 {
            return self.slice_done();
        }
        Ok(())
    }

    /// A slice of work is done: count it, and collect when the heap grew.
    #[cold]
    #[inline(never)]
    fn slice_done(&mut self) -> Result<(), String> {
        {
            self.ticks = SLICE;
            self.work += 1;
            self.slices = self.slices.saturating_sub(1);
            if self.slices == 0 {
                return Err("the cartridge is stuck: too much work without a frame".into());
            }
            if self.heap.made > self.gc_at || self.heap.bytes > 4 << 20 {
                self.collect();
            }
        }
        Ok(())
    }

    /// A frame begins: its work is counted from nothing, so stat(1) and
    /// when the collector runs depend on the frame alone.
    pub fn begin_frame(&mut self, slices: u32) {
        self.slices = slices;
        self.work = 0;
        self.ticks = SLICE;
    }

    /// Whether the heap should be collected between frames.
    pub fn gc_due(&self) -> bool {
        self.heap.made > self.gc_at / 2 || self.heap.bytes > 1 << 20
    }

    /// Runs the current thread until its boundary frame returns, or it
    /// yields or flips.
    pub fn run(&mut self) -> Result<Exit, String> {
        'frames: loop {
            let fi = self.thread.frames.len() - 1;
            let (cl, base, mut pc) = {
                let f = &self.thread.frames[fi];
                (f.closure, f.base as usize, f.pc as usize)
            };
            let proto = self.heap.closures.get(cl).proto.clone();
            let code = &proto.code[..];
            let k = &proto.consts[..];
            // The stack is at least base + regs long while this frame runs
            // (call_at makes it so, nothing shrinks it under a running
            // frame, and a loaded state is checked for it), and the
            // compiler numbers registers below regs and constants within
            // the function's: so these reads and writes need no checks.
            debug_assert!(self.thread.stack.len() >= base + proto.regs as usize);
            macro_rules! r {
                ($x:expr) => {
                    *{
                        let i = base + $x as usize;
                        debug_assert!(i < self.thread.stack.len());
                        unsafe { self.thread.stack.get_unchecked_mut(i) }
                    }
                };
            }
            macro_rules! rk {
                ($x:expr) => {{
                    let x = $x;
                    if x & K != 0 {
                        debug_assert!(((x & !K) as usize) < k.len());
                        unsafe { *k.get_unchecked((x & !K) as usize) }
                    } else {
                        let i = base + x as usize;
                        debug_assert!(i < self.thread.stack.len());
                        unsafe { *self.thread.stack.get_unchecked(i) }
                    }
                }};
            }
            macro_rules! fail {
                ($e:expr) => {{
                    let e: String = $e;
                    self.thread.frames[fi].pc = pc as u32;
                    return Err(locate(e, &proto, pc - 1));
                }};
            }
            macro_rules! tr {
                ($e:expr) => {
                    match $e {
                        Ok(v) => v,
                        Err(e) => fail!(e),
                    }
                };
            }
            // A call: the function in R[a], b - 1 arguments after it (0:
            // up to the top), c - 1 results wanted (-1: all).
            // An arithmetic instruction of its own: numbers in place, the
            // rest (strings, metamethods) as Arith.
            macro_rules! num_op {
                ($a:expr, $b:expr, $c:expr, $op:expr, $f:expr) => {{
                    let (x, y) = (rk!($b), rk!($c));
                    r!($a) = match (x, y) {
                        (Value::Num(x), Value::Num(y)) => Value::Num($f(x, y)),
                        _ => {
                            self.thread.frames[fi].pc = pc as u32;
                            tr!(self.arith($op, x, y))
                        }
                    };
                }};
            }
            macro_rules! do_call {
                ($a:expr, $b:expr, $c:expr) => {{
                    let (a, b, c): (u8, u8, u8) = ($a, $b, $c);
                        self.thread.frames[fi].pc = pc as u32;
                            tr!(self.tick());
                            let func = base + a as usize;
                            let nargs = if b == 0 { self.top - func - 1 } else { b as usize - 1 };
                            let want = c as i16 - 1;
                            match self.call_at(func, nargs, want) {
                                Ok(true) => continue 'frames,
                                Ok(false) => {}
                                Err(Ctl::Error(e)) => {
                                    let d = if e.starts_with("attempt to call") { self.describe(&proto, pc - 1, a) } else { String::new() };
                                    fail!(format!("{e}{d}"))
                                }
                                Err(Ctl::Yield) => {
                                    if self.co.is_none() {
                                        fail!("attempt to yield from outside a coroutine".into());
                                    }
                                    if self.thread.nested > 0 {
                                        fail!("attempt to yield across a call from the console".into());
                                    }
                                    self.thread.resume_at = Some((func as u32, want));
                                    return Ok(Exit::Yield(std::mem::take(&mut self.ret)));
                                }
                                Err(Ctl::Flip) => {
                                    if self.co.is_none() && self.thread.nested == 0 {
                                        self.thread.resume_at = Some((func as u32, want));
                                        return Ok(Exit::Flip);
                                    }
                                    self.put(func, want, &[]);
                                }
                            }
                }};
            }
            loop {
                // Every function ends with a return, and every jump lands
                // in it (a loaded state's frames are checked for it too).
                debug_assert!(pc < code.len());
                let op = unsafe { *code.get_unchecked(pc) };
                pc += 1;
                match op {
                    Op::Move(a, b) => r!(a) = r!(b),
                    Op::LoadK(a, i) => r!(a) = k[i as usize],
                    Op::LoadNil(a, n) => {
                        for i in 0..n {
                            r!(a + i) = Value::Nil;
                        }
                    }
                    Op::LoadBool(a, b) => r!(a) = Value::Bool(b),
                    Op::GetUp(a, i) => {
                        let c = self.heap.closures.get(cl).upvals[i as usize];
                        r!(a) = *self.heap.cells.get(c);
                    }
                    Op::SetUp(i, b) => {
                        let v = rk!(b);
                        let c = self.heap.closures.get(cl).upvals[i as usize];
                        *self.heap.cells.get_mut(c) = v;
                    }
                    Op::GetCell(a, b) => {
                        let Value::Cell(c) = r!(b) else { fail!("internal error: no cell".into()) };
                        r!(a) = *self.heap.cells.get(c);
                    }
                    Op::SetCell(a, b) => {
                        let v = rk!(b);
                        let Value::Cell(c) = r!(a) else { fail!("internal error: no cell".into()) };
                        *self.heap.cells.get_mut(c) = v;
                    }
                    Op::NewCell(a) => {
                        let v = r!(a);
                        r!(a) = Value::Cell(self.heap.cell(v));
                    }
                    Op::GetGlobal(a, i) => {
                        let key = k[i as usize];
                        let g = self.heap.tab(self.globals);
                        let v = g.get_hinted(key, &proto.hints[pc - 1]);
                        r!(a) = if v == Value::Nil && g.meta.is_some() {
                            tr!(self.index(Value::Table(self.globals), key))
                        } else {
                            v
                        };
                    }
                    Op::SetGlobal(i, b) => {
                        let (key, v) = (k[i as usize], rk!(b));
                        let g = self.heap.tab_mut(self.globals);
                        let hinted = g.meta.is_none() && g.set_hinted(key, v, &proto.hints[pc - 1]);
                        if hinted {
                            // Set where it was.
                        } else if self.heap.tab(self.globals).meta.is_none() {
                            self.heap.tab_mut(self.globals).set(key, v);
                        } else {
                            tr!(self.set_index(Value::Table(self.globals), key, v));
                        }
                    }
                    Op::GetTable(a, b, c) => {
                        let (t, key) = (r!(b), rk!(c));
                        let fast = match t {
                            Value::Table(t) => {
                                let tab = self.heap.tab(t);
                                let v = if let Value::Str(_) = key { tab.get_hinted(key, &proto.hints[pc - 1]) } else { tab.get(key) };
                                if v != Value::Nil || tab.meta.is_none() { Some(v) } else { None }
                            }
                            _ => None,
                        };
                        r!(a) = match fast {
                            Some(v) => v,
                            None => {
                                self.thread.frames[fi].pc = pc as u32;
                                match self.index(t, key) {
                                    Ok(v) => v,
                                    Err(e) => fail!(format!("{e}{}", self.describe(&proto, pc - 1, b))),
                                }
                            }
                        };
                    }
                    Op::SetTable(a, b, c) => {
                        let (t, key, v) = (r!(a), rk!(b), rk!(c));
                        let done = match t {
                            Value::Table(t) if matches!(key, Value::Str(_)) && self.heap.tab(t).meta.is_none() => {
                                let tab = self.heap.tab_mut(t);
                                if !tab.set_hinted(key, v, &proto.hints[pc - 1]) {
                                    tab.set(key, v);
                                }
                                true
                            }
                            Value::Table(t) => {
                                let tab = self.heap.tab(t);
                                if (tab.meta.is_none() || tab.get(key) != Value::Nil) && key != Value::Nil {
                                    self.heap.tab_mut(t).set(key, v);
                                    true
                                } else {
                                    false
                                }
                            }
                            _ => false,
                        };
                        if !done {
                            self.thread.frames[fi].pc = pc as u32;
                            if let Err(e) = self.set_index(t, key, v) {
                                fail!(format!("{e}{}", self.describe(&proto, pc - 1, a)));
                            }
                        }
                    }
                    Op::NewTable(a, n, h) => {
                        r!(a) = self.heap.table(Table::with_capacity(n as usize, h as usize));
                    }
                    Op::SetList(a, n, start) => {
                        let first = base + a as usize + 1;
                        let count = if n == 0 { self.top.saturating_sub(first) } else { n as usize };
                        let Value::Table(t) = r!(a) else { fail!("internal error: no table".into()) };
                        let (heap, stack) = (&mut self.heap, &self.thread.stack);
                        heap.tab_mut(t).set_list(start as usize, &stack[first..first + count]);
                    }
                    Op::Method(a, b, c) => {
                        let (obj, key) = (r!(b), rk!(c));
                        r!(a + 1) = obj;
                        let fast = match obj {
                            Value::Table(t) => {
                                let tab = self.heap.tab(t);
                                let v = tab.get_hinted(key, &proto.hints[pc - 1]);
                                if v != Value::Nil || tab.meta.is_none() { Some(v) } else { None }
                            }
                            _ => None,
                        };
                        r!(a) = match fast {
                            Some(v) => v,
                            None => {
                                self.thread.frames[fi].pc = pc as u32;
                                match self.index(obj, key) {
                                    Ok(v) => v,
                                    Err(e) => fail!(format!("{e}{}", self.describe(&proto, pc - 1, b))),
                                }
                            }
                        };
                    }
                    Op::Add(a, b, c) => num_op!(a, b, c, Arith::Add, |x: i32, y: i32| x.wrapping_add(y)),
                    Op::Sub(a, b, c) => num_op!(a, b, c, Arith::Sub, |x: i32, y: i32| x.wrapping_sub(y)),
                    Op::Mul(a, b, c) => num_op!(a, b, c, Arith::Mul, fix::mul),
                    Op::Div(a, b, c) => num_op!(a, b, c, Arith::Div, fix::div),
                    Op::Mod(a, b, c) => num_op!(a, b, c, Arith::Mod, fix::modulo),
                    Op::Arith(op, a, b, c) => {
                        let (x, y) = (rk!(b), rk!(c));
                        r!(a) = match (x, y) {
                            (Value::Num(x), Value::Num(y)) => Value::Num(arith(op, x, y)),
                            _ => {
                                self.thread.frames[fi].pc = pc as u32;
                                tr!(self.arith(op, x, y))
                            }
                        };
                    }
                    Op::Un(op, a, b) => {
                        let v = r!(b);
                        r!(a) = match (op, v) {
                            (Unary::Neg, Value::Num(n)) => Value::Num(n.wrapping_neg()),
                            (Unary::Not, v) => Value::Bool(!v.truthy()),
                            (Unary::Len, Value::Table(t)) if self.heap.tab(t).meta.is_none() => {
                                Value::int(self.heap.tab(t).len() as i32)
                            }
                            _ => {
                                self.thread.frames[fi].pc = pc as u32;
                                match self.unary(op, v) {
                                    Ok(v) => v,
                                    Err(e) => fail!(format!("{e}{}", self.describe(&proto, pc - 1, b))),
                                }
                            }
                        };
                    }
                    Op::Concat(a, b, c) => {
                        let (x, y) = (rk!(b), rk!(c));
                        self.thread.frames[fi].pc = pc as u32;
                        r!(a) = tr!(self.concat(x, y));
                    }
                    Op::Compare(op, a, b, c) => {
                        let (x, y) = (rk!(b), rk!(c));
                        self.thread.frames[fi].pc = pc as u32;
                        r!(a) = Value::Bool(tr!(self.compare(op, x, y)));
                    }
                    Op::Branch(op, flag, b, c, target) => {
                        let (x, y) = (rk!(b), rk!(c));
                        let t = match (op, x, y) {
                            (Cmp::Eq, x, y) if x == y => true,
                            (Cmp::Eq, Value::Table(_), Value::Table(_)) => {
                                self.thread.frames[fi].pc = pc as u32;
                                tr!(self.equal(x, y))
                            }
                            (Cmp::Eq, ..) => false,
                            (Cmp::Lt, Value::Num(x), Value::Num(y)) => x < y,
                            (Cmp::Le, Value::Num(x), Value::Num(y)) => x <= y,
                            _ => {
                                self.thread.frames[fi].pc = pc as u32;
                                tr!(self.compare(op, x, y))
                            }
                        };
                        if t == flag {
                            if (target as usize) < pc {
                                tr!(self.tick());
                            }
                            pc = target as usize;
                        }
                    }
                    Op::Jmp(target) => {
                        if (target as usize) < pc {
                            self.thread.frames[fi].pc = pc as u32;
                            tr!(self.tick());
                        }
                        pc = target as usize;
                    }
                    Op::Test(a, flag, target) => {
                        if r!(a).truthy() == flag {
                            if (target as usize) < pc {
                                tr!(self.tick());
                            }
                            pc = target as usize;
                        }
                    }
                    Op::Intrinsic(id, a, nargs, c) => {
                        let s = base + a as usize + 1;
                        if r!(a) == Value::Native(api::intrinsic_native(id))
                            && let Some(v) = api::intrinsic(&mut self.m, id, &self.thread.stack[s..s + nargs as usize])
                        {
                            if c == 2 {
                                r!(a) = v;
                            }
                            continue;
                        }
                        do_call!(a, nargs + 1, c);
                    }
                    Op::Call(a, b, c) => do_call!(a, b, c),
                    Op::Return(a, b) => {
                        let start = base + a as usize;
                        let n = if b == 0 { self.top - start } else { b as usize - 1 };
                        let frame = self.thread.frames.pop().unwrap();
                        if frame.boundary {
                            return Ok(Exit::Return(self.thread.stack[start..start + n].to_vec()));
                        }
                        let dst = base - 1;
                        self.thread.stack.copy_within(start..start + n, dst);
                        if frame.want < 0 {
                            self.top = dst + n;
                        } else {
                            for i in n..frame.want as usize {
                                self.thread.stack[dst + i] = Value::Nil;
                            }
                        }
                        continue 'frames;
                    }
                    Op::Closure(a, i) => {
                        let p = proto.protos[i as usize].clone();
                        let upvals = p
                            .upvals
                            .iter()
                            .map(|u| match *u {
                                Up::Reg(r) => match r!(r) {
                                    Value::Cell(c) => c,
                                    _ => u32::MAX,
                                },
                                Up::Up(i) => self.heap.closures.get(cl).upvals[i as usize],
                            })
                            .collect::<Box<[u32]>>();
                        if upvals.contains(&u32::MAX) {
                            fail!("internal error: a captured local has no cell".into());
                        }
                        r!(a) = self.heap.closure(Closure { proto: p, upvals });
                    }
                    Op::VarArg(a, b) => {
                        let start = base + a as usize;
                        let va = std::mem::take(&mut self.thread.frames[fi].varargs);
                        let want = b as i16 - 1;
                        self.put(start, want, &va);
                        self.thread.frames[fi].varargs = va;
                    }
                    Op::ForPrep(a, exit) => {
                        let mut nums = [0i32; 3];
                        for (i, n) in nums.iter_mut().enumerate() {
                            let v = r!(a + i as u8);
                            match self.number(v) {
                                Some(v) => *n = v,
                                None => fail!(format!("'for' {} must be a number", ["initial value", "limit", "step"][i])),
                            }
                        }
                        let [i, l, s] = nums;
                        r!(a) = Value::Num(i);
                        r!(a + 1) = Value::Num(l);
                        r!(a + 2) = Value::Num(s);
                        if (s > 0 && i > l) || (s <= 0 && i < l) {
                            pc = exit as usize;
                        } else {
                            r!(a + 3) = Value::Num(i);
                        }
                    }
                    Op::ForLoop(a, body) => {
                        // The loop's own registers, which no code of the game's
                        // can reach: numbers since ForPrep (or a loaded state,
                        // whose values are numbers or this fails).
                        let num = |v: Value| if let Value::Num(n) = v { n } else { 0 };
                        let (i, l, s) = (num(r!(a)), num(r!(a + 1)), num(r!(a + 2)));
                        let next = i64::from(i) + i64::from(s);
                        if (s > 0 && next <= i64::from(l)) || (s <= 0 && next >= i64::from(l)) {
                            r!(a) = Value::Num(next as i32);
                            r!(a + 3) = Value::Num(next as i32);
                            tr!(self.tick());
                            pc = body as usize;
                        }
                    }
                    Op::TForCall(a, n) => {
                        let (f, s, ctl) = (r!(a), r!(a + 1), r!(a + 2));
                        let dst = a + 3;
                        if let (Value::Native(id), Value::Table(t)) = (f, s) {
                            if id == self.names.next {
                                match self.heap.tab(t).next(ctl) {
                                    Ok(Some((key, v))) => {
                                        r!(dst) = key;
                                        if n >= 2 {
                                            r!(dst + 1) = v;
                                        }
                                    }
                                    Ok(None) => r!(dst) = Value::Nil,
                                    Err(()) => fail!("invalid key to 'next'".into()),
                                }
                                for i in 2..n {
                                    r!(dst + i) = Value::Nil;
                                }
                                continue;
                            }
                            if id == self.names.all_next {
                                r!(dst) = api::all_next(&mut self.heap, t, ctl);
                                for i in 1..n {
                                    r!(dst + i) = Value::Nil;
                                }
                                continue;
                            }
                        }
                        r!(dst) = f;
                        r!(dst + 1) = s;
                        r!(dst + 2) = ctl;
                        self.thread.frames[fi].pc = pc as u32;
                        tr!(self.tick());
                        match self.call_at(base + dst as usize, 2, n as i16) {
                            Ok(true) => continue 'frames,
                            Ok(false) => {}
                            Err(Ctl::Error(e)) => fail!(e),
                            Err(_) => fail!("cannot yield or flip from a for loop's iterator".into()),
                        }
                    }
                    Op::TForLoop(a, body) => {
                        let v = r!(a + 3);
                        if v != Value::Nil {
                            r!(a + 2) = v;
                            pc = body as usize;
                        }
                    }
                }
            }
        }
    }

    fn compare(&mut self, op: Cmp, a: Value, b: Value) -> Result<bool, String> {
        match op {
            Cmp::Eq => self.equal(a, b),
            Cmp::Lt => self.less(a, b, false),
            Cmp::Le => self.less(a, b, true),
        }
    }
}

fn native(name: &str) -> u16 {
    NATIVES.iter().position(|(n, _)| *n == name).expect("a native the machine uses") as u16
}

#[inline(always)]
pub fn arith(op: Arith, x: i32, y: i32) -> i32 {
    match op {
        Arith::Add => x.wrapping_add(y),
        Arith::Sub => x.wrapping_sub(y),
        Arith::Mul => fix::mul(x, y),
        Arith::Div => fix::div(x, y),
        Arith::IDiv => fix::idiv(x, y),
        Arith::Mod => fix::modulo(x, y),
        Arith::Pow => fix::pow(x, y),
        Arith::BAnd => x & y,
        Arith::BOr => x | y,
        Arith::BXor => x ^ y,
        Arith::Shl => fix::shl(x, y),
        Arith::Shr => fix::shr(x, y),
        Arith::LShr => fix::lshr(x, y),
        Arith::RotL => fix::rotl(x, y),
        Arith::RotR => fix::rotr(x, y),
    }
}

/// An error with the line it happened on, once.
fn locate(e: String, proto: &Proto, pc: usize) -> String {
    if e.starts_with("line ") || e.starts_with('\0') {
        return e;
    }
    match proto.lines.get(pc) {
        Some(&line) if line > 0 => format!("line {line}: {e}"),
        _ => e,
    }
}
