//! The syntax tree to register instructions, a function at a time.
//!
//! Locals live in registers numbered in the order they come into scope;
//! the values an expression needs on the way are in registers above them,
//! given back when it is done. A local that a closure captures is kept in
//! a cell from the start (`NewCell`), and read and written through it, so
//! closures share it with no further work when its scope ends.

use std::collections::HashMap;
use std::rc::Rc;

use crate::heap::{Fx, Heap, Value};
use crate::parse::{BinOp, Block, Chunk, Decl, Expr, Field, Func, Stmt, UnOp};
use crate::vm::{Arith, Cmp, K, Op, Proto, Rk, Unary, Up};

const MAX_REGS: u16 = 250;

struct Local {
    decl: u32,
    reg: u8,
    cell: bool,
}

struct Scope {
    actives: usize,
    free: u16,
    is_loop: bool,
    breaks: Vec<usize>,
    labels: Vec<(Box<[u8]>, usize)>,
    gotos: Vec<(Box<[u8]>, usize, u32)>,
}

struct FuncState {
    code: Vec<Op>,
    lines: Vec<u32>,
    consts: Vec<Value>,
    const_index: HashMap<Value, u16, Fx>,
    protos: Vec<Rc<Proto>>,
    upvals: Vec<(u32, Up)>,
    actives: Vec<Local>,
    scopes: Vec<Scope>,
    free: u16,
    max: u16,
}

struct Compiler<'a> {
    decls: &'a [Decl],
    heap: &'a mut Heap,
    fs: Vec<FuncState>,
    line: u32,
    next_id: u32,
}

enum Var {
    Reg(u8),
    Cell(u8),
    Up(u16),
}

type R<T> = Result<T, String>;

pub fn compile(chunk: &Chunk, heap: &mut Heap) -> R<Rc<Proto>> {
    let mut c = Compiler { decls: &chunk.decls, heap, fs: Vec::new(), line: 0, next_id: 0 };
    c.function(&chunk.main)
}

/// Whether compiling e straight into a local's register writes it only
/// at the end, after every read of it: all but `and` and `or`.
fn single_write(e: &Expr) -> bool {
    match e {
        Expr::And(..) | Expr::Or(..) => false,
        Expr::Paren(e) => single_write(e),
        _ => true,
    }
}

/// An arithmetic instruction: + - * their own, the rest Arith.
fn arith_op(op: BinOp, dst: u8, a: Rk, b: Rk) -> Op {
    match op {
        BinOp::Add => Op::Add(dst, a, b),
        BinOp::Sub => Op::Sub(dst, a, b),
        BinOp::Mul => Op::Mul(dst, a, b),
        BinOp::Div => Op::Div(dst, a, b),
        BinOp::Mod => Op::Mod(dst, a, b),
        op => Op::Arith(arith(op).unwrap(), dst, a, b),
    }
}

fn multi(e: &Expr) -> bool {
    matches!(e, Expr::Call(..) | Expr::Method(..) | Expr::Vararg)
}

fn arith(op: BinOp) -> Option<Arith> {
    Some(match op {
        BinOp::Add => Arith::Add,
        BinOp::Sub => Arith::Sub,
        BinOp::Mul => Arith::Mul,
        BinOp::Div => Arith::Div,
        BinOp::IDiv => Arith::IDiv,
        BinOp::Mod => Arith::Mod,
        BinOp::Pow => Arith::Pow,
        BinOp::BAnd => Arith::BAnd,
        BinOp::BOr => Arith::BOr,
        BinOp::BXor => Arith::BXor,
        BinOp::Shl => Arith::Shl,
        BinOp::Shr => Arith::Shr,
        BinOp::LShr => Arith::LShr,
        BinOp::RotL => Arith::RotL,
        BinOp::RotR => Arith::RotR,
        _ => return None,
    })
}

impl Compiler<'_> {
    fn f(&mut self) -> &mut FuncState {
        self.fs.last_mut().unwrap()
    }

    fn emit(&mut self, op: Op) -> usize {
        let line = self.line;
        let f = self.f();
        f.code.push(op);
        f.lines.push(line);
        f.code.len() - 1
    }

    fn pc(&mut self) -> usize {
        self.f().code.len()
    }

    /// Points the jump at i to target.
    fn patch(&mut self, i: usize, target: usize) {
        let t = target as u32;
        let op = &mut self.f().code[i];
        match op {
            Op::Jmp(x) | Op::Test(_, _, x) | Op::Branch(_, _, _, _, x) | Op::ForPrep(_, x) => *x = t,
            _ => unreachable!("not a jump"),
        }
    }
    fn patch_here(&mut self, jumps: Vec<usize>) {
        let here = self.pc();
        for j in jumps {
            self.patch(j, here);
        }
    }

    fn alloc(&mut self, n: u16) -> R<u8> {
        let f = self.f();
        let r = f.free;
        f.free += n;
        if f.free > MAX_REGS {
            return Err(format!("line {}: function or expression too complex", self.line));
        }
        let f = self.f();
        f.max = f.max.max(f.free);
        Ok(r as u8)
    }
    fn free(&self) -> u16 {
        self.fs.last().unwrap().free
    }
    fn set_free(&mut self, n: u16) {
        self.f().free = n;
    }

    fn constant(&mut self, v: Value) -> R<u16> {
        if let Some(&i) = self.f().const_index.get(&v) {
            return Ok(i);
        }
        let f = self.f();
        let i = f.consts.len();
        if i >= 0x7fff {
            return Err("too many constants in a function".into());
        }
        f.consts.push(v);
        f.const_index.insert(v, i as u16);
        Ok(i as u16)
    }
    fn string(&mut self, s: &[u8]) -> R<u16> {
        let v = self.heap.intern(s);
        self.heap.pin(v);
        self.constant(v)
    }

    // Variables.

    fn resolve(&mut self, decl: u32) -> Var {
        let level = self.fs.len() - 1;
        if let Some(l) = self.fs[level].actives.iter().rev().find(|l| l.decl == decl) {
            return if l.cell { Var::Cell(l.reg) } else { Var::Reg(l.reg) };
        }
        Var::Up(self.upval(level, decl))
    }

    fn upval(&mut self, level: usize, decl: u32) -> u16 {
        if let Some(i) = self.fs[level].upvals.iter().position(|(d, _)| *d == decl) {
            return i as u16;
        }
        let parent = level - 1;
        let src = match self.fs[parent].actives.iter().rev().find(|l| l.decl == decl) {
            Some(l) => Up::Reg(l.reg),
            None => Up::Up(self.upval(parent, decl)),
        };
        self.fs[level].upvals.push((decl, src));
        self.fs[level].upvals.len() as u16 - 1
    }

    /// Brings a local into scope in register reg, putting it in a cell
    /// when a closure captures it.
    fn activate(&mut self, decl: u32, reg: u8) {
        let cell = self.decls[decl as usize].captured;
        if cell {
            self.emit(Op::NewCell(reg));
        }
        self.f().actives.push(Local { decl, reg, cell });
    }

    // Expressions.

    /// e's value in register dst (allocated, or a local's).
    fn exp_to(&mut self, e: &Expr, dst: u8) -> R<()> {
        let save = self.free();
        match e {
            Expr::Nil => {
                self.emit(Op::LoadNil(dst, 1));
            }
            Expr::True => {
                self.emit(Op::LoadBool(dst, true));
            }
            Expr::False => {
                self.emit(Op::LoadBool(dst, false));
            }
            Expr::Num(n) => {
                let k = self.constant(Value::Num(*n))?;
                self.emit(Op::LoadK(dst, k));
            }
            Expr::Str(s) => {
                let k = self.string(s)?;
                self.emit(Op::LoadK(dst, k));
            }
            Expr::Vararg => {
                self.emit(Op::VarArg(dst, 2));
            }
            Expr::Local(d) => match self.resolve(*d) {
                Var::Reg(r) => {
                    if r != dst {
                        self.emit(Op::Move(dst, r));
                    }
                }
                Var::Cell(r) => {
                    self.emit(Op::GetCell(dst, r));
                }
                Var::Up(i) => {
                    self.emit(Op::GetUp(dst, i));
                }
            },
            Expr::Global(n) => {
                let k = self.string(n)?;
                self.emit(Op::GetGlobal(dst, k));
            }
            Expr::Index(t, key) => {
                let tr = self.exp_any(t)?;
                let kr = self.exp_rk(key)?;
                self.emit(Op::GetTable(dst, tr, kr));
                // PICO-8's own functions are there even when _ENV is a
                // table without them.
                if let (Expr::Local(d), Expr::Str(name)) = (&**t, &**key)
                    && &*self.decls[*d as usize].name == b"_ENV"
                    && crate::api::NATIVES.iter().any(|(n, _)| n.as_bytes() == &**name)
                {
                    let j = self.emit(Op::Test(dst, true, 0));
                    self.emit(Op::GetGlobal(dst, kr & !K));
                    self.patch_here(vec![j]);
                }
            }
            Expr::Call(..) | Expr::Method(..) => {
                let base = self.call(e, 1)?;
                if base != dst {
                    self.emit(Op::Move(dst, base));
                }
            }
            Expr::Function(f) => {
                let i = self.closure(f)?;
                self.emit(Op::Closure(dst, i));
            }
            Expr::Bin(op, a, b) => {
                let ra = self.exp_rk(a)?;
                let rb = self.exp_rk(b)?;
                match *op {
                    BinOp::Concat => {
                        self.emit(Op::Concat(dst, ra, rb));
                    }
                    BinOp::Eq => {
                        self.emit(Op::Compare(Cmp::Eq, dst, ra, rb));
                    }
                    BinOp::Ne => {
                        self.emit(Op::Compare(Cmp::Eq, dst, ra, rb));
                        self.emit(Op::Un(Unary::Not, dst, dst));
                    }
                    BinOp::Lt => {
                        self.emit(Op::Compare(Cmp::Lt, dst, ra, rb));
                    }
                    BinOp::Le => {
                        self.emit(Op::Compare(Cmp::Le, dst, ra, rb));
                    }
                    BinOp::Gt => {
                        self.emit(Op::Compare(Cmp::Lt, dst, rb, ra));
                    }
                    BinOp::Ge => {
                        self.emit(Op::Compare(Cmp::Le, dst, rb, ra));
                    }
                    op => {
                        self.emit(arith_op(op, dst, ra, rb));
                    }
                }
            }
            Expr::And(a, b) | Expr::Or(a, b) => {
                let is_and = matches!(e, Expr::And(..));
                self.exp_to(a, dst)?;
                let j = self.emit(Op::Test(dst, !is_and, 0));
                self.exp_to(b, dst)?;
                self.patch_here(vec![j]);
            }
            Expr::Un(op, x) => {
                let r = self.exp_any(x)?;
                let op = match op {
                    UnOp::Neg => Unary::Neg,
                    UnOp::Not => Unary::Not,
                    UnOp::Len => Unary::Len,
                    UnOp::BNot => Unary::BNot,
                    UnOp::Peek => Unary::Peek,
                    UnOp::Peek2 => Unary::Peek2,
                    UnOp::Peek4 => Unary::Peek4,
                };
                self.emit(Op::Un(op, dst, r));
            }
            Expr::Table(fields) => {
                if u16::from(dst) + 1 != self.free() {
                    // Its items go in the registers after it.
                    let t = self.alloc(1)?;
                    self.exp_to(e, t)?;
                    self.emit(Op::Move(dst, t));
                } else {
                    self.table(fields, dst)?;
                }
            }
            Expr::Paren(x) => self.exp_to(x, dst)?,
        }
        self.set_free(save);
        Ok(())
    }

    /// A register holding e's value: a local's own, or a new one.
    fn exp_any(&mut self, e: &Expr) -> R<u8> {
        if let Expr::Local(d) = e
            && let Var::Reg(r) = self.resolve(*d)
        {
            return Ok(r);
        }
        let r = self.alloc(1)?;
        self.exp_to(e, r)?;
        Ok(r)
    }

    /// A constant, or a register holding e's value.
    fn exp_rk(&mut self, e: &Expr) -> R<Rk> {
        let k = match e {
            Expr::Num(n) => Some(self.constant(Value::Num(*n))?),
            Expr::Str(s) => Some(self.string(s)?),
            Expr::Nil => Some(self.constant(Value::Nil)?),
            Expr::True => Some(self.constant(Value::Bool(true))?),
            Expr::False => Some(self.constant(Value::Bool(false))?),
            _ => None,
        };
        match k {
            Some(k) => Ok(K | k),
            None => Ok(u16::from(self.exp_any(e)?)),
        }
    }

    /// The values of the last expression of a list: n of them from the
    /// first free register on, or all (n = -1), setting the top.
    fn exp_multi(&mut self, e: &Expr, n: i16) -> R<()> {
        match e {
            Expr::Call(..) | Expr::Method(..) => {
                self.call(e, n)?;
            }
            Expr::Vararg => {
                let r = self.free() as u8;
                self.emit(Op::VarArg(r, (n + 1) as u8));
                if n > 0 {
                    self.alloc(n as u16)?;
                }
            }
            _ => {
                let r = self.alloc(1)?;
                self.exp_to(e, r)?;
                if n > 1 {
                    let rest = self.alloc(n as u16 - 1)?;
                    self.emit(Op::LoadNil(rest, n as u8 - 1));
                }
            }
        }
        Ok(())
    }

    /// Values in consecutive new registers: exactly n of them (n >= 0), or
    /// all of them (n = -1, the last one's all results, up to the top).
    /// The first register.
    fn exp_list(&mut self, list: &[Expr], n: i16) -> R<u8> {
        let first = self.free() as u8;
        for (i, e) in list.iter().enumerate() {
            let last = i + 1 == list.len();
            if n >= 0 && i as i16 >= n {
                // Extra values: still evaluated, then dropped.
                let save = self.free();
                let r = self.alloc(1)?;
                self.exp_to(e, r)?;
                self.set_free(save);
                continue;
            }
            if last && multi(e) {
                let rest = if n < 0 { -1 } else { n - i as i16 };
                self.exp_multi(e, rest)?;
                return Ok(first);
            }
            let r = self.alloc(1)?;
            self.exp_to(e, r)?;
        }
        if n >= 0 && (list.len() as i16) < n {
            let missing = n - list.len() as i16;
            let r = self.alloc(missing as u16)?;
            self.emit(Op::LoadNil(r, missing as u8));
        }
        Ok(first)
    }

    /// A call wanting n results (-1: all), at a new register: that
    /// register, where the results are. The free registers start after
    /// them.
    fn call(&mut self, e: &Expr, n: i16) -> R<u8> {
        let base = self.alloc(1)?;
        let args = match e {
            Expr::Call(f, args) => {
                self.exp_to(f, base)?;
                args
            }
            Expr::Method(obj, name, args) => {
                self.alloc(1)?;
                let o = self.exp_any(obj)?;
                let k = self.string(name)?;
                self.emit(Op::Method(base, o, K | k));
                self.set_free(u16::from(base) + 2);
                args
            }
            _ => unreachable!(),
        };
        let all = args.last().is_some_and(multi);
        self.exp_list(args, if all { -1 } else { args.len() as i16 })?;
        let nargs = if all { 0 } else { (self.free() - u16::from(base)) as u8 };
        // flr(x), min(a, b), sget(x, y)...: an instruction of their own.
        let intrinsic = match e {
            Expr::Call(f, _) if !all && (0..=1).contains(&n) => match &**f {
                Expr::Global(name) => crate::api::INTRINSICS
                    .iter()
                    .position(|(i, arity)| i.as_bytes() == &**name && usize::from(*arity) == args.len()),
                _ => None,
            },
            _ => None,
        };
        match intrinsic {
            Some(id) => self.emit(Op::Intrinsic(id as u8, base, args.len() as u8, (n + 1) as u8)),
            None => self.emit(Op::Call(base, nargs, (n + 1) as u8)),
        };
        self.set_free(u16::from(base) + n.max(0) as u16);
        let f = self.f();
        f.max = f.max.max(f.free);
        Ok(base)
    }

    fn table(&mut self, fields: &[Field], t: u8) -> R<()> {
        let positional = fields.iter().filter(|f| matches!(f, Field::Pos(_))).count();
        let keyed = fields.len() - positional;
        self.emit(Op::NewTable(t, positional.min(0xffff) as u16, keyed.min(0xffff) as u16));
        let mut pending = 0u16;
        let mut index = 1u32;
        let count = fields.len();
        for (i, field) in fields.iter().enumerate() {
            match field {
                Field::Pos(e) => {
                    if i + 1 == count && multi(e) {
                        self.exp_multi(e, -1)?;
                        self.emit(Op::SetList(t, 0, index));
                        self.set_free(u16::from(t) + 1);
                        return Ok(());
                    }
                    let r = self.alloc(1)?;
                    self.exp_to(e, r)?;
                    pending += 1;
                    if pending == 50 {
                        self.emit(Op::SetList(t, pending as u8, index));
                        index += u32::from(pending);
                        pending = 0;
                        self.set_free(u16::from(t) + 1);
                    }
                }
                Field::Named(name, e) => {
                    let save = self.free();
                    let k = self.string(name)?;
                    let v = self.exp_rk(e)?;
                    self.emit(Op::SetTable(t, K | k, v));
                    self.set_free(save);
                }
                Field::Keyed(key, e) => {
                    let save = self.free();
                    let k = self.exp_rk(key)?;
                    let v = self.exp_rk(e)?;
                    self.emit(Op::SetTable(t, k, v));
                    self.set_free(save);
                }
            }
        }
        if pending > 0 {
            self.emit(Op::SetList(t, pending as u8, index));
        }
        self.set_free(u16::from(t) + 1);
        Ok(())
    }

    /// Jumps taken when e's truth is `when`, to be pointed later.
    fn cond(&mut self, e: &Expr, when: bool) -> R<Vec<usize>> {
        let save = self.free();
        let jumps = match e {
            Expr::Un(UnOp::Not, x) => self.cond(x, !when)?,
            Expr::Paren(x) => self.cond(x, when)?,
            Expr::And(a, b) | Expr::Or(a, b) => {
                let is_and = matches!(e, Expr::And(..));
                if is_and != when {
                    // and/false, or/true: either side decides.
                    let mut j = self.cond(a, when)?;
                    j.extend(self.cond(b, when)?);
                    j
                } else {
                    let skip = self.cond(a, !when)?;
                    let j = self.cond(b, when)?;
                    self.patch_here(skip);
                    j
                }
            }
            Expr::Bin(op @ (BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge), a, b) => {
                let ra = self.exp_rk(a)?;
                let rb = self.exp_rk(b)?;
                let op = match op {
                    BinOp::Eq => Op::Branch(Cmp::Eq, when, ra, rb, 0),
                    BinOp::Ne => Op::Branch(Cmp::Eq, !when, ra, rb, 0),
                    BinOp::Lt => Op::Branch(Cmp::Lt, when, ra, rb, 0),
                    BinOp::Le => Op::Branch(Cmp::Le, when, ra, rb, 0),
                    BinOp::Gt => Op::Branch(Cmp::Lt, when, rb, ra, 0),
                    _ => Op::Branch(Cmp::Le, when, rb, ra, 0),
                };
                vec![self.emit(op)]
            }
            Expr::Nil | Expr::False => {
                if when {
                    vec![]
                } else {
                    vec![self.emit(Op::Jmp(0))]
                }
            }
            Expr::True | Expr::Num(_) | Expr::Str(_) => {
                if when {
                    vec![self.emit(Op::Jmp(0))]
                } else {
                    vec![]
                }
            }
            _ => {
                let r = self.exp_any(e)?;
                vec![self.emit(Op::Test(r, when, 0))]
            }
        };
        self.set_free(save);
        Ok(jumps)
    }

    // Functions.

    fn closure(&mut self, f: &Func) -> R<u16> {
        let proto = self.function(f)?;
        let fs = self.f();
        fs.protos.push(proto);
        Ok(fs.protos.len() as u16 - 1)
    }

    fn function(&mut self, f: &Func) -> R<Rc<Proto>> {
        self.fs.push(FuncState {
            code: Vec::new(),
            lines: Vec::new(),
            consts: Vec::new(),
            const_index: HashMap::default(),
            protos: Vec::new(),
            upvals: Vec::new(),
            actives: Vec::new(),
            scopes: Vec::new(),
            free: 0,
            max: 0,
        });
        let outer_line = self.line;
        self.line = f.line;
        let id = self.next_id;
        self.next_id += 1;
        if f.params.len() > 200 {
            return Err(format!("line {}: too many parameters", f.line));
        }
        let first = self.alloc(f.params.len() as u16)?;
        for (i, &p) in f.params.iter().enumerate() {
            self.activate(p, first + i as u8);
        }
        self.block(&f.body, false)?;
        self.emit(Op::Return(0, 1));
        let fs = self.fs.pop().unwrap();
        self.line = outer_line;
        let hints = (0..fs.code.len()).map(|_| std::cell::Cell::new(0)).collect();
        Ok(Rc::new(Proto {
            code: fs.code,
            lines: fs.lines,
            consts: fs.consts,
            protos: fs.protos,
            upvals: fs.upvals.into_iter().map(|(_, u)| u).collect(),
            params: f.params.len() as u8,
            vararg: f.vararg,
            regs: fs.max + 4,
            hints,
            name: f.name.clone(),
            id,
        }))
    }

    // Statements.

    fn enter(&mut self, is_loop: bool) {
        let f = self.f();
        let scope = Scope { actives: f.actives.len(), free: f.free, is_loop, breaks: Vec::new(), labels: Vec::new(), gotos: Vec::new() };
        f.scopes.push(scope);
    }

    /// Ends a scope: its locals go, its unresolved gotos go out to the one
    /// around it, and its breaks are returned.
    fn leave(&mut self) -> R<Vec<usize>> {
        let f = self.f();
        let scope = f.scopes.pop().unwrap();
        f.actives.truncate(scope.actives);
        f.free = scope.free;
        match f.scopes.last_mut() {
            Some(outer) => outer.gotos.extend(scope.gotos),
            None => {
                if let Some((name, _, line)) = scope.gotos.first() {
                    return Err(format!("line {line}: no visible label '{}' for goto", String::from_utf8_lossy(name)));
                }
            }
        }
        Ok(scope.breaks)
    }

    fn block(&mut self, body: &Block, is_loop: bool) -> R<Vec<usize>> {
        self.enter(is_loop);
        for (line, s) in body {
            self.line = *line;
            self.statement(s)?;
        }
        self.leave()
    }

    fn statement(&mut self, s: &Stmt) -> R<()> {
        let save = self.free();
        match s {
            Stmt::Local(decls, values) => {
                let first = self.exp_list(values, decls.len() as i16)?;
                self.set_free(u16::from(first) + decls.len() as u16);
                for (i, &d) in decls.iter().enumerate() {
                    self.activate(d, first + i as u8);
                }
                return Ok(());
            }
            Stmt::LocalFunction(d, f) => {
                let r = self.alloc(1)?;
                if self.decls[*d as usize].captured {
                    self.emit(Op::LoadNil(r, 1));
                    self.activate(*d, r);
                    let i = self.closure(f)?;
                    let t = self.alloc(1)?;
                    self.emit(Op::Closure(t, i));
                    self.emit(Op::SetCell(r, u16::from(t)));
                    self.set_free(u16::from(r) + 1);
                } else {
                    self.activate(*d, r);
                    let i = self.closure(f)?;
                    self.emit(Op::Closure(r, i));
                }
                return Ok(());
            }
            Stmt::Assign(targets, values) => {
                if targets.len() == 1 && values.len() == 1 {
                    self.assign_one(&targets[0], &values[0])?;
                } else {
                    self.assign_many(targets, values)?;
                }
            }
            Stmt::OpAssign(op, target, value) => self.op_assign(*op, target, value)?,
            Stmt::Call(e) => {
                self.call(e, 0)?;
            }
            Stmt::Do(b) => {
                let breaks = self.block(b, false)?;
                self.pass_breaks(breaks);
            }
            Stmt::While(c, body) => {
                let top = self.pc();
                let exits = self.cond(c, false)?;
                let breaks = self.block(body, true)?;
                self.emit(Op::Jmp(top as u32));
                self.patch_here(exits);
                self.patch_here(breaks);
            }
            Stmt::Repeat(body, c) => {
                let top = self.pc();
                self.enter(true);
                for (line, s) in body {
                    self.line = *line;
                    self.statement(s)?;
                }
                let again = self.cond(c, false)?;
                for j in again {
                    self.patch(j, top);
                }
                let breaks = self.leave()?;
                self.patch_here(breaks);
            }
            Stmt::If(branches, otherwise) => {
                let mut ends = Vec::new();
                for (i, (c, b)) in branches.iter().enumerate() {
                    let skip = self.cond(c, false)?;
                    let breaks = self.block(b, false)?;
                    self.pass_breaks(breaks);
                    if i + 1 < branches.len() || otherwise.is_some() {
                        ends.push(self.emit(Op::Jmp(0)));
                    }
                    self.patch_here(skip);
                }
                if let Some(b) = otherwise {
                    let breaks = self.block(b, false)?;
                    self.pass_breaks(breaks);
                }
                self.patch_here(ends);
            }
            Stmt::NumFor(var, start, limit, step, body) => {
                let base = self.alloc(3)?;
                self.exp_to(start, base)?;
                self.exp_to(limit, base + 1)?;
                match step {
                    Some(s) => self.exp_to(s, base + 2)?,
                    None => {
                        let k = self.constant(Value::int(1))?;
                        self.emit(Op::LoadK(base + 2, k));
                    }
                }
                let prep = self.emit(Op::ForPrep(base, 0));
                self.enter(true);
                let v = self.alloc(1)?;
                let top = self.pc();
                self.activate(*var, v);
                for (line, s) in body {
                    self.line = *line;
                    self.statement(s)?;
                }
                let breaks = self.leave()?;
                self.emit(Op::ForLoop(base, top as u32));
                self.patch_here(vec![prep]);
                self.patch_here(breaks);
            }
            Stmt::GenFor(vars, values, body) => {
                let base = self.exp_list(values, 3)?;
                let to_call = self.emit(Op::Jmp(0));
                self.enter(true);
                let first = self.alloc(vars.len().max(3) as u16)?;
                self.set_free(u16::from(first) + vars.len() as u16);
                let top = self.pc();
                for (i, &d) in vars.iter().enumerate() {
                    self.activate(d, first + i as u8);
                }
                for (line, s) in body {
                    self.line = *line;
                    self.statement(s)?;
                }
                let breaks = self.leave()?;
                self.patch_here(vec![to_call]);
                self.emit(Op::TForCall(base, vars.len() as u8));
                self.emit(Op::TForLoop(base, top as u32));
                self.patch_here(breaks);
            }
            Stmt::Return(values) => {
                if values.len() == 1 && !multi(&values[0]) {
                    let r = self.exp_any(&values[0])?;
                    self.emit(Op::Return(r, 2));
                } else {
                    let all = values.last().is_some_and(multi);
                    let first = self.exp_list(values, if all { -1 } else { values.len() as i16 })?;
                    let n = if all { 0 } else { values.len() as u8 + 1 };
                    self.emit(Op::Return(first, n));
                }
            }
            Stmt::Break => {
                let j = self.emit(Op::Jmp(0));
                let line = self.line;
                match self.f().scopes.iter_mut().rev().find(|s| s.is_loop) {
                    Some(s) => s.breaks.push(j),
                    None => return Err(format!("line {line}: break outside a loop")),
                }
            }
            Stmt::Goto(name) => {
                let found = self.f().scopes.iter().rev().find_map(|s| s.labels.iter().find(|(n, _)| n == name).map(|&(_, pc)| pc));
                match found {
                    Some(pc) => {
                        self.emit(Op::Jmp(pc as u32));
                    }
                    None => {
                        let j = self.emit(Op::Jmp(0));
                        let line = self.line;
                        self.f().scopes.last_mut().unwrap().gotos.push((name.clone(), j, line));
                    }
                }
            }
            Stmt::Label(name) => {
                let here = self.pc();
                let scope = self.f().scopes.last_mut().unwrap();
                scope.labels.push((name.clone(), here));
                let (matched, rest): (Vec<_>, Vec<_>) = scope.gotos.drain(..).partition(|(n, _, _)| n == name);
                scope.gotos = rest;
                for (_, j, _) in matched {
                    self.patch(j, here);
                }
            }
        }
        self.set_free(save);
        Ok(())
    }

    /// A block that is no loop hands its breaks to the loop around it.
    fn pass_breaks(&mut self, breaks: Vec<usize>) {
        if let Some(s) = self.f().scopes.iter_mut().rev().find(|s| s.is_loop) {
            s.breaks.extend(breaks);
        }
    }

    fn assign_one(&mut self, target: &Expr, value: &Expr) -> R<()> {
        match target {
            Expr::Local(d) => match self.resolve(*d) {
                Var::Reg(r) => {
                    if single_write(value) {
                        self.exp_to(value, r)?;
                    } else {
                        let t = self.alloc(1)?;
                        self.exp_to(value, t)?;
                        self.emit(Op::Move(r, t));
                    }
                }
                Var::Cell(r) => {
                    let v = self.exp_rk(value)?;
                    self.emit(Op::SetCell(r, v));
                }
                Var::Up(i) => {
                    let v = self.exp_rk(value)?;
                    self.emit(Op::SetUp(i, v));
                }
            },
            Expr::Global(n) => {
                let k = self.string(n)?;
                let v = self.exp_rk(value)?;
                self.emit(Op::SetGlobal(k, v));
            }
            Expr::Index(t, key) => {
                let tr = self.exp_any(t)?;
                let kr = self.exp_rk(key)?;
                let v = self.exp_rk(value)?;
                self.emit(Op::SetTable(tr, kr, v));
            }
            _ => return Err(format!("line {}: cannot assign to this", self.line)),
        }
        Ok(())
    }

    fn assign_many(&mut self, targets: &[Expr], values: &[Expr]) -> R<()> {
        // The tables and keys first, then the values, then the stores.
        let mut places = Vec::new();
        for t in targets {
            places.push(match t {
                Expr::Index(table, key) => {
                    let tr = self.alloc(1)?;
                    self.exp_to(table, tr)?;
                    let kr = match **key {
                        Expr::Str(_) | Expr::Num(_) => self.exp_rk(key)?,
                        _ => {
                            let r = self.alloc(1)?;
                            self.exp_to(key, r)?;
                            u16::from(r)
                        }
                    };
                    Some((tr, kr))
                }
                _ => None,
            });
        }
        let first = self.exp_list(values, targets.len() as i16)?;
        // Stored from the last to the first, as Lua does: `a,a=1,2` leaves 1.
        for (i, t) in targets.iter().enumerate().rev() {
            let v = first + i as u8;
            match (t, places[i]) {
                (_, Some((tr, kr))) => {
                    self.emit(Op::SetTable(tr, kr, u16::from(v)));
                }
                (Expr::Local(d), _) => match self.resolve(*d) {
                    Var::Reg(r) => {
                        self.emit(Op::Move(r, v));
                    }
                    Var::Cell(r) => {
                        self.emit(Op::SetCell(r, u16::from(v)));
                    }
                    Var::Up(i) => {
                        self.emit(Op::SetUp(i, u16::from(v)));
                    }
                },
                (Expr::Global(n), _) => {
                    let k = self.string(n)?;
                    self.emit(Op::SetGlobal(k, u16::from(v)));
                }
                _ => return Err(format!("line {}: cannot assign to this", self.line)),
            }
        }
        Ok(())
    }

    fn op_assign(&mut self, op: BinOp, target: &Expr, value: &Expr) -> R<()> {
        let apply = |c: &mut Self, dst: u8, a: Rk, b: Rk| {
            match op {
                BinOp::Concat => c.emit(Op::Concat(dst, a, b)),
                op => c.emit(arith_op(op, dst, a, b)),
            };
        };
        match target {
            Expr::Local(d) => match self.resolve(*d) {
                Var::Reg(r) => {
                    let v = self.exp_rk(value)?;
                    apply(self, r, u16::from(r), v);
                }
                Var::Cell(r) => {
                    let t = self.alloc(1)?;
                    self.emit(Op::GetCell(t, r));
                    let v = self.exp_rk(value)?;
                    apply(self, t, u16::from(t), v);
                    self.emit(Op::SetCell(r, u16::from(t)));
                }
                Var::Up(i) => {
                    let t = self.alloc(1)?;
                    self.emit(Op::GetUp(t, i));
                    let v = self.exp_rk(value)?;
                    apply(self, t, u16::from(t), v);
                    self.emit(Op::SetUp(i, u16::from(t)));
                }
            },
            Expr::Global(n) => {
                let k = self.string(n)?;
                let t = self.alloc(1)?;
                self.emit(Op::GetGlobal(t, k));
                let v = self.exp_rk(value)?;
                apply(self, t, u16::from(t), v);
                self.emit(Op::SetGlobal(k, u16::from(t)));
            }
            Expr::Index(table, key) => {
                let tr = self.exp_any(table)?;
                let kr = self.exp_rk(key)?;
                let t = self.alloc(1)?;
                self.emit(Op::GetTable(t, tr, kr));
                let v = self.exp_rk(value)?;
                apply(self, t, u16::from(t), v);
                self.emit(Op::SetTable(tr, kr, u16::from(t)));
            }
            _ => return Err(format!("line {}: cannot assign to this", self.line)),
        }
        Ok(())
    }
}
