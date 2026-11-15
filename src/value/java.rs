//! Java serialization (`AC ED 00 05`), read in part: objects with their class names,
//! field names and primitive values, strings, enums, arrays, class references and
//! block data. Whatever it does not understand (an `Externalizable` written in
//! protocol 1, a corrupt stream, a cap reached) is shown as a hex tail from there, so
//! everything read before it stays on screen. Never an error.
//!
//! Bounded for untrusted bytes: at most [`MAX_HANDLES`] handles, nesting at most
//! [`MAX_DEPTH`] deep, no array or string allocated by a length before its bytes are
//! there.

use std::fmt::Write as _;

use super::tree::{MAX_DEPTH, Tree, bytes_text, hex_head, hex_tail};

/// The stream header: magic and version 5.
pub(super) const MAGIC: [u8; 4] = [0xac, 0xed, 0x00, 0x05];
/// The most handles (objects, classes and strings a stream can refer back to).
pub(super) const MAX_HANDLES: usize = 100_000;
/// The first handle number.
const BASE_HANDLE: u32 = 0x7e_0000;
/// Array elements drawn; the rest are read and counted.
const ARRAY_SHOWN: usize = 64;

const SC_WRITE_METHOD: u8 = 0x01;
const SC_SERIALIZABLE: u8 = 0x02;
const SC_EXTERNALIZABLE: u8 = 0x04;
const SC_BLOCK_DATA: u8 = 0x08;
const SC_ENUM: u8 = 0x10;

const TC_NULL: u8 = 0x70;
const TC_REFERENCE: u8 = 0x71;
const TC_CLASSDESC: u8 = 0x72;
const TC_OBJECT: u8 = 0x73;
const TC_STRING: u8 = 0x74;
const TC_ARRAY: u8 = 0x75;
const TC_CLASS: u8 = 0x76;
const TC_BLOCKDATA: u8 = 0x77;
const TC_ENDBLOCKDATA: u8 = 0x78;
const TC_RESET: u8 = 0x79;
const TC_BLOCKDATALONG: u8 = 0x7a;
const TC_EXCEPTION: u8 = 0x7b;
const TC_LONGSTRING: u8 = 0x7c;
const TC_PROXYCLASSDESC: u8 = 0x7d;
const TC_ENUM: u8 = 0x7e;

/// Starts with the magic and version: what detection and `f` call Java.
pub(super) fn detect(b: &[u8]) -> bool {
    b.starts_with(&MAGIC)
}

/// Writes what can be read of the stream into `out`. False when `b` lacks the header.
pub(super) fn render(b: &[u8], out: &mut Tree) -> bool {
    if !detect(b) {
        return false;
    }
    let mut r = Reader {
        b,
        i: MAGIC.len(),
        handles: Vec::new(),
        spare: Handle::Pending,
        quiet: 0,
        at: MAGIC.len(),
        out,
    };
    if b.len() == MAGIC.len() {
        r.out.push(0, "empty stream".to_string());
    }
    while r.i < b.len() && !r.out.full() {
        if r.content(0, String::new()).is_err() {
            let at = r.at.min(b.len());
            hex_tail(r.out, 0, at, &b[at..]);
            break;
        }
    }
    true
}

struct Field {
    code: u8,
    name: String,
}

struct Desc {
    name: String,
    flags: u8,
    fields: Vec<Field>,
    sup: Option<usize>,
}

enum Handle {
    Desc(Desc),
    Str(String),
    /// An object, array, enum or class, by what it is called on screen.
    Named(String),
    /// Assigned, not filled in yet.
    Pending,
}

struct Reader<'a, 'o> {
    b: &'a [u8],
    i: usize,
    handles: Vec<Handle>,
    /// Where a write to a handle lands when a `TC_RESET` inside the item has cleared the
    /// handle table since the handle was assigned: it is thrown away.
    spare: Handle,
    /// Above 0, nothing is written (class annotations).
    quiet: usize,
    /// Where the item being read started: a hex tail starts there.
    at: usize,
    out: &'o mut Tree,
}

impl Reader<'_, '_> {
    /// Handle `k`, or `Pending` when a `TC_RESET` has cleared the table since `k` was
    /// read: corrupted or hostile bytes must never index out of bounds.
    fn handle(&self, k: usize) -> &Handle {
        self.handles.get(k).unwrap_or(&Handle::Pending)
    }

    /// Handle `h` to fill in, or a spare slot when a `TC_RESET` has cleared the table
    /// since `h` was assigned.
    fn handle_mut(&mut self, h: usize) -> &mut Handle {
        match self.handles.get_mut(h) {
            Some(slot) => slot,
            None => &mut self.spare,
        }
    }
}

/// The stream is not understood from here.
struct Stop;

type R<T> = Result<T, Stop>;

impl<'a> Reader<'a, '_> {
    fn emit(&mut self, depth: usize, text: String) {
        if self.quiet == 0 {
            self.out.push(depth, text);
        }
    }

    fn peek(&self) -> R<u8> {
        self.b.get(self.i).copied().ok_or(Stop)
    }

    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.i.checked_add(n).ok_or(Stop)?;
        let s = self.b.get(self.i..end).ok_or(Stop)?;
        self.i = end;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> R<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| Stop)
    }

    fn u8(&mut self) -> R<u8> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> R<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn i32(&mut self) -> R<i32> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    /// A modified UTF-8 string with a 2-byte length.
    fn utf(&mut self) -> R<String> {
        let n = usize::from(self.u16()?);
        Ok(bytes_text(self.take(n)?))
    }

    /// A modified UTF-8 string with an 8-byte length.
    fn long_utf(&mut self) -> R<String> {
        let n = usize::try_from(u64::from_be_bytes(self.array()?)).map_err(|_| Stop)?;
        Ok(bytes_text(self.take(n)?))
    }

    fn new_handle(&mut self, h: Handle) -> R<usize> {
        if self.handles.len() >= MAX_HANDLES {
            return Err(Stop);
        }
        self.handles.push(h);
        Ok(self.handles.len() - 1)
    }

    fn handle_ref(&mut self) -> R<usize> {
        let h = u32::from_be_bytes(self.array()?);
        let k = h.checked_sub(BASE_HANDLE).ok_or(Stop)? as usize;
        if k >= self.handles.len() {
            return Err(Stop);
        }
        Ok(k)
    }

    /// One content item: an object, string, array, enum, class, null, reference or
    /// block data, as a line `prefix` and what it is, its parts below.
    fn content(&mut self, depth: usize, prefix: String) -> R<()> {
        self.at = self.i;
        if depth > MAX_DEPTH {
            return Err(Stop);
        }
        let tc = self.peek()?;
        match tc {
            TC_NULL => {
                self.i += 1;
                self.emit(depth, format!("{prefix}null"));
            }
            TC_REFERENCE => {
                self.i += 1;
                let k = self.handle_ref()?;
                let text = match self.handle(k) {
                    Handle::Str(s) => format!("\"{s}\""),
                    Handle::Named(n) => format!("{n} (again)"),
                    Handle::Desc(d) => format!("class {}", d.name),
                    Handle::Pending => "itself".to_string(),
                };
                self.emit(depth, format!("{prefix}{text}"));
            }
            TC_STRING | TC_LONGSTRING => {
                let s = self.string()?;
                self.emit(depth, format!("{prefix}\"{s}\""));
            }
            TC_OBJECT => {
                self.i += 1;
                self.object(depth, prefix)?;
            }
            TC_ARRAY => {
                self.i += 1;
                self.array_item(depth, prefix)?;
            }
            TC_ENUM => {
                self.i += 1;
                let d = self.class_desc(depth)?.ok_or(Stop)?;
                let h = self.new_handle(Handle::Pending)?;
                let name = self.string()?;
                let class = self.desc_name(d);
                let text = format!("enum {class}.{name}");
                *self.handle_mut(h) = Handle::Named(text.clone());
                self.emit(depth, format!("{prefix}{text}"));
            }
            TC_CLASS => {
                self.i += 1;
                let d = self.class_desc(depth)?.ok_or(Stop)?;
                let text = format!("class {}", self.desc_name(d));
                self.new_handle(Handle::Named(text.clone()))?;
                self.emit(depth, format!("{prefix}{text}"));
            }
            TC_CLASSDESC | TC_PROXYCLASSDESC => {
                let d = self.class_desc(depth)?.ok_or(Stop)?;
                let text = format!("class description {}", self.desc_name(d));
                self.emit(depth, format!("{prefix}{text}"));
            }
            TC_BLOCKDATA | TC_BLOCKDATALONG => {
                self.i += 1;
                let n = if tc == TC_BLOCKDATA {
                    usize::from(self.u8()?)
                } else {
                    usize::try_from(self.i32()?).map_err(|_| Stop)?
                };
                let data = self.take(n)?;
                self.emit(
                    depth,
                    format!("{prefix}block data ({n} bytes) {}", hex_head(data, 32)),
                );
            }
            TC_RESET => {
                self.i += 1;
                self.handles.clear();
            }
            TC_EXCEPTION => {
                self.i += 1;
                self.handles.clear();
                self.emit(depth, format!("{prefix}exception"));
                self.content(depth + 1, String::new())?;
                self.handles.clear();
            }
            _ => return Err(Stop),
        }
        Ok(())
    }

    /// A string item (`TC_STRING`, `TC_LONGSTRING` or a reference to one).
    fn string(&mut self) -> R<String> {
        let s = match self.u8()? {
            TC_STRING => self.utf()?,
            TC_LONGSTRING => self.long_utf()?,
            TC_REFERENCE => {
                let k = self.handle_ref()?;
                return match self.handle(k) {
                    Handle::Str(s) => Ok(s.clone()),
                    _ => Err(Stop),
                };
            }
            _ => return Err(Stop),
        };
        self.new_handle(Handle::Str(s.clone()))?;
        Ok(s)
    }

    fn desc_name(&self, d: usize) -> String {
        match self.handle(d) {
            Handle::Desc(d) => d.name.clone(),
            _ => "?".to_string(),
        }
    }

    /// A class descriptor: new, a proxy, a reference to one, or null (`None`).
    fn class_desc(&mut self, depth: usize) -> R<Option<usize>> {
        self.at = self.i;
        if depth > MAX_DEPTH {
            return Err(Stop);
        }
        match self.u8()? {
            TC_NULL => Ok(None),
            TC_REFERENCE => {
                let k = self.handle_ref()?;
                match *self.handle(k) {
                    Handle::Desc(_) => Ok(Some(k)),
                    _ => Err(Stop),
                }
            }
            TC_CLASSDESC => {
                let name = self.utf()?;
                let _suid = self.take(8)?;
                let h = self.new_handle(Handle::Pending)?;
                let flags = self.u8()?;
                let count = usize::from(self.u16()?);
                let mut fields = Vec::new();
                for _ in 0..count {
                    let code = self.u8()?;
                    let fname = self.utf()?;
                    match code {
                        b'L' | b'[' => {
                            // The field's class name: the value says its own class.
                            self.string()?;
                        }
                        c if prim_size(c).is_some() => {}
                        _ => return Err(Stop),
                    }
                    fields.push(Field { code, name: fname });
                }
                self.annotation(depth)?;
                let sup = self.class_desc(depth + 1)?;
                *self.handle_mut(h) = Handle::Desc(Desc {
                    name,
                    flags,
                    fields,
                    sup,
                });
                Ok(Some(h))
            }
            TC_PROXYCLASSDESC => {
                let h = self.new_handle(Handle::Pending)?;
                let n = usize::try_from(self.i32()?).map_err(|_| Stop)?;
                let mut names = Vec::new();
                for _ in 0..n {
                    let s = self.utf()?;
                    if names.len() < 8 {
                        names.push(s);
                    }
                }
                self.annotation(depth)?;
                let sup = self.class_desc(depth + 1)?;
                *self.handle_mut(h) = Handle::Desc(Desc {
                    name: format!("proxy({})", names.join(", ")),
                    flags: SC_SERIALIZABLE,
                    fields: Vec::new(),
                    sup,
                });
                Ok(Some(h))
            }
            _ => Err(Stop),
        }
    }

    /// A class annotation: contents up to `TC_ENDBLOCKDATA`, not drawn.
    fn annotation(&mut self, depth: usize) -> R<()> {
        self.quiet += 1;
        let r = self.contents_until_end(depth + 1);
        self.quiet -= 1;
        r
    }

    /// Contents up to and including `TC_ENDBLOCKDATA`, drawn at `depth`.
    fn contents_until_end(&mut self, depth: usize) -> R<()> {
        loop {
            if self.peek()? == TC_ENDBLOCKDATA {
                self.i += 1;
                return Ok(());
            }
            self.content(depth, String::new())?;
        }
    }

    fn object(&mut self, depth: usize, prefix: String) -> R<()> {
        let d = self.class_desc(depth)?.ok_or(Stop)?;
        let name = self.desc_name(d);
        let h = self.new_handle(Handle::Pending)?;
        self.emit(depth, format!("{prefix}object {name}"));
        // Class data, from the topmost superclass down.
        let mut chain = Vec::new();
        let mut at = Some(d);
        while let Some(k) = at {
            if chain.len() > MAX_DEPTH || chain.contains(&k) {
                return Err(Stop);
            }
            chain.push(k);
            at = match self.handle(k) {
                Handle::Desc(desc) => desc.sup,
                _ => None,
            };
        }
        for &k in chain.iter().rev() {
            let (flags, fields): (u8, Vec<(u8, String)>) = match self.handle(k) {
                Handle::Desc(desc) => (
                    desc.flags,
                    desc.fields
                        .iter()
                        .map(|f| (f.code, f.name.clone()))
                        .collect(),
                ),
                _ => return Err(Stop),
            };
            if flags & SC_EXTERNALIZABLE != 0 {
                if flags & SC_BLOCK_DATA == 0 {
                    return Err(Stop);
                }
                self.contents_until_end(depth + 1)?;
                continue;
            }
            if flags & SC_SERIALIZABLE == 0 || flags & SC_ENUM != 0 {
                continue;
            }
            for (code, fname) in fields {
                self.at = self.i;
                match code {
                    b'L' | b'[' => {
                        self.content(depth + 1, format!("{fname}: "))?;
                    }
                    _ => {
                        let v = self.primitive(code)?;
                        self.emit(depth + 1, format!("{fname}: {v}"));
                    }
                }
            }
            if flags & SC_WRITE_METHOD != 0 {
                self.contents_until_end(depth + 1)?;
            }
        }
        *self.handle_mut(h) = Handle::Named(format!("object {name}"));
        Ok(())
    }

    /// A primitive field's value with its type: `int 3`.
    fn primitive(&mut self, code: u8) -> R<String> {
        let size = prim_size(code).ok_or(Stop)?;
        let v = primitive_value(code, self.take(size)?).ok_or(Stop)?;
        Ok(format!("{} {v}", prim_name(code)))
    }

    fn array_item(&mut self, depth: usize, prefix: String) -> R<()> {
        let d = self.class_desc(depth)?.ok_or(Stop)?;
        let class = self.desc_name(d);
        let h = self.new_handle(Handle::Pending)?;
        let n = usize::try_from(self.i32()?).map_err(|_| Stop)?;
        let code = class.as_bytes().get(1).copied().ok_or(Stop)?;
        let size = match code {
            b'L' | b'[' => 0,
            c => prim_size(c).ok_or(Stop)?,
        };
        let shown = type_name(&class);
        if size > 0 {
            // All the bytes must be there before any is read.
            let data = self.take(n.checked_mul(size).ok_or(Stop)?)?;
            let text = if code == b'B' {
                hex_head(data, 32)
            } else {
                let mut s = String::new();
                for (k, e) in data.chunks(size).take(ARRAY_SHOWN).enumerate() {
                    if k > 0 {
                        s.push_str(", ");
                    }
                    s.push_str(&primitive_value(code, e).ok_or(Stop)?);
                }
                if n > ARRAY_SHOWN {
                    let _ = write!(s, ", … ({} more)", n - ARRAY_SHOWN);
                }
                s
            };
            self.emit(depth, format!("{prefix}{shown} ({n}) {text}"));
        } else {
            self.emit(depth, format!("{prefix}{shown} ({n})"));
            for k in 0..n {
                let q = k >= ARRAY_SHOWN;
                if q {
                    self.quiet += 1;
                }
                let r = self.content(depth + 1, String::new());
                if q {
                    self.quiet -= 1;
                }
                r?;
            }
            if n > ARRAY_SHOWN {
                self.emit(depth + 1, format!("… {} more", n - ARRAY_SHOWN));
            }
        }
        *self.handle_mut(h) = Handle::Named(shown);
        Ok(())
    }
}

/// The bytes a primitive of type `code` takes.
fn prim_size(code: u8) -> Option<usize> {
    Some(match code {
        b'B' | b'Z' => 1,
        b'C' | b'S' => 2,
        b'F' | b'I' => 4,
        b'D' | b'J' => 8,
        _ => return None,
    })
}

fn prim_name(code: u8) -> &'static str {
    match code {
        b'B' => "byte",
        b'C' => "char",
        b'D' => "double",
        b'F' => "float",
        b'I' => "int",
        b'J' => "long",
        b'S' => "short",
        _ => "boolean",
    }
}

/// A primitive's value from its big-endian bytes (exactly [`prim_size`] of them).
fn primitive_value(code: u8, b: &[u8]) -> Option<String> {
    Some(match code {
        b'B' => (i8::from_be_bytes(b.try_into().ok()?)).to_string(),
        b'C' => {
            let c = u16::from_be_bytes(b.try_into().ok()?);
            match char::from_u32(u32::from(c)) {
                Some(ch) => format!("'{ch}'"),
                None => format!("\\u{c:04x}"),
            }
        }
        b'D' => format!("{:?}", f64::from_be_bytes(b.try_into().ok()?)),
        b'F' => format!("{:?}", f32::from_be_bytes(b.try_into().ok()?)),
        b'I' => i32::from_be_bytes(b.try_into().ok()?).to_string(),
        b'J' => i64::from_be_bytes(b.try_into().ok()?).to_string(),
        b'S' => i16::from_be_bytes(b.try_into().ok()?).to_string(),
        b'Z' => (*b.first()? != 0).to_string(),
        _ => return None,
    })
}

/// `[I` → `int[]`, `[Ljava.lang.String;` → `java.lang.String[]`, `[[B` → `byte[][]`.
fn type_name(class: &str) -> String {
    let dims = class.bytes().take_while(|&c| c == b'[').count();
    let rest = &class[dims.min(class.len())..];
    let base = match rest.as_bytes().first() {
        Some(b'B') => "byte".to_string(),
        Some(b'C') => "char".to_string(),
        Some(b'D') => "double".to_string(),
        Some(b'F') => "float".to_string(),
        Some(b'I') => "int".to_string(),
        Some(b'J') => "long".to_string(),
        Some(b'S') => "short".to_string(),
        Some(b'Z') => "boolean".to_string(),
        Some(b'L') => rest[1..].trim_end_matches(';').to_string(),
        _ => rest.to_string(),
    };
    format!("{base}{}", "[]".repeat(dims))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(b: &[u8]) -> Vec<String> {
        let mut t = Tree::new();
        assert!(render(b, &mut t));
        t.lines()
            .iter()
            .map(|(d, s)| format!("{}{s}", "  ".repeat(*d)))
            .collect()
    }

    /// Written by OpenJDK 21's `ObjectOutputStream`:
    /// `class Point implements Serializable { int x = 3; int y = -4; String label =
    /// "p1"; }`, written once.
    pub(super) const POINT: &[u8] = b"\xac\xed\x00\x05sr\x00\x05Point\x00\x00\x00\x00\x00\x00\x00\x01\x02\x00\x03I\x00\x01xI\x00\x01yL\x00\x05labelt\x00\x12Ljava/lang/String;xp\x00\x00\x00\x03\xff\xff\xff\xfct\x00\x02p1";

    #[test]
    fn an_object_with_fields() {
        assert!(detect(POINT));
        assert_eq!(
            lines(POINT),
            [
                "object Point",
                "  x: int 3",
                "  y: int -4",
                "  label: \"p1\""
            ]
        );
    }

    #[test]
    fn strings_arrays_references_and_block_data() {
        let mut b = MAGIC.to_vec();
        // "hi", then a reference back to it.
        b.extend([TC_STRING, 0, 2, b'h', b'i']);
        b.extend([TC_REFERENCE, 0x00, 0x7e, 0x00, 0x00]);
        // int[] {1, 2}
        b.extend([TC_ARRAY, TC_CLASSDESC, 0, 2, b'[', b'I']);
        b.extend([0; 8]);
        b.extend([SC_SERIALIZABLE, 0, 0, TC_ENDBLOCKDATA, TC_NULL]);
        b.extend([0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 2]);
        b.extend([TC_BLOCKDATA, 2, 0xca, 0xfe]);
        b.push(TC_NULL);
        assert_eq!(
            lines(&b),
            [
                "\"hi\"",
                "\"hi\"",
                "int[] (2) 1, 2",
                "block data (2 bytes) ca fe",
                "null"
            ]
        );
    }

    #[test]
    fn what_is_not_understood_is_a_hex_tail() {
        let mut b = POINT.to_vec();
        b.extend([0x42, 0x43, 0x44]);
        let l = lines(&b);
        assert_eq!(l[..4], lines(POINT)[..]);
        assert_eq!(l[4], format!("not read: 3 bytes from byte {}", POINT.len()));
        assert_eq!(l[5], "  42 43 44");
        // Cut short in the middle of the fields: what was read stays.
        let short = &POINT[..POINT.len() - 3];
        let l = lines(short);
        assert_eq!(l[0], "object Point");
        assert!(l.iter().any(|x| x.starts_with("not read:")), "{l:?}");
        assert_eq!(lines(&MAGIC), ["empty stream"]);
        assert!(!render(b"\xac\xed\x00\x04", &mut Tree::new()));
    }

    #[test]
    fn huge_claims_and_deep_nesting_stop_without_allocating() {
        let mut b = MAGIC.to_vec();
        b.extend([TC_ARRAY, TC_CLASSDESC, 0, 2, b'[', b'J']);
        b.extend([0; 8]);
        b.extend([SC_SERIALIZABLE, 0, 0, TC_ENDBLOCKDATA, TC_NULL]);
        b.extend([0x7f, 0xff, 0xff, 0xff]);
        let l = lines(&b);
        assert!(l[0].starts_with("not read:"), "{l:?}");
        // Exceptions nested ten thousand deep.
        let mut b = MAGIC.to_vec();
        b.extend(vec![TC_EXCEPTION; 10_000]);
        let l = lines(&b);
        assert!(l.len() < MAX_DEPTH + 40, "{}", l.len());
        assert!(l.iter().any(|x| x.starts_with("not read:")));
    }

    #[test]
    fn array_type_names() {
        assert_eq!(type_name("[I"), "int[]");
        assert_eq!(type_name("[[B"), "byte[][]");
        assert_eq!(type_name("[Ljava.lang.String;"), "java.lang.String[]");
    }
}
