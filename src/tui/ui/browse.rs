//! Browse › Keys in three panes, from [`THREE_PANES`] columns (brief §9): Patterns, the
//! keys of the open pattern and the open key, side by side in one frame.
//!
//! The three screens of the state machine (`Screen::Patterns`, `KeyList`, `KeyDetail`)
//! say which pane has the focus; `→` and `←` open and go back as they always did, which
//! reads as moving the focus. Nothing is read as a cursor moves: a pane fills when its
//! parent row is opened, and until then the pane right of the focus shows the summary
//! the right-hand pane shows in two panes ([`super::patterns::keys_of_selected`],
//! [`value_of_selected`]).
//!
//! - Focus on Patterns: two panes, Patterns as wide as in two panes and at its right
//!   the selected row's summary, or the key list while the selected row is the pattern
//!   whose keys were opened (`ui/patterns.rs` draws it).
//! - Focus on the key list: Patterns narrow (name, memory or keys, share, `▲`), the key
//!   list with every column it shows in two panes at 120, and the selected key's facts
//!   (or its value while it is the key whose value was opened).
//! - Focus on the value: Patterns narrow, the key list narrow (name, size, expires) and
//!   the value pane.
//!
//! The pure width rule is [`widths`].

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::tui::app::App;
use crate::tui::app::views::Ctx;

use super::widgets::{Title, frame_panes, rows};
use super::{keys, patterns, style, value};

/// From this body width on, Browse › Keys draws three panes.
pub const THREE_PANES: u16 = 120;

/// The narrow Patterns pane beside the focused key list, and beside a value.
const PATTERNS_BY_LIST: u16 = 34;
const PATTERNS_BY_VALUE: u16 = 30;
/// With `v` the Patterns pane narrows to this, or hides when there is no room for it.
const PATTERNS_MIN: u16 = 30;
/// The focused key list at 120 columns (KEY, SIZE, EXPIRES and TYPE, the name ample).
const LIST_AT_120: u16 = 50;
/// The narrow key list beside a value (name, size and expires).
const LIST_BY_VALUE: u16 = 34;
/// The pane right of the focused key list (the key's facts) never gets less.
const RIGHT_MIN: u16 = 30;

/// Which pane has the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Patterns,
    Keys,
    Value,
}

/// The panes' widths, each from its left edge (the frame's border or a divider) to the
/// next divider; the last includes the right border. A pane of width 0 is not drawn.
/// On Patterns' focus `keys` is 0: the pane at its right is `right`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Widths {
    pub patterns: u16,
    pub keys: u16,
    pub right: u16,
}

/// The three panes' widths on a body `total` wide (at least [`THREE_PANES`]), with `focus`
/// and `more` (`v`). `need` is the width the focused key list shows all its columns at,
/// its name 28 cells ([`keys::list_need`]).
///
/// Extra width goes to the focused pane first (up to what it can use), then to the
/// pane at its right.
///
/// - Patterns: [`patterns::tree_width`], as in two panes; the rest at its right.
/// - Key list: 34 / 50 / 36 at 120 (the facts never under 30); wider terminals give
///   the list more up to `need`, then the facts. With `v` the list asks for `need`
///   at once, and Patterns narrows to 30 or hides when there is no room for it.
/// - Value: Patterns 30 and the list 34; the value the rest.
pub fn widths(total: u16, focus: Focus, more: bool, need: u16) -> Widths {
    match focus {
        Focus::Patterns => {
            let p = patterns::tree_width(total, more);
            Widths {
                patterns: p,
                keys: 0,
                right: total.saturating_sub(p),
            }
        }
        Focus::Keys if more => {
            let want = need.max(LIST_AT_120);
            let list = want.min(total.saturating_sub(RIGHT_MIN));
            let room = total.saturating_sub(list + RIGHT_MIN);
            let patterns = if room >= PATTERNS_MIN {
                room.min(PATTERNS_BY_LIST)
            } else {
                0
            };
            Widths {
                patterns,
                keys: list,
                right: total.saturating_sub(patterns + list),
            }
        }
        Focus::Keys => {
            // 34 / 50 / 36 at 120; what is wider goes to the list until it shows every
            // column, then to the facts.
            let spare = total.saturating_sub(THREE_PANES);
            let list = LIST_AT_120
                .saturating_add(spare)
                .min(need.max(LIST_AT_120))
                .min(total.saturating_sub(PATTERNS_BY_LIST + RIGHT_MIN));
            Widths {
                patterns: PATTERNS_BY_LIST,
                keys: list,
                right: total.saturating_sub(PATTERNS_BY_LIST + list),
            }
        }
        Focus::Value => Widths {
            patterns: PATTERNS_BY_VALUE,
            keys: LIST_BY_VALUE,
            right: total.saturating_sub(PATTERNS_BY_VALUE + LIST_BY_VALUE),
        },
    }
}

/// The value pane's width (border included) on a body `total` wide.
pub(super) fn value_pane_width(app: &App, total: u16) -> u16 {
    widths(total, Focus::Value, app.more_here(), 0).right
}

/// True when the open value is a row of the key list held: opened from this list
/// (the same pattern or find) and its key among the loaded rows.
pub(super) fn value_in_list(app: &App) -> bool {
    let keys = &app.views.keys;
    let v = &app.views.value;
    v.row.as_ref().is_some_and(|open| {
        v.pattern == keys.source() && keys.rows.iter().any(|r| r.key == open.key)
    })
}

/// True when the pane right of the focused key list shows the open value rather than
/// the selected key's facts: while the selected key is the key whose value was opened
/// from this list (and is still held).
pub(super) fn value_of_selected(app: &App) -> bool {
    let keys = &app.views.keys;
    let v = &app.views.value;
    match (keys.selected(), &v.row) {
        (Some(sel), Some(open)) => sel.key == open.key && v.pattern == keys.source(),
        _ => false,
    }
}

/// Draws Browse › Keys in three panes with the focus on the key list or the value (on
/// Patterns `ui/patterns.rs` draws its two).
pub fn draw(frame: &mut Frame, area: Rect, app: &App, focus: Focus) {
    let Some(ctx) = app.ctx() else {
        return;
    };
    let state = &app.views.keys;
    let need = keys::list_need(state.lfu, state.by_node());
    let w = widths(area.width, focus, ctx.more, need);
    let tree = patterns::tree(app);
    let (p_title, p_right) = match &tree {
        Some(t) => patterns::titles(app, t, w.patterns),
        None => ("Patterns".to_string(), String::new()),
    };
    let (k_title, k_right) = keys::list_titles(app);
    // The narrow list beside a value: no right title, as in two panes.
    let k_right = if focus == Focus::Keys {
        k_right
    } else {
        String::new()
    };
    let shows_value = focus == Focus::Value || value_of_selected(app);
    let (r_title, r_right) = if shows_value {
        value::value_titles(&app.views.value, &ctx)
    } else {
        keys::key_titles(state)
    };
    let focus_at = match focus {
        Focus::Patterns => 0,
        Focus::Keys => 1,
        Focus::Value => 2,
    };
    let inner = frame_panes(
        frame,
        area,
        &[w.patterns, w.keys, w.right],
        &[
            Title::new(&p_title, &p_right),
            Title::new(&k_title, &k_right),
            Title::new(&r_title, &r_right),
        ],
        focus_at,
    );
    let (p, k, r) = (inner[0], inner[1], inner[2]);
    if w.patterns > 0 {
        match &tree {
            // Beside a value from another list no pattern is marked as its parent.
            Some(t) => patterns::narrow_pane(
                frame,
                p,
                app,
                t,
                focus != Focus::Value || value_in_list(app),
            ),
            None => no_patterns(frame, p),
        }
    }
    match focus {
        Focus::Value => value::key_column(frame, k, app, true),
        _ => keys::list_pane(frame, k, app, &ctx, true),
    }
    if shows_value {
        value::open_key_pane(frame, r, &app.views.value, &ctx, true);
    } else {
        keys::key_detail(frame, r, app, &ctx);
    }
    if w.patterns > 0
        && let Some(t) = &tree
    {
        patterns::strip(frame, area, w.patterns, app, t);
    }
    forms(frame, area, app, &ctx, focus);
}

/// An edit form open on the list or the value, over the frame.
fn forms(frame: &mut Frame, area: Rect, app: &App, _ctx: &Ctx<'_>, focus: Focus) {
    let form = match focus {
        Focus::Value => app.views.value.form.as_ref(),
        _ => app.views.keys.form.as_ref(),
    };
    if let Some(form) = form {
        value::draw_form(frame, area, form);
    }
}

/// The Patterns pane with no rows to show yet (a find opened before the names were
/// read).
fn no_patterns(frame: &mut Frame, inner: Rect) {
    rows(
        frame,
        inner,
        vec![
            Line::default(),
            Line::from(Span::styled(" No patterns to show yet.", style::dim())),
        ],
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(w: Widths) -> u16 {
        w.patterns + w.keys + w.right
    }

    /// Every pane's width at 120, 140, 160 and 200, on each focus, with and without
    /// `v`, and the columns each pane shows there.
    #[test]
    fn the_width_split_at_each_size_focus_and_v() {
        // A standalone server with LFU off: the key list needs every column.
        let need = keys::list_need(false, false);
        assert_eq!(need, 73);
        let cases: [(u16, Focus, bool, [u16; 3]); 24] = [
            (120, Focus::Patterns, false, [74, 0, 46]),
            (140, Focus::Patterns, false, [86, 0, 54]),
            (160, Focus::Patterns, false, [98, 0, 62]),
            (200, Focus::Patterns, false, [123, 0, 77]),
            (120, Focus::Patterns, true, [84, 0, 36]),
            (140, Focus::Patterns, true, [98, 0, 42]),
            (160, Focus::Patterns, true, [112, 0, 48]),
            (200, Focus::Patterns, true, [140, 0, 60]),
            // The focused list first, up to every column; then the facts.
            (120, Focus::Keys, false, [34, 50, 36]),
            (140, Focus::Keys, false, [34, 70, 36]),
            (160, Focus::Keys, false, [34, 73, 53]),
            (200, Focus::Keys, false, [34, 73, 93]),
            (120, Focus::Keys, true, [0, 73, 47]),
            (140, Focus::Keys, true, [34, 73, 33]),
            (160, Focus::Keys, true, [34, 73, 53]),
            (200, Focus::Keys, true, [34, 73, 93]),
            (120, Focus::Value, false, [30, 34, 56]),
            (140, Focus::Value, false, [30, 34, 76]),
            (160, Focus::Value, false, [30, 34, 96]),
            (200, Focus::Value, false, [30, 34, 136]),
            (120, Focus::Value, true, [30, 34, 56]),
            (140, Focus::Value, true, [30, 34, 76]),
            (160, Focus::Value, true, [30, 34, 96]),
            (200, Focus::Value, true, [30, 34, 136]),
        ];
        for (total, focus, more, want) in cases {
            let w = widths(total, focus, more, need);
            assert_eq!(
                [w.patterns, w.keys, w.right],
                want,
                "{total} {focus:?} v {more}"
            );
            assert_eq!(sum(w), total, "{total} {focus:?} v {more}");
            // The facts beside the focused list never under 30.
            if focus == Focus::Keys {
                assert!(w.right >= RIGHT_MIN, "{total} v {more}");
            }
        }
        // What each pane shows. Focus on Patterns: its two-pane columns.
        assert_eq!(
            patterns::shown_columns(74, false, false, true),
            ["PATTERN", "TYPE", "KEYS", "MEMORY", "SHARE", "▲"]
        );
        assert_eq!(
            patterns::shown_columns(84, false, true, true),
            [
                "PATTERN", "TYPE", "KEYS", "MEMORY", "SHARE", "AVG", "NO TTL", "Δ", "▲"
            ]
        );
        // Beside the keys: name, memory (keys without sizes) and the mark.
        for p in [30, 34] {
            assert_eq!(
                patterns::shown_columns(p, true, false, true),
                ["PATTERN", "MEM", "▲"]
            );
            assert_eq!(
                patterns::shown_columns(p, true, false, false),
                ["PATTERN", "KEYS", "▲"]
            );
        }
        // The focused list at 120: KEY, SIZE, EXPIRES and TYPE; the rest join once the
        // name keeps 28 cells (from 73 wide), or with `v`.
        let w = widths(120, Focus::Keys, false, need);
        assert_eq!(
            keys::shown_columns(w.keys, true, false, false),
            ["KEY", "SIZE", "EXPIRES", "TYPE"]
        );
        let all = ["KEY", "SIZE", "EXPIRES", "TYPE", "STORED AS", "LENGTH"];
        assert_eq!(keys::shown_columns(72, true, false, false), all[..4]);
        assert_eq!(keys::shown_columns(73, true, false, false), all);
        for total in [160, 200] {
            let w = widths(total, Focus::Keys, false, need);
            assert_eq!(keys::shown_columns(w.keys, true, false, false), all);
        }
        for total in [120, 140, 160, 200] {
            let w = widths(total, Focus::Keys, true, need);
            assert_eq!(
                keys::shown_columns_v(w.keys, true, true, false, false),
                all,
                "{total}"
            );
        }
        // With LFU counters, LFU joins with the others.
        let lfu = keys::list_need(true, false);
        let w = widths(200, Focus::Keys, false, lfu);
        assert_eq!(
            keys::shown_columns(w.keys, true, true, false),
            [
                "KEY",
                "SIZE",
                "EXPIRES",
                "TYPE",
                "STORED AS",
                "LENGTH",
                "LFU"
            ]
        );
        // On a cluster NODE joins first, before STORED AS, LENGTH and LFU.
        let node = keys::list_need(false, true);
        let w = widths(120, Focus::Keys, false, node);
        assert_eq!([w.patterns, w.keys, w.right], [34, 50, 36]);
        assert_eq!(
            keys::shown_columns(w.keys, true, false, true),
            ["KEY", "SIZE", "EXPIRES", "TYPE"]
        );
        let w = widths(140, Focus::Keys, false, node);
        assert_eq!(
            keys::shown_columns(w.keys, true, false, true),
            ["KEY", "NODE", "SIZE", "EXPIRES", "TYPE"]
        );
        let w = widths(200, Focus::Keys, false, node);
        assert_eq!(
            keys::shown_columns(w.keys, true, false, true),
            [
                "KEY",
                "NODE",
                "SIZE",
                "EXPIRES",
                "TYPE",
                "STORED AS",
                "LENGTH"
            ]
        );
        // The narrow key list beside a value: name, size and expires.
        const { assert!(LIST_BY_VALUE >= 34, "SIZE and EXPIRES need room") };
    }

    /// Below 120 nothing changes: the two-pane widths are what they were.
    #[test]
    fn below_120_the_two_pane_widths_stay() {
        assert_eq!(patterns::tree_width(80, false), 41);
        assert_eq!(keys::list_width(80, false), 41);
        assert_eq!(value::list_width(80), 28);
        const { assert!(THREE_PANES == 120) };
    }
}
