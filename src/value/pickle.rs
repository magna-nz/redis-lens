//! Python Pickle, protocols 0 to 5, as a tree. A reader only: it runs the pickle
//! machine's stack and memo over plain data, and never imports, calls or builds an
//! object. `GLOBAL`, `STACK_GLOBAL`, `REDUCE`, `NEWOBJ`, `INST`, `OBJ` and `BUILD` are
//! drawn as text: `module.name(args)`, with a `BUILD` state below it.
//!
//! Bounded for untrusted bytes: at most [`MAX_STACK`] items on the stack (marks
//! included), [`MAX_MEMO`] memo entries and [`MAX_OBJECTS`] objects; strings keep at
//! most [`MAX_STR`] bytes; no length the input claims is allocated before it is read.
//! The memo shares objects, so a pickle can hold cycles and shared parts: drawing
//! stops at [`MAX_DEPTH`] and at a cycle, and at the tree's caps.

use std::collections::HashMap;
use std::fmt::Write as _;

use super::tree::{MAX_DEPTH, MAX_STR, Tree, ascii_text, bytes_text};

/// The most items on the stack, marks included.
pub(super) const MAX_STACK: usize = 65_536;
/// The most memo entries.
pub(super) const MAX_MEMO: usize = 65_536;
/// The most objects a pickle makes.
pub(super) const MAX_OBJECTS: usize = 200_000;
/// Arguments drawn inside `name(…)` when this short; longer ones go on lines below.
const INLINE_ARGS: usize = 120;

/// Starts with `PROTO` 2 to 5, and is one complete pickle ending at its `STOP`: what
/// detection calls Pickle. Protocols 0 and 1 have no header, so they are only shown
/// when picked with `f`.
pub(super) fn detect(b: &[u8]) -> bool {
    matches!(b, [0x80, 2..=5, ..]) && matches!(run(b), Ok((_, end)) if end == b.len())
}

/// Writes the pickle `b` holds into `out`. False when it is not a valid pickle (or
/// is bigger than the caps), and then nothing is written.
pub(super) fn render(b: &[u8], out: &mut Tree) -> bool {
    let Ok((m, _)) = run(b) else {
        return false;
    };
    let Some(&root) = m.result.as_ref() else {
        return false;
    };
    let mut path = Vec::new();
    m.emit(root, 0, String::new(), &mut path, out);
    true
}

/// What an object is. Containers keep their children in [`Obj::items`] and
/// [`Obj::entries`]; anything can get items, entries and a state, the way the pickle
/// machine adds them (`APPEND`, `SETITEM`, `BUILD`) to whatever is on the stack.
enum Kind {
    None,
    Bool(bool),
    /// An integer, as decimal text (or `0x…` hex for a long one).
    Int(String),
    Float(f64),
    /// Text, cut at [`MAX_STR`], and its whole length in bytes.
    Str(String, usize),
    Bytes(String, usize),
    ByteArray(String, usize),
    List,
    Tuple,
    Dict,
    Set,
    FrozenSet,
    /// `module.name`.
    Global(String),
    /// A call that was not made: `func(*args, **kwargs)`.
    Call {
        func: usize,
        args: usize,
        kwargs: Option<usize>,
    },
    Ext(u32),
    PersId(String),
    PersObj(usize),
    Buffer,
}

struct Obj {
    kind: Kind,
    items: Vec<usize>,
    entries: Vec<(usize, usize)>,
    state: Option<usize>,
}

#[derive(Default)]
struct Machine {
    objs: Vec<Obj>,
    stack: Vec<usize>,
    /// Stack lengths at each `MARK`.
    marks: Vec<usize>,
    memo: HashMap<u64, usize>,
    result: Option<usize>,
}

/// Why a pickle was not read. Never shown: the value falls back to text or hex.
#[derive(Debug)]
struct Invalid;

type R<T> = Result<T, Invalid>;

struct Input<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Input<'a> {
    fn byte(&mut self) -> R<u8> {
        let c = *self.b.get(self.i).ok_or(Invalid)?;
        self.i += 1;
        Ok(c)
    }

    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.i.checked_add(n).ok_or(Invalid)?;
        let s = self.b.get(self.i..end).ok_or(Invalid)?;
        self.i = end;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> R<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| Invalid)
    }

    fn u32(&mut self) -> R<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn len8(&mut self) -> R<usize> {
        usize::try_from(u64::from_le_bytes(self.array()?)).map_err(|_| Invalid)
    }

    /// Up to `\n`, which is consumed.
    fn line(&mut self) -> R<&'a [u8]> {
        let rest = &self.b[self.i..];
        let n = rest.iter().position(|&c| c == b'\n').ok_or(Invalid)?;
        self.i += n + 1;
        Ok(&rest[..n])
    }

    fn decimal_line(&mut self) -> R<u64> {
        let l = self.line()?;
        if l.is_empty() || l.len() > 19 || !l.iter().all(u8::is_ascii_digit) {
            return Err(Invalid);
        }
        std::str::from_utf8(l)
            .map_err(|_| Invalid)?
            .parse()
            .map_err(|_| Invalid)
    }
}

/// Runs the machine to `STOP`: the machine and where the pickle ended.
fn run(b: &[u8]) -> R<(Machine, usize)> {
    let mut m = Machine::default();
    let mut inp = Input { b, i: 0 };
    loop {
        let op = inp.byte()?;
        if op == b'.' {
            let top = m.pop()?;
            m.result = Some(top);
            return Ok((m, inp.i));
        }
        m.step(op, &mut inp)?;
    }
}

/// `n` little-endian two's complement bytes as an integer's text.
fn long_text(b: &[u8]) -> String {
    if b.is_empty() {
        return "0".to_string();
    }
    if b.len() <= 16 {
        let neg = b[b.len() - 1] & 0x80 != 0;
        let mut buf = [if neg { 0xff } else { 0 }; 16];
        buf[..b.len()].copy_from_slice(b);
        return i128::from_le_bytes(buf).to_string();
    }
    let mut s = String::from("0x");
    let shown = b.len().min(MAX_STR / 2);
    for x in b.iter().rev().take(shown) {
        let _ = write!(s, "{x:02x}");
    }
    if shown < b.len() {
        s.push('…');
    }
    format!("{s} ({}-byte int)", b.len())
}

fn int_text(l: &[u8]) -> R<String> {
    let digits = l.strip_prefix(b"-").unwrap_or(l);
    let digits = digits.strip_prefix(b"+").unwrap_or(digits);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(Invalid);
    }
    Ok(bytes_text(l))
}

/// A protocol 0 quoted string line, its quotes taken off.
fn unquote(l: &[u8]) -> R<&[u8]> {
    match l {
        [q @ (b'\'' | b'"'), body @ .., e] if e == q => Ok(body),
        _ => Err(Invalid),
    }
}

impl Machine {
    fn new_obj(&mut self, kind: Kind) -> R<usize> {
        if self.objs.len() >= MAX_OBJECTS {
            return Err(Invalid);
        }
        self.objs.push(Obj {
            kind,
            items: Vec::new(),
            entries: Vec::new(),
            state: None,
        });
        Ok(self.objs.len() - 1)
    }

    fn push(&mut self, o: usize) -> R<()> {
        if self.stack.len() + self.marks.len() >= MAX_STACK {
            return Err(Invalid);
        }
        self.stack.push(o);
        Ok(())
    }

    fn push_new(&mut self, kind: Kind) -> R<()> {
        let o = self.new_obj(kind)?;
        self.push(o)
    }

    /// The stack's floor: the last mark's position.
    fn floor(&self) -> usize {
        self.marks.last().copied().unwrap_or(0)
    }

    fn pop(&mut self) -> R<usize> {
        if self.stack.len() <= self.floor() {
            return Err(Invalid);
        }
        self.stack.pop().ok_or(Invalid)
    }

    fn top(&self) -> R<usize> {
        if self.stack.len() <= self.floor() {
            return Err(Invalid);
        }
        self.stack.last().copied().ok_or(Invalid)
    }

    /// Everything since the last mark, and the mark.
    fn pop_mark(&mut self) -> R<Vec<usize>> {
        let at = self.marks.pop().ok_or(Invalid)?;
        Ok(self.stack.split_off(at.min(self.stack.len())))
    }

    fn memo_put(&mut self, k: u64) -> R<()> {
        let top = self.top()?;
        if self.memo.len() >= MAX_MEMO && !self.memo.contains_key(&k) {
            return Err(Invalid);
        }
        self.memo.insert(k, top);
        Ok(())
    }

    fn memo_get(&mut self, k: u64) -> R<()> {
        let o = *self.memo.get(&k).ok_or(Invalid)?;
        self.push(o)
    }

    fn str_obj(&mut self, b: &[u8]) -> R<()> {
        self.push_new(Kind::Str(bytes_text(b), b.len()))
    }

    fn container(&mut self, kind: Kind, items: Vec<usize>) -> R<()> {
        let o = self.new_obj(kind)?;
        self.objs[o].items = items;
        self.push(o)
    }

    fn call(&mut self, func: usize, args: usize, kwargs: Option<usize>) -> R<()> {
        self.push_new(Kind::Call { func, args, kwargs })
    }

    fn global(&mut self, module: &[u8], name: &[u8]) -> R<()> {
        self.push_new(Kind::Global(format!(
            "{}.{}",
            bytes_text(module),
            bytes_text(name)
        )))
    }

    fn step(&mut self, op: u8, inp: &mut Input<'_>) -> R<()> {
        match op {
            // MARK
            b'(' => {
                if self.stack.len() + self.marks.len() >= MAX_STACK {
                    return Err(Invalid);
                }
                self.marks.push(self.stack.len());
            }
            // POP: the top item, or the mark when nothing is above it.
            b'0' => {
                if self.stack.len() <= self.floor() && !self.marks.is_empty() {
                    self.marks.pop();
                } else {
                    self.pop()?;
                }
            }
            // POP_MARK
            b'1' => {
                self.pop_mark()?;
            }
            // DUP
            b'2' => {
                let t = self.top()?;
                self.push(t)?;
            }
            // FLOAT
            b'F' => {
                let l = inp.line()?;
                let f: f64 = std::str::from_utf8(l)
                    .map_err(|_| Invalid)?
                    .trim()
                    .parse()
                    .map_err(|_| Invalid)?;
                self.push_new(Kind::Float(f))?;
            }
            // INT, with protocol 0's booleans
            b'I' => {
                let l = inp.line()?;
                let kind = match l {
                    b"00" => Kind::Bool(false),
                    b"01" => Kind::Bool(true),
                    l => Kind::Int(int_text(l)?),
                };
                self.push_new(kind)?;
            }
            // BININT, BININT1, BININT2
            b'J' => {
                let v = i32::from_le_bytes(inp.array()?);
                self.push_new(Kind::Int(v.to_string()))?;
            }
            b'K' => {
                let v = inp.byte()?;
                self.push_new(Kind::Int(v.to_string()))?;
            }
            b'M' => {
                let v = u16::from_le_bytes(inp.array()?);
                self.push_new(Kind::Int(v.to_string()))?;
            }
            // LONG: `123L`
            b'L' => {
                let l = inp.line()?;
                let l = l.strip_suffix(b"L").unwrap_or(l);
                self.push_new(Kind::Int(int_text(l)?))?;
            }
            // LONG1, LONG4
            0x8a => {
                let n = usize::from(inp.byte()?);
                let b = inp.take(n)?;
                self.push_new(Kind::Int(long_text(b)))?;
            }
            0x8b => {
                let n = i32::from_le_bytes(inp.array()?);
                let b = inp.take(usize::try_from(n).map_err(|_| Invalid)?)?;
                self.push_new(Kind::Int(long_text(b)))?;
            }
            // NONE, NEWTRUE, NEWFALSE
            b'N' => self.push_new(Kind::None)?,
            0x88 => self.push_new(Kind::Bool(true))?,
            0x89 => self.push_new(Kind::Bool(false))?,
            // BINFLOAT
            b'G' => {
                let f = f64::from_be_bytes(inp.array()?);
                self.push_new(Kind::Float(f))?;
            }
            // STRING, BINSTRING, SHORT_BINSTRING (Python 2 str)
            b'S' => {
                let l = inp.line()?;
                let s = unquote(l)?;
                self.str_obj(s)?;
            }
            b'T' => {
                let n = i32::from_le_bytes(inp.array()?);
                let s = inp.take(usize::try_from(n).map_err(|_| Invalid)?)?;
                self.str_obj(s)?;
            }
            b'U' => {
                let n = usize::from(inp.byte()?);
                let s = inp.take(n)?;
                self.str_obj(s)?;
            }
            // UNICODE (raw-unicode-escape, shown as written), BINUNICODE,
            // SHORT_BINUNICODE, BINUNICODE8
            b'V' => {
                let l = inp.line()?;
                self.str_obj(l)?;
            }
            b'X' => {
                let n = inp.u32()? as usize;
                let s = inp.take(n)?;
                self.str_obj(s)?;
            }
            0x8c => {
                let n = usize::from(inp.byte()?);
                let s = inp.take(n)?;
                self.str_obj(s)?;
            }
            0x8d => {
                let n = inp.len8()?;
                let s = inp.take(n)?;
                self.str_obj(s)?;
            }
            // BINBYTES, SHORT_BINBYTES, BINBYTES8, BYTEARRAY8
            b'B' => {
                let n = inp.u32()? as usize;
                let s = inp.take(n)?;
                self.push_new(Kind::Bytes(ascii_text(s), s.len()))?;
            }
            b'C' => {
                let n = usize::from(inp.byte()?);
                let s = inp.take(n)?;
                self.push_new(Kind::Bytes(ascii_text(s), s.len()))?;
            }
            0x8e => {
                let n = inp.len8()?;
                let s = inp.take(n)?;
                self.push_new(Kind::Bytes(ascii_text(s), s.len()))?;
            }
            0x96 => {
                let n = inp.len8()?;
                let s = inp.take(n)?;
                self.push_new(Kind::ByteArray(ascii_text(s), s.len()))?;
            }
            // NEXT_BUFFER, READONLY_BUFFER
            0x97 => self.push_new(Kind::Buffer)?,
            0x98 => {
                self.top()?;
            }
            // EMPTY_LIST, EMPTY_TUPLE, EMPTY_DICT, EMPTY_SET
            b']' => self.container(Kind::List, Vec::new())?,
            b')' => self.container(Kind::Tuple, Vec::new())?,
            b'}' => self.container(Kind::Dict, Vec::new())?,
            0x8f => self.container(Kind::Set, Vec::new())?,
            // LIST, TUPLE, FROZENSET, DICT
            b'l' => {
                let items = self.pop_mark()?;
                self.container(Kind::List, items)?;
            }
            b't' => {
                let items = self.pop_mark()?;
                self.container(Kind::Tuple, items)?;
            }
            0x91 => {
                let items = self.pop_mark()?;
                self.container(Kind::FrozenSet, items)?;
            }
            b'd' => {
                let items = self.pop_mark()?;
                let (pairs, rest) = items.as_chunks::<2>();
                if !rest.is_empty() {
                    return Err(Invalid);
                }
                let o = self.new_obj(Kind::Dict)?;
                self.objs[o].entries = pairs.iter().map(|[k, v]| (*k, *v)).collect();
                self.push(o)?;
            }
            // TUPLE1, TUPLE2, TUPLE3
            0x85..=0x87 => {
                let n = usize::from(op - 0x84);
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.pop()?);
                }
                items.reverse();
                self.container(Kind::Tuple, items)?;
            }
            // APPEND, APPENDS, ADDITEMS
            b'a' => {
                let v = self.pop()?;
                let t = self.top()?;
                self.objs[t].items.push(v);
            }
            b'e' | 0x90 => {
                let items = self.pop_mark()?;
                let t = self.top()?;
                self.objs[t].items.extend(items);
            }
            // SETITEM, SETITEMS
            b's' => {
                let v = self.pop()?;
                let k = self.pop()?;
                let t = self.top()?;
                self.objs[t].entries.push((k, v));
            }
            b'u' => {
                let items = self.pop_mark()?;
                let (pairs, rest) = items.as_chunks::<2>();
                if !rest.is_empty() {
                    return Err(Invalid);
                }
                let t = self.top()?;
                self.objs[t]
                    .entries
                    .extend(pairs.iter().map(|[k, v]| (*k, *v)));
            }
            // GLOBAL, STACK_GLOBAL
            b'c' => {
                let module = inp.line()?;
                let name = inp.line()?;
                self.global(module, name)?;
            }
            0x93 => {
                let name = self.pop()?;
                let module = self.pop()?;
                let text = |o: usize| match &self.objs[o].kind {
                    Kind::Str(s, _) => Ok(s.clone()),
                    _ => Err(Invalid),
                };
                let g = format!("{}.{}", text(module)?, text(name)?);
                self.push_new(Kind::Global(g))?;
            }
            // REDUCE, NEWOBJ, NEWOBJ_EX: drawn, never called
            b'R' | 0x81 => {
                let args = self.pop()?;
                let func = self.pop()?;
                self.call(func, args, None)?;
            }
            0x92 => {
                let kwargs = self.pop()?;
                let args = self.pop()?;
                let func = self.pop()?;
                self.call(func, args, Some(kwargs))?;
            }
            // INST, OBJ
            b'i' => {
                let module = inp.line()?;
                let name = inp.line()?;
                let items = self.pop_mark()?;
                self.global(module, name)?;
                let func = self.pop()?;
                let args = self.new_obj(Kind::Tuple)?;
                self.objs[args].items = items;
                self.call(func, args, None)?;
            }
            b'o' => {
                let mut items = self.pop_mark()?;
                if items.is_empty() {
                    return Err(Invalid);
                }
                let func = items.remove(0);
                let args = self.new_obj(Kind::Tuple)?;
                self.objs[args].items = items;
                self.call(func, args, None)?;
            }
            // BUILD: the state is shown with the object, never applied
            b'b' => {
                let state = self.pop()?;
                let t = self.top()?;
                self.objs[t].state = Some(state);
            }
            // PERSID, BINPERSID
            b'P' => {
                let l = inp.line()?;
                self.push_new(Kind::PersId(bytes_text(l)))?;
            }
            b'Q' => {
                let o = self.pop()?;
                self.push_new(Kind::PersObj(o))?;
            }
            // EXT1, EXT2, EXT4
            0x82 => {
                let c = inp.byte()?;
                self.push_new(Kind::Ext(u32::from(c)))?;
            }
            0x83 => {
                let c = u16::from_le_bytes(inp.array()?);
                self.push_new(Kind::Ext(u32::from(c)))?;
            }
            0x84 => {
                let c = inp.u32()?;
                self.push_new(Kind::Ext(c))?;
            }
            // PUT, BINPUT, LONG_BINPUT, MEMOIZE
            b'p' => {
                let k = inp.decimal_line()?;
                self.memo_put(k)?;
            }
            b'q' => {
                let k = inp.byte()?;
                self.memo_put(u64::from(k))?;
            }
            b'r' => {
                let k = inp.u32()?;
                self.memo_put(u64::from(k))?;
            }
            0x94 => {
                let k = self.memo.len() as u64;
                self.memo_put(k)?;
            }
            // GET, BINGET, LONG_BINGET
            b'g' => {
                let k = inp.decimal_line()?;
                self.memo_get(k)?;
            }
            b'h' => {
                let k = inp.byte()?;
                self.memo_get(u64::from(k))?;
            }
            b'j' => {
                let k = inp.u32()?;
                self.memo_get(u64::from(k))?;
            }
            // PROTO
            0x80 => {
                if inp.byte()? > 5 {
                    return Err(Invalid);
                }
            }
            // FRAME: a size hint, nothing to do
            0x95 => {
                inp.len8()?;
            }
            _ => return Err(Invalid),
        }
        Ok(())
    }

    /// The one-line text of object `o`, Python style: `'a'`, `[1, 2]`, `{'k': 1}`,
    /// `module.name(args)`. Stops past `limit` bytes.
    fn repr(&self, o: usize, depth: usize, path: &mut Vec<usize>, out: &mut String, limit: usize) {
        if out.len() > limit {
            return;
        }
        if depth > MAX_DEPTH || path.contains(&o) {
            out.push_str("...");
            return;
        }
        path.push(o);
        let obj = &self.objs[o];
        let seq = |m: &Self, open: &str, close: &str, path: &mut Vec<usize>, out: &mut String| {
            out.push_str(open);
            for (k, &c) in obj.items.iter().enumerate() {
                if k > 0 {
                    out.push_str(", ");
                }
                if out.len() > limit {
                    break;
                }
                m.repr(c, depth + 1, path, out, limit);
            }
            out.push_str(close);
        };
        match &obj.kind {
            Kind::List => seq(self, "[", "]", path, out),
            Kind::Tuple if obj.items.len() == 1 => seq(self, "(", ",)", path, out),
            Kind::Tuple => seq(self, "(", ")", path, out),
            Kind::Set if obj.items.is_empty() => out.push_str("set()"),
            Kind::Set => seq(self, "{", "}", path, out),
            Kind::FrozenSet => seq(self, "frozenset({", "})", path, out),
            Kind::Dict => {
                out.push('{');
                for (k, &(key, v)) in obj.entries.iter().enumerate() {
                    if k > 0 {
                        out.push_str(", ");
                    }
                    if out.len() > limit {
                        break;
                    }
                    self.repr(key, depth + 1, path, out, limit);
                    out.push_str(": ");
                    self.repr(v, depth + 1, path, out, limit);
                }
                out.push('}');
            }
            Kind::Call { func, args, kwargs } => {
                self.repr_call(*func, *args, *kwargs, depth, path, out, limit);
            }
            _ => out.push_str(&self.scalar(o)),
        }
        path.pop();
    }

    #[allow(clippy::too_many_arguments)]
    fn repr_call(
        &self,
        func: usize,
        args: usize,
        kwargs: Option<usize>,
        depth: usize,
        path: &mut Vec<usize>,
        out: &mut String,
        limit: usize,
    ) {
        self.callee(func, depth, path, out, limit);
        out.push('(');
        self.args_text(args, kwargs, depth, path, out, limit);
        out.push(')');
    }

    /// The function of a call: a global's name, or the object's text.
    fn callee(
        &self,
        func: usize,
        depth: usize,
        path: &mut Vec<usize>,
        out: &mut String,
        limit: usize,
    ) {
        match &self.objs[func].kind {
            Kind::Global(g) => out.push_str(g),
            _ => self.repr(func, depth + 1, path, out, limit),
        }
    }

    /// A call's arguments without the brackets: `1, 'a', k=2`.
    fn args_text(
        &self,
        args: usize,
        kwargs: Option<usize>,
        depth: usize,
        path: &mut Vec<usize>,
        out: &mut String,
        limit: usize,
    ) {
        let a = &self.objs[args];
        let mut first = true;
        if matches!(a.kind, Kind::Tuple) {
            for &c in &a.items {
                if !first {
                    out.push_str(", ");
                }
                first = false;
                if out.len() > limit {
                    return;
                }
                self.repr(c, depth + 1, path, out, limit);
            }
        } else {
            out.push('*');
            self.repr(args, depth + 1, path, out, limit);
            first = false;
        }
        if let Some(kw) = kwargs {
            let k = &self.objs[kw];
            if matches!(k.kind, Kind::Dict) {
                for &(key, v) in &k.entries {
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    if out.len() > limit {
                        return;
                    }
                    match &self.objs[key].kind {
                        Kind::Str(s, _) => out.push_str(s),
                        _ => self.repr(key, depth + 1, path, out, limit),
                    }
                    out.push('=');
                    self.repr(v, depth + 1, path, out, limit);
                }
            } else {
                if !first {
                    out.push_str(", ");
                }
                out.push_str("**");
                self.repr(kw, depth + 1, path, out, limit);
            }
        }
    }

    /// The text of a leaf.
    fn scalar(&self, o: usize) -> String {
        let more = |len: usize| {
            if len > MAX_STR {
                format!(" ({len} bytes)")
            } else {
                String::new()
            }
        };
        match &self.objs[o].kind {
            Kind::None => "None".to_string(),
            Kind::Bool(true) => "True".to_string(),
            Kind::Bool(false) => "False".to_string(),
            Kind::Int(s) => s.clone(),
            Kind::Float(f) => format!("{f:?}"),
            Kind::Str(s, n) => format!("'{s}'{}", more(*n)),
            Kind::Bytes(s, n) => format!("b'{s}'{}", more(*n)),
            Kind::ByteArray(s, n) => format!("bytearray(b'{s}'){}", more(*n)),
            Kind::Global(g) => g.clone(),
            Kind::Ext(c) => format!("extension code {c}"),
            Kind::PersId(s) => format!("persistent id '{s}'"),
            Kind::PersObj(_) => "persistent id".to_string(),
            Kind::Buffer => "out-of-band buffer".to_string(),
            Kind::List => "list".to_string(),
            Kind::Tuple => "tuple".to_string(),
            Kind::Dict => "dict".to_string(),
            Kind::Set => "set".to_string(),
            Kind::FrozenSet => "frozenset".to_string(),
            Kind::Call { .. } => "call".to_string(),
        }
    }

    /// Object `o` as tree lines: its line `prefix` and what it is, then its items,
    /// entries and state below it.
    fn emit(&self, o: usize, depth: usize, prefix: String, path: &mut Vec<usize>, out: &mut Tree) {
        if out.full() {
            return;
        }
        if depth > MAX_DEPTH {
            out.push(depth, format!("{prefix}…"));
            return;
        }
        if path.contains(&o) {
            out.push(depth, format!("{prefix}... (contains itself)"));
            return;
        }
        let obj = &self.objs[o];
        let mut args_below = None;
        let head = match &obj.kind {
            Kind::List => format!("list ({})", obj.items.len()),
            Kind::Tuple => format!("tuple ({})", obj.items.len()),
            Kind::Set => format!("set ({})", obj.items.len()),
            Kind::FrozenSet => format!("frozenset ({})", obj.items.len()),
            Kind::Dict => format!("dict ({})", obj.entries.len()),
            Kind::Call { func, args, kwargs } => {
                path.push(o);
                let mut name = String::new();
                self.callee(*func, depth, path, &mut name, INLINE_ARGS);
                let mut a = String::new();
                self.args_text(*args, *kwargs, depth, path, &mut a, INLINE_ARGS);
                path.pop();
                if a.len() <= INLINE_ARGS {
                    format!("{name}({a})")
                } else {
                    args_below = Some((*args, *kwargs));
                    format!("{name}(…)")
                }
            }
            Kind::PersObj(inner) => {
                let mut s = String::from("persistent id ");
                path.push(o);
                self.repr(*inner, depth + 1, path, &mut s, INLINE_ARGS);
                path.pop();
                s
            }
            _ => self.scalar(o),
        };
        out.push(depth, format!("{prefix}{head}"));
        path.push(o);
        if let Some((args, kwargs)) = args_below {
            self.emit(args, depth + 1, "args: ".to_string(), path, out);
            if let Some(kw) = kwargs {
                self.emit(kw, depth + 1, "kwargs: ".to_string(), path, out);
            }
        }
        for &c in &obj.items {
            if out.full() {
                break;
            }
            self.emit(c, depth + 1, String::new(), path, out);
        }
        for &(k, v) in &obj.entries {
            if out.full() {
                break;
            }
            let mut key = String::new();
            self.repr(k, depth + 1, path, &mut key, 80);
            if key.len() > 80 {
                let mut end = 80;
                while !key.is_char_boundary(end) {
                    end -= 1;
                }
                key.truncate(end);
                key.push('…');
            }
            self.emit(v, depth + 1, format!("{key}: "), path, out);
        }
        if let Some(s) = obj.state {
            self.emit(s, depth + 1, "state: ".to_string(), path, out);
        }
        path.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(b: &[u8]) -> Vec<String> {
        let mut t = Tree::new();
        assert!(render(b, &mut t), "{b:?}");
        t.lines()
            .iter()
            .map(|(d, s)| format!("{}{s}", "  ".repeat(*d)))
            .collect()
    }

    // Written by CPython 3.9's `pickle.dumps`.
    /// `{'name': 'bob', 'tags': ['a', 'b'], 'n': 3, 'ok': True, 'x': None, 'f': 1.5,
    /// 'b': b'\x00ab'}`, protocol 4.
    const PROTO4: &[u8] = b"\x80\x04\x95K\x00\x00\x00\x00\x00\x00\x00}\x94(\x8c\x04name\x94\x8c\x03bob\x94\x8c\x04tags\x94]\x94(\x8c\x01a\x94\x8c\x01b\x94e\x8c\x01n\x94K\x03\x8c\x02ok\x94\x88\x8c\x01x\x94N\x8c\x01f\x94G?\xf8\x00\x00\x00\x00\x00\x00h\x06C\x03\x00ab\x94u.";
    /// `[1, (2, 'x'), {'k': -5}]`, protocol 0.
    const PROTO0: &[u8] =
        b"(lp0\x0aI1\x0aa(I2\x0aVx\x0ap1\x0atp2\x0aa(dp3\x0aVk\x0ap4\x0aI-5\x0asa.";
    /// `{'a': [1.25, 'z']}`, protocol 1.
    const PROTO1: &[u8] = b"}q\x00X\x01\x00\x00\x00aq\x01]q\x02(G?\xf4\x00\x00\x00\x00\x00\x00X\x01\x00\x00\x00zq\x03es.";
    /// `(b'raw', {1, 2}, frozenset([3]))`, protocol 3.
    const PROTO3: &[u8] = b"\x80\x03C\x03rawq\x00cbuiltins\x0aset\x0aq\x01]q\x02(K\x01K\x02e\x85q\x03Rq\x04cbuiltins\x0afrozenset\x0aq\x05]q\x06K\x03a\x85q\x07Rq\x08\x87q\x09.";
    /// `[bytearray(b'ba'), 2**70, -3]`, protocol 5.
    const PROTO5: &[u8] = b"\x80\x05\x95!\x00\x00\x00\x00\x00\x00\x00]\x94(\x96\x02\x00\x00\x00\x00\x00\x00\x00ba\x94\x8a\x09\x00\x00\x00\x00\x00\x00\x00\x00@J\xfd\xff\xff\xffe.";
    /// `datetime.date(2024, 1, 2)`, protocol 2: a REDUCE of a GLOBAL.
    const DATE: &[u8] = b"\x80\x02cdatetime\x0adate\x0aq\x00c_codecs\x0aencode\x0aq\x01X\x05\x00\x00\x00\x07\xc3\xa8\x01\x02q\x02X\x06\x00\x00\x00latin1q\x03\x86q\x04Rq\x05\x85q\x06Rq\x07.";
    /// An instance of `myapp.User` with `id = 7` and `name = 'ann'`, protocol 4:
    /// STACK_GLOBAL, NEWOBJ and BUILD.
    const USER: &[u8] = b"\x80\x04\x95.\x00\x00\x00\x00\x00\x00\x00\x8c\x05myapp\x94\x8c\x04User\x94\x93\x94)\x81\x94}\x94(\x8c\x02id\x94K\x07\x8c\x04name\x94\x8c\x03ann\x94ub.";
    /// `collections.OrderedDict([('a', 1)])`, protocol 2: items set on a REDUCE.
    const ODICT: &[u8] =
        b"\x80\x02ccollections\x0aOrderedDict\x0aq\x00)Rq\x01X\x01\x00\x00\x00aq\x02K\x01s.";

    #[test]
    fn protocol_4_dict() {
        assert!(detect(PROTO4));
        assert_eq!(
            lines(PROTO4),
            [
                "dict (7)",
                "  'name': 'bob'",
                "  'tags': list (2)",
                "    'a'",
                "    'b'",
                "  'n': 3",
                "  'ok': True",
                "  'x': None",
                "  'f': 1.5",
                "  'b': b'\\x00ab'",
            ]
        );
    }

    #[test]
    fn protocols_0_and_1_are_only_shown_when_picked() {
        assert!(!detect(PROTO0));
        assert_eq!(
            lines(PROTO0),
            [
                "list (3)",
                "  1",
                "  tuple (2)",
                "    2",
                "    'x'",
                "  dict (1)",
                "    'k': -5",
            ]
        );
        assert!(!detect(PROTO1));
        assert_eq!(
            lines(PROTO1),
            ["dict (1)", "  'a': list (2)", "    1.25", "    'z'"]
        );
    }

    #[test]
    fn protocols_3_and_5() {
        assert!(detect(PROTO3) && detect(PROTO5));
        assert_eq!(
            lines(PROTO3),
            [
                "tuple (3)",
                "  b'raw'",
                "  builtins.set([1, 2])",
                "  builtins.frozenset([3])",
            ]
        );
        assert_eq!(
            lines(PROTO5),
            [
                "list (3)",
                "  bytearray(b'ba')",
                "  1180591620717411303424",
                "  -3",
            ]
        );
    }

    #[test]
    fn globals_and_reduce_are_text_never_called() {
        assert!(detect(DATE));
        assert_eq!(
            lines(DATE),
            ["datetime.date(_codecs.encode('\u{7}è\u{1}\u{2}', 'latin1'))"]
        );
        // `os.system('echo hi')` as a REDUCE: drawn, not run.
        let evil = b"\x80\x04\x8c\x02os\x8c\x06system\x93\x8c\x07echo hi\x85R.";
        assert_eq!(lines(evil), ["os.system('echo hi')"]);
    }

    #[test]
    fn build_and_setitems_show_below_the_object() {
        assert_eq!(
            lines(USER),
            [
                "myapp.User()",
                "  state: dict (2)",
                "    'id': 7",
                "    'name': 'ann'",
            ]
        );
        assert_eq!(lines(ODICT), ["collections.OrderedDict()", "  'a': 1"]);
    }

    #[test]
    fn long_arguments_go_below_the_call() {
        let long = "x".repeat(200);
        let mut b = b"\x80\x04\x8c\x01m\x8c\x01f\x93X".to_vec();
        b.extend((long.len() as u32).to_le_bytes());
        b.extend(long.as_bytes());
        b.extend(b"\x85R.");
        let l = lines(&b);
        assert_eq!(l[0], "m.f(…)");
        assert_eq!(l[1], "  args: tuple (1)");
        assert!(l[2].starts_with("    'xxx"));
    }

    #[test]
    fn cycles_and_shared_parts_stay_small() {
        // l = []; l.append(l)
        let b = b"\x80\x02]q\x00h\x00a.";
        assert_eq!(lines(b), ["list (1)", "  ... (contains itself)"]);
        // x = (x, x) thirty times over: 2^30 paths in 150 bytes of pickle.
        let mut p = b"\x80\x02N".to_vec();
        for i in 0..30u8 {
            p.extend([b'q', i, b'h', i, 0x86]);
        }
        p.push(b'.');
        let started = std::time::Instant::now();
        let mut t = Tree::new();
        assert!(render(&p, &mut t));
        assert!(t.lines().len() <= super::super::tree::MAX_NODES);
        let mut s = String::new();
        let m = run(&p).unwrap().0;
        m.repr(m.result.unwrap(), 0, &mut Vec::new(), &mut s, 200);
        assert!(s.len() < 400);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn longs_and_big_ints() {
        assert_eq!(long_text(&[0xff]), "-1");
        assert_eq!(long_text(&[0x00, 0x01]), "256");
        assert_eq!(long_text(&[]), "0");
        assert!(long_text(&[1; 20]).ends_with("(20-byte int)"));
        let b = b"\x80\x02\x8a\x09\x00\x00\x00\x00\x00\x00\x00\x00\x01.";
        assert_eq!(lines(b), ["18446744073709551616"]);
    }

    #[test]
    fn invalid_pickles_are_refused() {
        for bad in [
            &b""[..],
            b"\x80\x02",
            b"\x80\x06N.",
            b"\x80\x02a.",
            b"\x80\x02h\x05.",
            b"\x80\x02X\xff\xff\xff\xff.",
            b"\x80\x02\x8e\xff\xff\xff\xff\xff\xff\xff\xff.",
            b"\x80\x02(.",
            b"\x80\x02\xff.",
            b"\x80\x02N(\x85.",
        ] {
            assert!(!render(bad, &mut Tree::new()), "{bad:?}");
            assert!(!detect(bad));
        }
        // Trailing bytes after STOP: shown when picked, not detected.
        assert!(render(b"\x80\x02N.xx", &mut Tree::new()));
        assert!(!detect(b"\x80\x02N.xx"));
    }

    #[test]
    fn the_stack_and_memo_are_capped() {
        let mut b = b"\x80\x02".to_vec();
        b.extend(std::iter::repeat_n(b'N', MAX_STACK + 1));
        b.push(b'.');
        assert!(!render(&b, &mut Tree::new()));
        let mut b = b"\x80\x04N".to_vec();
        b.extend(std::iter::repeat_n(0x94u8, MAX_MEMO + 1));
        b.push(b'.');
        assert!(!render(&b, &mut Tree::new()));
        let mut b = b"\x80\x04N".to_vec();
        b.extend(std::iter::repeat_n(0x94u8, MAX_MEMO));
        b.push(b'.');
        assert!(render(&b, &mut Tree::new()));
    }
}
