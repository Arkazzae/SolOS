//! P8 Lua source to a syntax tree: Lua 5.2 with PICO-8's additions (the
//! one-line `if (c) …` and `while (c) …`, `?` for print, compound
//! assignment, `!=`, `\` for whole division, `^^` and the shifts, the peek
//! operators `@ % $`, `//` comments and glyphs in names). The source is
//! P8SCII bytes, not UTF-8.
//!
//! Names are resolved here: each local gets a number, and a local that a
//! nested function uses is marked captured, so the compiler can keep it in
//! a cell from the start.

use crate::fix;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Name(Box<[u8]>),
    Num(i32),
    Str(Box<[u8]>),
    /// A keyword or a symbol.
    Op(&'static str),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub line: u32,
    /// The byte in the line it starts at, for minified code's long lines.
    pub col: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError(pub String);

const KEYWORDS: [&str; 22] = [
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local", "nil", "not",
    "or", "repeat", "return", "then", "true", "until", "while",
];

const SYMBOLS: [&str; 51] = [
    ">>>=", "<<>=", ">><=", "...", "..=", "^^=", "<<=", ">>=", ">>>", "<<>", ">><", "+=", "-=", "*=", "/=", "%=", "^=",
    "|=", "&=", "\\=", "!=", "~=", "==", "<=", ">=", "..", "<<", ">>", "^^", "::", "+", "-", "*", "/", "%", "^", "#",
    "&", "|", "~", "\\", "<", ">", "=", "(", ")", "{", "}", "[", "]", ";",
];
const MORE_SYMBOLS: [&str; 6] = [":", ",", ".", "?", "@", "$"];

fn keyword(name: &[u8]) -> Option<&'static str> {
    KEYWORDS.iter().copied().find(|k| k.as_bytes() == name)
}

/// The level of a long bracket `[==[` at s (the number of `=`), if one.
fn long_bracket(s: &[u8]) -> Option<usize> {
    if s.first() != Some(&b'[') {
        return None;
    }
    let level = s[1..].iter().take_while(|&&c| c == b'=').count();
    (s.get(1 + level) == Some(&b'[')).then_some(level)
}

pub fn lex(src: &[u8]) -> Result<Vec<Token>, SyntaxError> {
    let mut out = Vec::new();
    let (mut p, mut line) = (0usize, 1u32);
    let mut line_start = 0usize;
    let err = |line: u32, m: &str| SyntaxError(format!("line {line}: {m}"));
    // The end of a long bracket's contents, from just after its opening.
    let long_end = |p: usize, level: usize, line: &mut u32| -> Option<(usize, usize)> {
        let mut q = p;
        while q < src.len() {
            if src[q] == b']'
                && src[q + 1..].iter().take(level).all(|&c| c == b'=')
                && src.get(q + 1 + level) == Some(&b']')
                && src.len() > q + level
            {
                return Some((q, q + level + 2));
            }
            if src[q] == b'\n' {
                *line += 1;
            }
            q += 1;
        }
        None
    };
    while p < src.len() {
        let c = src[p];
        if c == b'\n' {
            line += 1;
            p += 1;
            line_start = p;
            continue;
        }
        if c <= b' ' {
            p += 1;
            continue;
        }
        if src[p..].starts_with(b"--") || src[p..].starts_with(b"//") {
            p += 2;
            if let Some(level) = long_bracket(&src[p..]) {
                let start_line = line;
                let (_, end) = long_end(p + level + 2, level, &mut line)
                    .ok_or_else(|| err(start_line, "unfinished long comment"))?;
                p = end;
            } else {
                while p < src.len() && src[p] != b'\n' {
                    p += 1;
                }
            }
            continue;
        }
        let at = line;
        let col = (p - line_start) as u32;
        let tok = if c == b'"' || c == b'\'' {
            let (s, end) = string(src, p, &mut line).map_err(|m| err(at, m))?;
            p = end;
            Tok::Str(s.into())
        } else if let Some(level) = long_bracket(&src[p..]) {
            let mut start = p + level + 2;
            if src.get(start) == Some(&b'\r') {
                start += 1;
            }
            if src.get(start) == Some(&b'\n') {
                start += 1;
                line += 1;
            }
            let (end, next) = long_end(start, level, &mut line).ok_or_else(|| err(at, "unfinished long string"))?;
            p = next;
            Tok::Str(src[start..end].into())
        } else if c.is_ascii_digit() || (c == b'.' && src.get(p + 1).is_some_and(u8::is_ascii_digit)) {
            let start = p;
            let radix = if src[p..].len() > 1 && c == b'0' && matches!(src[p + 1], b'x' | b'X') {
                p += 2;
                16
            } else if src[p..].len() > 1 && c == b'0' && matches!(src[p + 1], b'b' | b'B') {
                p += 2;
                2
            } else {
                10
            };
            let mut seen_point = false;
            while p < src.len() {
                let d = src[p];
                let ok = match d {
                    b'.' if !seen_point && src.get(p + 1) != Some(&b'.') => {
                        seen_point = true;
                        true
                    }
                    _ => (d as char).is_digit(radix),
                };
                if !ok {
                    break;
                }
                p += 1;
            }
            Tok::Num(fix::parse(&src[start..p]).ok_or_else(|| err(at, "malformed number"))?)
        } else if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            let start = p;
            while p < src.len() && (src[p].is_ascii_alphanumeric() || src[p] == b'_' || src[p] >= 0x80) {
                p += 1;
            }
            let name = &src[start..p];
            match keyword(name) {
                Some(k) => Tok::Op(k),
                None => Tok::Name(name.into()),
            }
        } else if let Some(&s) = SYMBOLS.iter().chain(&MORE_SYMBOLS).find(|s| src[p..].starts_with(s.as_bytes())) {
            p += s.len();
            Tok::Op(s)
        } else if c == b'!' {
            // A lone `!`: PICO-8 reads it as `not`... it does not; skip it.
            return Err(err(at, "unexpected symbol '!'"));
        } else {
            return Err(err(at, &format!("unexpected character 0x{c:02x}")));
        };
        out.push(Token { tok, line: at, col });
    }
    out.push(Token { tok: Tok::Eof, line, col: 0 });
    Ok(out)
}

/// A quoted string from p (at its quote): its bytes and where it ends.
fn string(src: &[u8], mut p: usize, line: &mut u32) -> Result<(Vec<u8>, usize), &'static str> {
    let quote = src[p];
    p += 1;
    let mut s = Vec::new();
    loop {
        let Some(&c) = src.get(p) else {
            return Err("unfinished string");
        };
        p += 1;
        if c == quote {
            return Ok((s, p));
        }
        if c == b'\n' {
            return Err("unfinished string");
        }
        if c != b'\\' {
            s.push(c);
            continue;
        }
        let Some(&e) = src.get(p) else {
            return Err("unfinished string");
        };
        p += 1;
        match e {
            b'n' => s.push(b'\n'),
            b't' => s.push(b'\t'),
            b'r' => s.push(b'\r'),
            b'a' => s.push(7),
            b'b' => s.push(8),
            b'v' => s.push(11),
            b'f' => s.push(12),
            // P8SCII's control codes for print.
            b'*' => s.push(1),
            b'#' => s.push(2),
            b'-' => s.push(3),
            b'|' => s.push(4),
            b'+' => s.push(5),
            b'^' => s.push(6),
            b'\n' => {
                *line += 1;
                s.push(b'\n');
            }
            b'z' => {
                while p < src.len() && src[p] <= b' ' {
                    if src[p] == b'\n' {
                        *line += 1;
                    }
                    p += 1;
                }
            }
            b'x' => {
                let hex = src.get(p..p + 2).ok_or("bad \\x escape")?;
                let v = std::str::from_utf8(hex).ok().and_then(|h| u8::from_str_radix(h, 16).ok()).ok_or("bad \\x escape")?;
                s.push(v);
                p += 2;
            }
            b'0'..=b'9' => {
                let mut v = u32::from(e - b'0');
                for _ in 0..2 {
                    match src.get(p) {
                        Some(&d) if d.is_ascii_digit() => {
                            v = v * 10 + u32::from(d - b'0');
                            p += 1;
                        }
                        _ => break,
                    }
                }
                s.push(v as u8);
            }
            other => s.push(other),
        }
    }
}

// The syntax tree.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
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
pub enum UnOp {
    Neg,
    Not,
    Len,
    BNot,
    Peek,
    Peek2,
    Peek4,
}

pub enum Expr {
    Nil,
    True,
    False,
    Num(i32),
    Str(Box<[u8]>),
    Vararg,
    Local(u32),
    Global(Box<[u8]>),
    Index(Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    Method(Box<Expr>, Box<[u8]>, Vec<Expr>),
    Function(Box<Func>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Un(UnOp, Box<Expr>),
    Table(Vec<Field>),
    /// Parentheses: one value of a call or `...`.
    Paren(Box<Expr>),
}

pub enum Field {
    Pos(Expr),
    Named(Box<[u8]>, Expr),
    Keyed(Expr, Expr),
}

pub struct Func {
    pub params: Vec<u32>,
    pub vararg: bool,
    pub body: Block,
    pub line: u32,
    pub name: String,
}

pub type Block = Vec<(u32, Stmt)>;

pub enum Stmt {
    Local(Vec<u32>, Vec<Expr>),
    LocalFunction(u32, Box<Func>),
    Assign(Vec<Expr>, Vec<Expr>),
    OpAssign(BinOp, Expr, Expr),
    Call(Expr),
    Do(Block),
    While(Expr, Block),
    /// The condition sees the body's locals.
    Repeat(Block, Expr),
    If(Vec<(Expr, Block)>, Option<Block>),
    NumFor(u32, Expr, Expr, Option<Expr>, Block),
    GenFor(Vec<u32>, Vec<Expr>, Block),
    Return(Vec<Expr>),
    Break,
    Goto(Box<[u8]>),
    Label(Box<[u8]>),
}

pub struct Decl {
    pub name: Box<[u8]>,
    /// The depth of the function it belongs to.
    pub function: u32,
    pub captured: bool,
}

pub struct Chunk {
    pub main: Func,
    pub decls: Vec<Decl>,
}

struct Parser<'a> {
    toks: &'a [Token],
    p: usize,
    decls: Vec<Decl>,
    /// The locals in scope, innermost last.
    scope: Vec<u32>,
    function: u32,
    depth: u32,
    /// The line of a one-line if or while being read: its body ends there.
    short_line: Option<u32>,
}

pub fn parse(src: &[u8]) -> Result<Chunk, SyntaxError> {
    let toks = lex(src)?;
    let mut parser = Parser { toks: &toks, p: 0, decls: Vec::new(), scope: Vec::new(), function: 0, depth: 0, short_line: None };
    let body = parser.block()?;
    if parser.tok() != &Tok::Eof {
        return Err(parser.err("unexpected symbol"));
    }
    Ok(Chunk { main: Func { params: Vec::new(), vararg: true, body, line: 0, name: "main".into() }, decls: parser.decls })
}

impl Parser<'_> {
    fn tok(&self) -> &Tok {
        &self.toks[self.p].tok
    }
    fn peek(&self, n: usize) -> &Tok {
        &self.toks[(self.p + n).min(self.toks.len() - 1)].tok
    }
    fn line(&self) -> u32 {
        self.toks[self.p].line
    }
    fn prev_line(&self) -> u32 {
        self.toks[self.p.saturating_sub(1)].line
    }
    fn err(&self, m: &str) -> SyntaxError {
        let near = match self.tok() {
            Tok::Name(n) => format!(" near '{}'", String::from_utf8_lossy(n)),
            Tok::Op(o) => format!(" near '{o}'"),
            Tok::Num(_) => " near a number".into(),
            Tok::Str(_) => " near a string".into(),
            Tok::Eof => " at the end".into(),
        };
        let t = &self.toks[self.p];
        let col = if t.col > 60 { format!(" (column {})", t.col + 1) } else { String::new() };
        SyntaxError(format!("line {}{col}: {m}{near}", self.line()))
    }
    fn at(&self, op: &str) -> bool {
        matches!(self.tok(), Tok::Op(o) if *o == op)
    }
    fn eat(&mut self, op: &str) -> bool {
        if self.at(op) {
            self.p += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, op: &str) -> Result<(), SyntaxError> {
        if self.eat(op) { Ok(()) } else { Err(self.err(&format!("'{op}' expected"))) }
    }
    fn name(&mut self) -> Result<Box<[u8]>, SyntaxError> {
        if let Tok::Name(n) = self.tok() {
            let n = n.clone();
            self.p += 1;
            Ok(n)
        } else {
            Err(self.err("name expected"))
        }
    }
    fn nest(&mut self) -> Result<(), SyntaxError> {
        self.depth += 1;
        if self.depth > 200 { Err(self.err("too deeply nested")) } else { Ok(()) }
    }

    fn declare(&mut self, name: Box<[u8]>) -> u32 {
        let id = self.decls.len() as u32;
        self.decls.push(Decl { name, function: self.function, captured: false });
        id
    }
    fn activate(&mut self, id: u32) {
        self.scope.push(id);
    }
    fn resolve(&mut self, name: &[u8]) -> Option<u32> {
        let id = *self.scope.iter().rev().find(|&&id| &*self.decls[id as usize].name == name)?;
        let decl = &mut self.decls[id as usize];
        if decl.function != self.function {
            decl.captured = true;
        }
        Some(id)
    }
    fn variable(&mut self, name: Box<[u8]>) -> Expr {
        if let Some(id) = self.resolve(&name) {
            return Expr::Local(id);
        }
        // A free name with a local _ENV in scope is a field of it.
        if let Some(env) = self.resolve(b"_ENV") {
            return Expr::Index(Box::new(Expr::Local(env)), Box::new(Expr::Str(name)));
        }
        Expr::Global(name)
    }

    /// Whether the block ends here.
    fn block_end(&self) -> bool {
        match self.tok() {
            Tok::Eof => true,
            Tok::Op(o) => matches!(*o, "end" | "else" | "elseif" | "until"),
            _ => false,
        }
    }

    fn block(&mut self) -> Result<Block, SyntaxError> {
        self.nest()?;
        // A block inside a one-line if (`if (c) do … end`) has lines of its own.
        let outer_short = self.short_line.take();
        let mark = self.scope.len();
        let mut out = Vec::new();
        while !self.block_end() {
            if self.eat(";") {
                continue;
            }
            let line = self.line();
            let s = self.statement()?;
            let last = matches!(s, Stmt::Return(_));
            out.push((line, s));
            if last {
                self.eat(";");
                break;
            }
        }
        self.scope.truncate(mark);
        self.short_line = outer_short;
        self.depth -= 1;
        Ok(out)
    }

    /// The statements of a one-line if or while: up to the end of the line.
    fn short_block(&mut self, line: u32, stop_at_else: bool) -> Result<Block, SyntaxError> {
        let outer = self.short_line.replace(line);
        let mark = self.scope.len();
        let mut out = Vec::new();
        while self.line() == line && !self.block_end() && !(stop_at_else && self.at("else")) {
            if self.eat(";") {
                continue;
            }
            let at = self.line();
            let s = self.statement()?;
            let last = matches!(s, Stmt::Return(_));
            out.push((at, s));
            if last {
                break;
            }
        }
        self.scope.truncate(mark);
        self.short_line = outer;
        Ok(out)
    }

    fn function_body(&mut self, method: bool, name: String) -> Result<Func, SyntaxError> {
        let line = self.line();
        self.function += 1;
        let mark = self.scope.len();
        let outer_short = self.short_line.take();
        let mut params = Vec::new();
        if method {
            let id = self.declare(b"self".as_slice().into());
            params.push(id);
        }
        self.expect("(")?;
        let mut vararg = false;
        if !self.at(")") {
            loop {
                if self.eat("...") {
                    vararg = true;
                    break;
                }
                let n = self.name()?;
                params.push(self.declare(n));
                if !self.eat(",") {
                    break;
                }
            }
        }
        self.expect(")")?;
        for &id in &params {
            self.activate(id);
        }
        let body = self.block()?;
        self.expect("end")?;
        self.scope.truncate(mark);
        self.function -= 1;
        self.short_line = outer_short;
        Ok(Func { params, vararg, body, line, name })
    }

    fn statement(&mut self) -> Result<Stmt, SyntaxError> {
        self.nest()?;
        let s = self.statement_inner();
        self.depth -= 1;
        s
    }

    fn statement_inner(&mut self) -> Result<Stmt, SyntaxError> {
        let line = self.line();
        let Tok::Op(op) = *self.tok() else {
            return self.expression_statement();
        };
        match op {
            "local" => {
                self.p += 1;
                if self.eat("function") {
                    let name = self.name()?;
                    let label = String::from_utf8_lossy(&name).into_owned();
                    let id = self.declare(name);
                    self.activate(id);
                    let f = self.function_body(false, label)?;
                    return Ok(Stmt::LocalFunction(id, Box::new(f)));
                }
                let mut names = vec![self.name()?];
                while self.eat(",") {
                    names.push(self.name()?);
                }
                let values = if self.eat("=") { self.expressions()? } else { Vec::new() };
                let ids = names.into_iter().map(|n| self.declare(n)).collect::<Vec<_>>();
                for &id in &ids {
                    self.activate(id);
                }
                Ok(Stmt::Local(ids, values))
            }
            "function" => {
                self.p += 1;
                let first = self.name()?;
                let mut label = String::from_utf8_lossy(&first).into_owned();
                let mut target = self.variable(first);
                let mut method = false;
                loop {
                    if self.eat(".") {
                        let n = self.name()?;
                        label = format!("{label}.{}", String::from_utf8_lossy(&n));
                        target = Expr::Index(Box::new(target), Box::new(Expr::Str(n)));
                    } else if self.eat(":") {
                        let n = self.name()?;
                        label = format!("{label}:{}", String::from_utf8_lossy(&n));
                        target = Expr::Index(Box::new(target), Box::new(Expr::Str(n)));
                        method = true;
                        break;
                    } else {
                        break;
                    }
                }
                let f = self.function_body(method, label)?;
                Ok(Stmt::Assign(vec![target], vec![Expr::Function(Box::new(f))]))
            }
            "if" => {
                self.p += 1;
                let cond = self.expr()?;
                if self.eat("then") {
                    let mut branches = vec![(cond, self.block()?)];
                    let mut otherwise = None;
                    loop {
                        if self.eat("elseif") {
                            let c = self.expr()?;
                            self.expect("then")?;
                            branches.push((c, self.block()?));
                        } else if self.eat("else") {
                            otherwise = Some(self.block()?);
                            self.expect("end")?;
                            break;
                        } else {
                            self.expect("end")?;
                            break;
                        }
                    }
                    return Ok(Stmt::If(branches, otherwise));
                }
                // PICO-8's one-line if: `if (c) statements [else statements]`.
                let at = self.prev_line();
                if self.line() != at {
                    return Err(self.err("'then' expected"));
                }
                let body = self.short_block(at, true)?;
                let otherwise = if self.line() == at && self.eat("else") { Some(self.short_block(at, false)?) } else { None };
                Ok(Stmt::If(vec![(cond, body)], otherwise))
            }
            "while" => {
                self.p += 1;
                let cond = self.expr()?;
                if self.eat("do") {
                    let body = self.block()?;
                    self.expect("end")?;
                    return Ok(Stmt::While(cond, body));
                }
                let at = self.prev_line();
                if self.line() != at {
                    return Err(self.err("'do' expected"));
                }
                Ok(Stmt::While(cond, self.short_block(at, false)?))
            }
            "do" => {
                self.p += 1;
                let b = self.block()?;
                self.expect("end")?;
                Ok(Stmt::Do(b))
            }
            "for" => {
                self.p += 1;
                let first = self.name()?;
                if self.eat("=") {
                    let start = self.expr()?;
                    self.expect(",")?;
                    let limit = self.expr()?;
                    let step = if self.eat(",") { Some(self.expr()?) } else { None };
                    self.expect("do")?;
                    let mark = self.scope.len();
                    let id = self.declare(first);
                    self.activate(id);
                    let body = self.block()?;
                    self.scope.truncate(mark);
                    self.expect("end")?;
                    return Ok(Stmt::NumFor(id, start, limit, step, body));
                }
                let mut names = vec![first];
                while self.eat(",") {
                    names.push(self.name()?);
                }
                self.expect("in")?;
                let values = self.expressions()?;
                self.expect("do")?;
                let mark = self.scope.len();
                let ids = names.into_iter().map(|n| self.declare(n)).collect::<Vec<_>>();
                for &id in &ids {
                    self.activate(id);
                }
                let body = self.block()?;
                self.scope.truncate(mark);
                self.expect("end")?;
                Ok(Stmt::GenFor(ids, values, body))
            }
            "repeat" => {
                self.p += 1;
                let mark = self.scope.len();
                // The body's scope stays open for the condition.
                let mut body = Vec::new();
                while !self.block_end() {
                    if self.eat(";") {
                        continue;
                    }
                    let at = self.line();
                    let s = self.statement()?;
                    let last = matches!(s, Stmt::Return(_));
                    body.push((at, s));
                    if last {
                        break;
                    }
                }
                self.expect("until")?;
                let cond = self.expr()?;
                self.scope.truncate(mark);
                Ok(Stmt::Repeat(body, cond))
            }
            "return" => {
                self.p += 1;
                let ends = self.block_end()
                    || self.at(";")
                    || (self.short_line.is_some() && self.line() != line)
                    || (self.short_line.is_some() && self.at("else"));
                let values = if ends { Vec::new() } else { self.expressions()? };
                Ok(Stmt::Return(values))
            }
            "break" => {
                self.p += 1;
                Ok(Stmt::Break)
            }
            "goto" => {
                self.p += 1;
                Ok(Stmt::Goto(self.name()?))
            }
            "::" => {
                self.p += 1;
                let n = self.name()?;
                self.expect("::")?;
                Ok(Stmt::Label(n))
            }
            "?" => {
                // `?a,b,c`: print(a,b,c), to the end of the line.
                self.p += 1;
                let mut args = Vec::new();
                if self.line() == line && !self.block_end() {
                    args = self.expressions()?;
                }
                Ok(Stmt::Call(Expr::Call(Box::new(Expr::Global(b"print".as_slice().into())), args)))
            }
            _ => self.expression_statement(),
        }
    }

    fn expression_statement(&mut self) -> Result<Stmt, SyntaxError> {
        let first = self.suffixed()?;
        if self.at("=") || self.at(",") {
            let mut targets = vec![first];
            while self.eat(",") {
                targets.push(self.suffixed()?);
            }
            self.expect("=")?;
            for t in &targets {
                if !matches!(t, Expr::Local(_) | Expr::Global(_) | Expr::Index(..)) {
                    return Err(self.err("cannot assign to this"));
                }
            }
            let values = self.expressions()?;
            return Ok(Stmt::Assign(targets, values));
        }
        if let Tok::Op(o) = *self.tok() {
            let op = match o {
                "+=" => Some(BinOp::Add),
                "-=" => Some(BinOp::Sub),
                "*=" => Some(BinOp::Mul),
                "/=" => Some(BinOp::Div),
                "\\=" => Some(BinOp::IDiv),
                "%=" => Some(BinOp::Mod),
                "^=" => Some(BinOp::Pow),
                "..=" => Some(BinOp::Concat),
                "|=" => Some(BinOp::BOr),
                "&=" => Some(BinOp::BAnd),
                "^^=" => Some(BinOp::BXor),
                "<<=" => Some(BinOp::Shl),
                ">>=" => Some(BinOp::Shr),
                ">>>=" => Some(BinOp::LShr),
                "<<>=" => Some(BinOp::RotL),
                ">><=" => Some(BinOp::RotR),
                _ => None,
            };
            if let Some(op) = op {
                self.p += 1;
                if !matches!(first, Expr::Local(_) | Expr::Global(_) | Expr::Index(..)) {
                    return Err(self.err("cannot assign to this"));
                }
                let value = self.expr()?;
                return Ok(Stmt::OpAssign(op, first, value));
            }
        }
        if !matches!(first, Expr::Call(..) | Expr::Method(..)) {
            return Err(self.err("syntax error"));
        }
        Ok(Stmt::Call(first))
    }

    fn expressions(&mut self) -> Result<Vec<Expr>, SyntaxError> {
        let mut v = vec![self.expr()?];
        while self.eat(",") {
            v.push(self.expr()?);
        }
        Ok(v)
    }

    fn expr(&mut self) -> Result<Expr, SyntaxError> {
        self.sub_expr(0)
    }

    fn sub_expr(&mut self, limit: u8) -> Result<Expr, SyntaxError> {
        self.nest()?;
        let unary = match self.tok() {
            Tok::Op("not") => Some(UnOp::Not),
            Tok::Op("-") => Some(UnOp::Neg),
            Tok::Op("#") => Some(UnOp::Len),
            Tok::Op("~") => Some(UnOp::BNot),
            Tok::Op("@") => Some(UnOp::Peek),
            Tok::Op("%") => Some(UnOp::Peek2),
            Tok::Op("$") => Some(UnOp::Peek4),
            _ => None,
        };
        let mut e = if let Some(op) = unary {
            self.p += 1;
            let operand = self.sub_expr(12)?;
            match (op, operand) {
                (UnOp::Neg, Expr::Num(n)) => Expr::Num(n.wrapping_neg()),
                (op, operand) => Expr::Un(op, Box::new(operand)),
            }
        } else {
            self.simple()?
        };
        while let Tok::Op(o) = *self.tok() {
            let (op, left, right) = match o {
                "or" => (None, 1, 1),
                "and" => (None, 2, 2),
                "<" => (Some(BinOp::Lt), 3, 3),
                ">" => (Some(BinOp::Gt), 3, 3),
                "<=" => (Some(BinOp::Le), 3, 3),
                ">=" => (Some(BinOp::Ge), 3, 3),
                "~=" | "!=" => (Some(BinOp::Ne), 3, 3),
                "==" => (Some(BinOp::Eq), 3, 3),
                "|" => (Some(BinOp::BOr), 4, 4),
                "^^" | "~" => (Some(BinOp::BXor), 5, 5),
                "&" => (Some(BinOp::BAnd), 6, 6),
                "<<" => (Some(BinOp::Shl), 7, 7),
                ">>" => (Some(BinOp::Shr), 7, 7),
                ">>>" => (Some(BinOp::LShr), 7, 7),
                "<<>" => (Some(BinOp::RotL), 7, 7),
                ">><" => (Some(BinOp::RotR), 7, 7),
                ".." => (Some(BinOp::Concat), 9, 8),
                "+" => (Some(BinOp::Add), 10, 10),
                "-" => (Some(BinOp::Sub), 10, 10),
                "*" => (Some(BinOp::Mul), 11, 11),
                "/" => (Some(BinOp::Div), 11, 11),
                "\\" => (Some(BinOp::IDiv), 11, 11),
                "%" => (Some(BinOp::Mod), 11, 11),
                "^" => (Some(BinOp::Pow), 14, 13),
                _ => break,
            };
            if left <= limit {
                break;
            }
            self.p += 1;
            let rhs = self.sub_expr(right)?;
            e = match (op, o) {
                (Some(op), _) => Expr::Bin(op, Box::new(e), Box::new(rhs)),
                (None, "and") => Expr::And(Box::new(e), Box::new(rhs)),
                _ => Expr::Or(Box::new(e), Box::new(rhs)),
            };
        }
        self.depth -= 1;
        Ok(e)
    }

    fn simple(&mut self) -> Result<Expr, SyntaxError> {
        let e = match self.tok().clone() {
            Tok::Num(n) => Expr::Num(n),
            Tok::Str(s) => Expr::Str(s),
            Tok::Op("nil") => Expr::Nil,
            Tok::Op("true") => Expr::True,
            Tok::Op("false") => Expr::False,
            Tok::Op("...") => Expr::Vararg,
            Tok::Op("{") => return self.table(),
            Tok::Op("function") => {
                self.p += 1;
                let line = self.prev_line();
                return Ok(Expr::Function(Box::new(self.function_body(false, format!("function at line {line}"))?)));
            }
            _ => return self.suffixed(),
        };
        self.p += 1;
        Ok(e)
    }

    fn primary(&mut self) -> Result<Expr, SyntaxError> {
        match self.tok().clone() {
            Tok::Name(n) => {
                self.p += 1;
                Ok(self.variable(n))
            }
            Tok::Op("(") => {
                self.p += 1;
                let e = self.expr()?;
                self.expect(")")?;
                Ok(match e {
                    e @ (Expr::Call(..) | Expr::Method(..) | Expr::Vararg) => Expr::Paren(Box::new(e)),
                    e => e,
                })
            }
            _ => Err(self.err("unexpected symbol")),
        }
    }

    fn suffixed(&mut self) -> Result<Expr, SyntaxError> {
        let mut e = self.primary()?;
        loop {
            match self.tok() {
                Tok::Op(".") => {
                    self.p += 1;
                    let n = self.name()?;
                    e = Expr::Index(Box::new(e), Box::new(Expr::Str(n)));
                }
                Tok::Op("[") => {
                    self.p += 1;
                    let k = self.expr()?;
                    self.expect("]")?;
                    e = Expr::Index(Box::new(e), Box::new(k));
                }
                Tok::Op(":") => {
                    self.p += 1;
                    let n = self.name()?;
                    let args = self.args()?;
                    e = Expr::Method(Box::new(e), n, args);
                }
                Tok::Op("(") | Tok::Op("{") | Tok::Str(_) => {
                    // In a one-line if, a `(` on the next line starts a new
                    // statement, not a call.
                    if self.short_line.is_some_and(|l| l != self.line()) {
                        break;
                    }
                    let args = self.args()?;
                    e = Expr::Call(Box::new(e), args);
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn args(&mut self) -> Result<Vec<Expr>, SyntaxError> {
        match self.tok().clone() {
            Tok::Str(s) => {
                self.p += 1;
                Ok(vec![Expr::Str(s)])
            }
            Tok::Op("{") => Ok(vec![self.table()?]),
            Tok::Op("(") => {
                self.p += 1;
                if self.eat(")") {
                    return Ok(Vec::new());
                }
                let v = self.expressions()?;
                self.expect(")")?;
                Ok(v)
            }
            _ => Err(self.err("function arguments expected")),
        }
    }

    fn table(&mut self) -> Result<Expr, SyntaxError> {
        self.expect("{")?;
        let mut fields = Vec::new();
        while !self.at("}") {
            if self.eat("[") {
                let k = self.expr()?;
                self.expect("]")?;
                self.expect("=")?;
                fields.push(Field::Keyed(k, self.expr()?));
            } else if matches!(self.tok(), Tok::Name(_)) && self.peek(1) == &Tok::Op("=") {
                let n = self.name()?;
                self.p += 1;
                fields.push(Field::Named(n, self.expr()?));
            } else {
                fields.push(Field::Pos(self.expr()?));
            }
            if !self.eat(",") && !self.eat(";") {
                break;
            }
        }
        self.expect("}")?;
        Ok(Expr::Table(fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(src: &str) -> usize {
        parse(src.as_bytes()).unwrap().main.body.len()
    }

    #[test]
    fn pico8_syntax() {
        assert_eq!(count("if (a) b=1 c=2\nd=3"), 2);
        assert_eq!(count("if (a) b=1 else c=2\nd=3"), 2);
        assert_eq!(count("while (a<3) a+=1\nprint(a)"), 2);
        assert_eq!(count("?\"hi\",1,2\nx=1"), 2);
        assert_eq!(count("a\\=2 b..=\"x\" c^^=1 d>>>=2 g=e!=f"), 5);
        assert_eq!(count("x=@0x5f00 y=%0 z=$1 w=~x"), 4);
        assert_eq!(count("function f() if (x) return\nend"), 1);
        assert_eq!(count("x=.5end_=1"), 2);
        // No exponents: `.5e8()` is .5 and a call of e8.
        assert_eq!(count("s-=.5e8()"), 2);
        assert_eq!(count("if (a) do\n f(1)\n g(2)\nend\nh()"), 2);
        assert_eq!(count("::top:: goto top // a comment\n--[[ long\ncomment ]] x=1"), 3);
        assert_eq!(count("local s=[[\nline]] t={1,2,x=3,[4]=5;6}"), 2);
        assert!(parse(b"x=").is_err());
        assert!(parse(b"if x then").is_err());
    }

    #[test]
    fn resolves_and_marks_captured_locals() {
        let c = parse(b"local a=1 local b=2 function f() return a end").unwrap();
        assert!(c.decls[0].captured);
        assert!(!c.decls[1].captured);
    }

    #[test]
    fn strings_are_p8scii_bytes() {
        let toks = lex(b"'\\^w\\f8\\x41\\65\x8b'").unwrap();
        assert_eq!(toks[0].tok, Tok::Str(vec![6, b'w', 12, b'8', 0x41, 65, 0x8b].into()));
        assert_eq!(lex(b"0x1.8").unwrap()[0].tok, Tok::Num(0x18000));
        assert_eq!(lex(b"1..2").unwrap()[1].tok, Tok::Op(".."));
    }
}
