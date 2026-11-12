//! [`Memo`]: decoded, previewed and rendered values kept between frames.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use super::{Decoded, Format, Raw, Rendered, decode, preview, render};

/// The most decoded values a [`Memo`] holds (each up to [`super::MAX_DECOMPRESSED`]).
const MAX_DECODED: usize = 8;
/// The most rendered values a [`Memo`] holds.
const MAX_RENDERED: usize = 8;
/// The most previews a [`Memo`] holds (each a short line).
const MAX_PREVIEWS: usize = 4096;

/// Keeps [`decode`], [`render`] and [`preview`] results between frames, so a screen
/// drawn ten times a second doesn't decompress and parse its values again each time.
///
/// Entries are keyed by the [`Raw`]'s id, which names its bytes for as long as it
/// lives, so a hit is always the result for those bytes. It is bounded: when a table
/// is full it is emptied and filled again. `Debug` shows only how many entries it holds.
/// Draw functions take `&self`, so it locks inside.
#[derive(Default)]
pub struct Memo(Mutex<Tables>);

#[derive(Default)]
struct Tables {
    decoded: HashMap<u64, Arc<Decoded>>,
    rendered: HashMap<(u64, Format, usize), Arc<Rendered>>,
    previews: HashMap<(u64, usize), String>,
}

impl Memo {
    fn tables(&self) -> MutexGuard<'_, Tables> {
        // Every update is a plain insert, so a panic elsewhere leaves it consistent.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// [`decode`] of `raw`.
    pub fn decode(&self, raw: &Raw) -> Arc<Decoded> {
        if raw.id() == 0 {
            return Arc::new(decode(raw.as_bytes()));
        }
        let mut t = self.tables();
        if let Some(d) = t.decoded.get(&raw.id()) {
            return Arc::clone(d);
        }
        if t.decoded.len() >= MAX_DECODED {
            t.decoded.clear();
        }
        let d = Arc::new(decode(raw.as_bytes()));
        t.decoded.insert(raw.id(), Arc::clone(&d));
        d
    }

    /// [`decode`] and [`render`] of `raw` at `width`, in the format `pick` chooses from
    /// the decoded value (the one `f` picked, or the detected one).
    pub fn render(
        &self,
        raw: &Raw,
        pick: impl FnOnce(&Decoded) -> Format,
        width: usize,
    ) -> (Arc<Decoded>, Arc<Rendered>) {
        let d = self.decode(raw);
        let format = pick(&d);
        if raw.id() == 0 {
            return (Arc::clone(&d), Arc::new(render(&d, format, width)));
        }
        let key = (raw.id(), format, width);
        let mut t = self.tables();
        if let Some(r) = t.rendered.get(&key) {
            return (d, Arc::clone(r));
        }
        if t.rendered.len() >= MAX_RENDERED {
            t.rendered.clear();
        }
        let r = Arc::new(render(&d, format, width));
        t.rendered.insert(key, Arc::clone(&r));
        (d, r)
    }

    /// [`preview`] of `raw` in at most `max_chars` characters.
    pub fn preview(&self, raw: &Raw, max_chars: usize) -> String {
        if raw.id() == 0 {
            return preview(raw.as_bytes(), max_chars);
        }
        let key = (raw.id(), max_chars);
        if let Some(p) = self.tables().previews.get(&key) {
            return p.clone();
        }
        // Not under the lock: a preview decodes the whole value.
        let p = preview(raw.as_bytes(), max_chars);
        let mut t = self.tables();
        if t.previews.len() >= MAX_PREVIEWS {
            t.previews.clear();
        }
        t.previews.insert(key, p.clone());
        p
    }
}

impl fmt::Debug for Memo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let t = self.tables();
        f.debug_struct("Memo")
            .field("decoded", &t.decoded.len())
            .field("rendered", &t.rendered.len())
            .field("previews", &t.previews.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hit_is_always_for_the_same_bytes() {
        let m = Memo::default();
        let a = Raw::from("{\"a\":1}");
        let b = Raw::from("plain");
        assert_eq!(m.preview(&a, 20), preview(a.as_bytes(), 20));
        assert_eq!(m.preview(&b, 20), "plain");
        let c = a.clone();
        assert!(Arc::ptr_eq(&m.decode(&a), &m.decode(&c)));
        let (_, r1) = m.render(&a, |d| d.detected, 40);
        let (_, r2) = m.render(&a, |_| Format::Hex, 40);
        assert_eq!(r1.format, Format::Json);
        assert_eq!(r2.format, Format::Hex);
        assert_eq!(
            *m.render(&b, |d| d.detected, 40).1,
            render(&decode(b"plain"), Format::Text, 40)
        );
    }

    #[test]
    fn tables_stay_bounded() {
        let m = Memo::default();
        let values: Vec<Raw> = (0..50)
            .map(|i| Raw::from(format!("v{i}").as_str()))
            .collect();
        for v in &values {
            m.decode(v);
            m.render(v, |d| d.detected, 10);
        }
        let t = m.tables();
        assert!(t.decoded.len() <= MAX_DECODED);
        assert!(t.rendered.len() <= MAX_RENDERED);
    }

    #[test]
    fn debug_shows_counts_only() {
        let m = Memo::default();
        let v = Raw::from("s3cr3t-v4lue");
        m.preview(&v, 40);
        m.render(&v, |d| d.detected, 40);
        let dbg = format!("{m:?}");
        assert!(!dbg.contains("s3cr3t"), "{dbg}");
        assert!(dbg.contains("previews: 1"));
    }
}
