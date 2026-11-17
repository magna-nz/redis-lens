//! Colours from the design mockup, in three modes: the dark palette (the default), a
//! light one for light terminals (`[settings] theme = "light"`), and none at all when
//! `NO_COLOR` is set and not empty (<https://no-color.org>).
//!
//! The style functions (`dim()`, `warning()` …) read one process-wide mode, set once at TUI
//! start by [`init`]. Each is a thin wrapper over a pure `role(Role, Mode)` that the
//! tests call with an explicit mode, so they never touch the global. Nothing in the UI
//! tells a state by colour alone: every state has its words (`read-only`, `✗ failed`,
//! `PROD`), so the colourless mode only has to keep selection, badges and the active tab
//! apart, with reverse video and bold.
//!
//! Roles, by meaning rather than by colour (`docs/ui-redesign.md` section 4):
//!
//! - **focus** (blue): where the keys go: the focused pane, the current panel, the `›`
//!   marker, the input cursor and key hints;
//! - **placeholder** (violet): the `{id}` parts of a pattern;
//! - **warning** (amber): `▲` findings and the sentence after them, `read-only`, `never`
//!   in an expiry column, a restriction or a degraded state, work in progress;
//! - **danger** (red): the PROD badge, `writes on`, `✗` and errors, destructive actions;
//! - **ok** (teal): `●` connected, `✓` analyzed, live;
//! - **data** (a quieter blue): bars, meters, heat bars and sparklines: quantities.

use std::sync::atomic::{AtomicU8, Ordering};

use ratatui::style::{Color, Modifier, Style};

use crate::model::Env;
use crate::write::Theme;

// The dark palette. Kept as constants: the snapshot tests name them.
pub const BG: Color = Color::Rgb(0x12, 0x18, 0x21);
pub const FG: Color = Color::Rgb(0xc6, 0xcf, 0xdb);
pub const HI: Color = Color::Rgb(0xee, 0xf2, 0xf7);
pub const DIM: Color = Color::Rgb(0x6d, 0x78, 0x89);
pub const FAINT: Color = Color::Rgb(0x2b, 0x34, 0x41);
pub const BORDER: Color = Color::Rgb(0x3a, 0x46, 0x58);
pub const SEL: Color = Color::Rgb(0x23, 0x30, 0x44);
/// Warning: `▲` findings, `read-only`, `never` in an expiry column.
pub const AMBER: Color = Color::Rgb(0xe8, 0xb4, 0x5c);
/// Danger: PROD, `writes on`, errors, destructive actions.
pub const RED: Color = Color::Rgb(0xf0, 0x76, 0x62);
pub const TEAL: Color = Color::Rgb(0x5f, 0xc6, 0xb4);
/// Focus: the focused pane's border and title, the current panel, the `›` marker, the
/// input cursor and key hints.
pub const FOCUS: Color = Color::Rgb(0x6c, 0xb6, 0xff);
/// `{placeholders}` in patterns.
pub const PH: Color = Color::Rgb(0xb7, 0x9c, 0xf5);
/// Data: bars, meters and sparklines.
pub const DATA: Color = Color::Rgb(0x4f, 0x8f, 0xd6);

/// What a colour is for. `bg` is the text colour on a filled badge, not a background
/// that is painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub bg: Color,
    pub fg: Color,
    pub hi: Color,
    pub dim: Color,
    pub faint: Color,
    pub border: Color,
    pub sel: Color,
    /// The warning colour (amber).
    pub warning: Color,
    /// The danger colour (red).
    pub danger: Color,
    pub teal: Color,
    pub focus: Color,
    pub ph: Color,
    /// Bars, meters and sparklines.
    pub data: Color,
}

pub const DARK_PALETTE: Palette = Palette {
    bg: BG,
    fg: FG,
    hi: HI,
    dim: DIM,
    faint: FAINT,
    border: BORDER,
    sel: SEL,
    warning: AMBER,
    danger: RED,
    teal: TEAL,
    focus: FOCUS,
    ph: PH,
    data: DATA,
};

/// The same roles for a light terminal: dark text, and accents dark enough to read on
/// white (each at least 4.5:1 against `#ffffff`, and white text on them the same).
pub const LIGHT_PALETTE: Palette = Palette {
    bg: Color::Rgb(0xff, 0xff, 0xff),
    fg: Color::Rgb(0x2b, 0x34, 0x40),
    hi: Color::Rgb(0x0f, 0x17, 0x20),
    dim: Color::Rgb(0x5d, 0x68, 0x79),
    faint: Color::Rgb(0xd9, 0xde, 0xe6),
    border: Color::Rgb(0xb4, 0xbc, 0xc8),
    sel: Color::Rgb(0xdd, 0xe6, 0xf5),
    warning: Color::Rgb(0x8f, 0x5e, 0x00),
    danger: Color::Rgb(0xc0, 0x39, 0x2b),
    teal: Color::Rgb(0x0f, 0x76, 0x6e),
    focus: Color::Rgb(0x0b, 0x5c, 0xad),
    ph: Color::Rgb(0x6f, 0x42, 0xc1),
    data: Color::Rgb(0x2e, 0x6d, 0xb5),
};

/// How the screen is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Dark,
    Light,
    /// `NO_COLOR`: no colour; reverse video and bold carry selection, badges and the
    /// active tab.
    Mono,
}

impl Mode {
    /// The mode for a theme setting and whether `NO_COLOR` asks for none.
    pub fn resolve(theme: Theme, no_color: bool) -> Self {
        match (no_color, theme) {
            (true, _) => Self::Mono,
            (false, Theme::Dark) => Self::Dark,
            (false, Theme::Light) => Self::Light,
        }
    }

    fn palette(self) -> Option<&'static Palette> {
        match self {
            Self::Dark => Some(&DARK_PALETTE),
            Self::Light => Some(&LIGHT_PALETTE),
            Self::Mono => None,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Self::Dark => 0,
            Self::Light => 1,
            Self::Mono => 2,
        }
    }

    fn from_u8(n: u8) -> Self {
        match n {
            1 => Self::Light,
            2 => Self::Mono,
            _ => Self::Dark,
        }
    }
}

/// Whether `NO_COLOR` is set and not empty, as no-color.org asks.
pub fn no_color_requested() -> bool {
    no_color_value(std::env::var_os("NO_COLOR").as_deref())
}

/// [`no_color_requested`] for a given value of the variable.
pub fn no_color_value(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

static MODE: AtomicU8 = AtomicU8::new(0);

/// Sets the mode every style function below reads. Called once, before the first
/// frame.
pub fn init(theme: Theme, no_color: bool) {
    MODE.store(Mode::resolve(theme, no_color).to_u8(), Ordering::Relaxed);
}

/// The mode in use.
pub fn mode() -> Mode {
    #[cfg(test)]
    if let Some(m) = TEST_MODE.get() {
        return m;
    }
    Mode::from_u8(MODE.load(Ordering::Relaxed))
}

#[cfg(test)]
thread_local! {
    /// A mode for this test's thread only, so a test can draw without colour while the
    /// others, on their own threads, keep the default.
    static TEST_MODE: std::cell::Cell<Option<Mode>> = const { std::cell::Cell::new(None) };
}

/// Runs `f` with the style functions in `m` on this thread only. Tests only.
#[cfg(test)]
pub fn with_mode<R>(m: Mode, f: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_MODE.set(None);
        }
    }
    TEST_MODE.set(Some(m));
    let _reset = Reset;
    f()
}

/// The roles a style is asked for by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Fg,
    Fgb,
    Dim,
    Hd,
    /// Red: PROD, `writes on`, `✗` and errors, destructive actions.
    Danger,
    Ok,
    /// Amber: `▲` findings and notes, `read-only`, `never`, restrictions.
    Warning,
    /// Bars, meters, heat bars and sparklines.
    Data,
    Ph,
    Phb,
    Track,
    Cmd,
    Sel,
    Border,
    Title,
    Brand,
    Tab,
    TabOn,
    Key,
    /// The focus colour alone: a focused border, `›`, the input cursor, key hints.
    Focus,
}

/// The style of `role` in `mode`. Pure.
pub fn role(role: Role, mode: Mode) -> Style {
    let bold = Modifier::BOLD;
    let Some(p) = mode.palette() else {
        // No colour: weight and reverse video only. A state's words stay on screen.
        let plain = Style::new();
        return match role {
            Role::Fg | Role::Ok | Role::Warning | Role::Data | Role::Ph | Role::Cmd => plain,
            Role::Fgb | Role::Hd | Role::Danger | Role::Phb | Role::Title | Role::Focus => {
                plain.add_modifier(bold)
            }
            Role::Dim | Role::Tab | Role::Border | Role::Track => plain.add_modifier(Modifier::DIM),
            Role::Sel | Role::Key => plain.add_modifier(Modifier::REVERSED),
            Role::Brand | Role::TabOn => plain.add_modifier(Modifier::REVERSED | bold),
        };
    };
    match role {
        Role::Fg => Style::new().fg(p.fg),
        Role::Fgb => Style::new().fg(p.hi).add_modifier(bold),
        Role::Dim | Role::Tab => Style::new().fg(p.dim),
        Role::Hd => Style::new().fg(p.dim).add_modifier(bold),
        Role::Danger => Style::new().fg(p.danger),
        Role::Ok => Style::new().fg(p.teal),
        Role::Warning => Style::new().fg(p.warning),
        Role::Data => Style::new().fg(p.data),
        Role::Ph => Style::new().fg(p.ph),
        Role::Phb => Style::new().fg(p.ph).add_modifier(bold),
        Role::Track => Style::new().fg(p.faint),
        Role::Cmd => Style::new().fg(p.hi),
        Role::Sel => Style::new().bg(p.sel),
        Role::Border => Style::new().fg(p.border),
        Role::Title => Style::new().fg(p.focus).add_modifier(bold),
        Role::Brand => Style::new().bg(p.warning).fg(p.bg).add_modifier(bold),
        Role::TabOn => Style::new().bg(p.sel).fg(p.focus).add_modifier(bold),
        Role::Key => Style::new().bg(p.faint).fg(p.hi),
        Role::Focus => Style::new().fg(p.focus),
    }
}

/// The badge of an environment in `mode`: PROD red, STAGE amber, LOCAL teal, OTHER grey;
/// in the colourless mode all four are reverse and bold, told apart by their words.
pub fn env_badge_in(env: Env, mode: Mode) -> Style {
    let s = Style::new().add_modifier(Modifier::BOLD);
    let Some(p) = mode.palette() else {
        return s.add_modifier(Modifier::REVERSED);
    };
    match env {
        Env::Prod => s.bg(p.danger).fg(p.bg),
        Env::Stage => s.bg(p.warning).fg(p.bg),
        Env::Local => s.bg(p.teal).fg(p.bg),
        Env::Other => s.bg(p.faint).fg(p.hi),
    }
}

pub fn fg() -> Style {
    role(Role::Fg, mode())
}
pub fn fgb() -> Style {
    role(Role::Fgb, mode())
}
pub fn dim() -> Style {
    role(Role::Dim, mode())
}
/// Column headings.
pub fn hd() -> Style {
    role(Role::Hd, mode())
}
/// Danger: the PROD badge, `writes on`, `✗` and errors, destructive actions.
pub fn danger() -> Style {
    role(Role::Danger, mode())
}
/// `●` connected, `✓` analyzed, live.
pub fn ok() -> Style {
    role(Role::Ok, mode())
}
/// Warning: `▲` findings and their sentences, `read-only`, `never` in an expiry
/// column, restrictions and degraded states.
pub fn warning() -> Style {
    role(Role::Warning, mode())
}
/// Data: bars, meters, heat bars and sparklines.
pub fn data() -> Style {
    role(Role::Data, mode())
}
/// `{placeholders}` in patterns.
pub fn ph() -> Style {
    role(Role::Ph, mode())
}
pub fn phb() -> Style {
    role(Role::Phb, mode())
}
pub fn track() -> Style {
    role(Role::Track, mode())
}
pub fn cmd() -> Style {
    role(Role::Cmd, mode())
}
pub fn sel() -> Style {
    role(Role::Sel, mode())
}
pub fn border() -> Style {
    role(Role::Border, mode())
}
/// The focused pane's title and the current panel: the focus colour, bold.
pub fn title() -> Style {
    role(Role::Title, mode())
}
/// The focus colour: a focused pane's border, the `›` marker, the input cursor, key
/// hints.
pub fn focus() -> Style {
    role(Role::Focus, mode())
}
pub fn brand() -> Style {
    role(Role::Brand, mode())
}
pub fn tab() -> Style {
    role(Role::Tab, mode())
}
pub fn tab_on() -> Style {
    role(Role::TabOn, mode())
}
/// Key caps in the key bar.
pub fn key() -> Style {
    role(Role::Key, mode())
}

/// What [`super::widgets::dim`] lays over the screen behind a dialog, in `mode`: the dim
/// colour and no bold; in the colourless mode the dim attribute, so it still reads.
pub fn backdrop_in(mode: Mode) -> Style {
    match mode.palette() {
        Some(p) => Style::new()
            .fg(p.dim)
            .remove_modifier(Modifier::BOLD | Modifier::REVERSED),
        None => Style::new()
            .add_modifier(Modifier::DIM)
            .remove_modifier(Modifier::BOLD),
    }
}

/// [`backdrop_in`] the mode in use.
pub fn backdrop() -> Style {
    backdrop_in(mode())
}

/// PROD red, STAGE amber, LOCAL teal, OTHER grey.
pub fn env_badge(env: Env) -> Style {
    env_badge_in(env, mode())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLES: [Role; 20] = [
        Role::Fg,
        Role::Fgb,
        Role::Dim,
        Role::Hd,
        Role::Danger,
        Role::Ok,
        Role::Warning,
        Role::Data,
        Role::Ph,
        Role::Phb,
        Role::Track,
        Role::Cmd,
        Role::Sel,
        Role::Border,
        Role::Title,
        Role::Brand,
        Role::Tab,
        Role::TabOn,
        Role::Key,
        Role::Focus,
    ];

    const ENVS: [Env; 4] = [Env::Prod, Env::Stage, Env::Local, Env::Other];

    #[test]
    fn dark_is_the_default_and_matches_the_constants() {
        assert_eq!(Mode::default(), Mode::Dark);
        assert_eq!(mode(), Mode::Dark);
        assert_eq!(role(Role::Dim, Mode::Dark).fg, Some(DIM));
        assert_eq!(role(Role::Danger, Mode::Dark).fg, Some(RED));
        assert_eq!(role(Role::Ok, Mode::Dark).fg, Some(TEAL));
        assert_eq!(role(Role::Warning, Mode::Dark).fg, Some(AMBER));
        assert_eq!(role(Role::Data, Mode::Dark).fg, Some(DATA));
        assert_eq!(role(Role::Sel, Mode::Dark).bg, Some(SEL));
        assert_eq!(env_badge_in(Env::Prod, Mode::Dark).bg, Some(RED));
        assert_eq!(env_badge_in(Env::Prod, Mode::Dark).fg, Some(BG));
        // The wrappers are the dark roles while nothing sets the mode.
        assert_eq!(dim(), role(Role::Dim, Mode::Dark));
    }

    /// Focus is its own colour, not amber; placeholders have theirs.
    #[test]
    fn focus_and_placeholders_have_their_own_colours() {
        let d = Mode::Dark;
        assert_eq!(role(Role::Focus, d).fg, Some(FOCUS));
        assert_eq!(role(Role::Title, d).fg, Some(FOCUS));
        assert!(role(Role::Title, d).add_modifier.contains(Modifier::BOLD));
        assert_eq!(role(Role::Ph, d).fg, Some(PH));
        assert_eq!(role(Role::Phb, d).fg, Some(PH));
        for r in [Role::Focus, Role::Title, Role::Ph, Role::TabOn] {
            for m in [Mode::Dark, Mode::Light] {
                assert_ne!(role(r, m).fg, Some(mode_amber(m)), "{r:?} {m:?}");
            }
        }
        // Without colour the focus is bold, so a focused border still stands out.
        assert!(
            role(Role::Focus, Mode::Mono)
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    fn mode_amber(m: Mode) -> Color {
        m.palette().unwrap().warning
    }

    /// The six roles of `docs/ui-redesign.md` section 4, pinned in each palette and
    /// without colour: (role, dark, light, mono modifiers).
    #[test]
    fn the_six_roles_have_the_colours_of_the_brief() {
        let rgb = |r, g, b| Some(Color::Rgb(r, g, b));
        let table = [
            (
                Role::Focus,
                rgb(0x6c, 0xb6, 0xff),
                rgb(0x0b, 0x5c, 0xad),
                Modifier::BOLD,
            ),
            (
                Role::Ph,
                rgb(0xb7, 0x9c, 0xf5),
                rgb(0x6f, 0x42, 0xc1),
                Modifier::empty(),
            ),
            (
                Role::Warning,
                rgb(0xe8, 0xb4, 0x5c),
                rgb(0x8f, 0x5e, 0x00),
                Modifier::empty(),
            ),
            (
                Role::Danger,
                rgb(0xf0, 0x76, 0x62),
                rgb(0xc0, 0x39, 0x2b),
                Modifier::BOLD,
            ),
            (
                Role::Ok,
                rgb(0x5f, 0xc6, 0xb4),
                rgb(0x0f, 0x76, 0x6e),
                Modifier::empty(),
            ),
            (
                Role::Data,
                rgb(0x4f, 0x8f, 0xd6),
                rgb(0x2e, 0x6d, 0xb5),
                Modifier::empty(),
            ),
        ];
        for (r, dark, light, mono) in table {
            assert_eq!(role(r, Mode::Dark), Style::new().fg(dark.unwrap()), "{r:?}");
            assert_eq!(
                role(r, Mode::Light),
                Style::new().fg(light.unwrap()),
                "{r:?}"
            );
            assert_eq!(
                role(r, Mode::Mono),
                Style::new().add_modifier(mono),
                "{r:?}"
            );
        }
        // The helpers are the roles.
        assert_eq!(warning(), role(Role::Warning, Mode::Dark));
        assert_eq!(danger(), role(Role::Danger, Mode::Dark));
        assert_eq!(data(), role(Role::Data, Mode::Dark));
        // Six different colours in each palette: no role borrows another's.
        for m in [Mode::Dark, Mode::Light] {
            let fgs: Vec<_> = table.iter().map(|t| role(t.0, m).fg).collect();
            for (i, a) in fgs.iter().enumerate() {
                assert!(!fgs[i + 1..].contains(a), "{m:?}: {a:?} twice");
            }
        }
        // PROD's badge is the danger colour, STAGE's the warning colour.
        for m in [Mode::Dark, Mode::Light] {
            let p = m.palette().unwrap();
            assert_eq!(env_badge_in(Env::Prod, m).bg, Some(p.danger));
            assert_eq!(env_badge_in(Env::Stage, m).bg, Some(p.warning));
        }
    }

    /// Warning, danger and data read on the page and on the selected row, in both
    /// palettes. Text needs 4.5:1 on the page and 4:1 on the selection, as the other
    /// accents; data is drawn as bars, a graphic, so 3:1 (WCAG 1.4.11) on the selection.
    #[test]
    fn warning_danger_and_data_are_readable_in_both_palettes() {
        for p in [DARK_PALETTE, LIGHT_PALETTE] {
            for (name, c, on_sel) in [
                ("warning", p.warning, 4.0),
                ("danger", p.danger, 4.0),
                ("data", p.data, 3.0),
            ] {
                // `bg` is the page's colour in both palettes.
                assert!(contrast(c, p.bg) >= 4.5, "{name} on the page: {p:?}");
                assert!(
                    contrast(c, p.sel) >= on_sel,
                    "{name} on the selection: {p:?}"
                );
            }
        }
    }

    #[test]
    fn light_has_every_role_with_its_own_colours() {
        for r in ROLES {
            let (dark, light) = (role(r, Mode::Dark), role(r, Mode::Light));
            // Same shape: the same parts are coloured, the same modifiers.
            assert_eq!(dark.fg.is_some(), light.fg.is_some(), "{r:?}");
            assert_eq!(dark.bg.is_some(), light.bg.is_some(), "{r:?}");
            assert_eq!(dark.add_modifier, light.add_modifier, "{r:?}");
            assert_ne!(dark, light, "{r:?} is the same in both palettes");
        }
        for e in ENVS {
            let (dark, light) = (env_badge_in(e, Mode::Dark), env_badge_in(e, Mode::Light));
            assert_ne!(dark, light, "{e:?}");
            assert!(light.bg.is_some() && light.fg.is_some());
        }
        // Text on a filled badge is not the text colour of the page.
        assert_ne!(
            env_badge_in(Env::Stage, Mode::Light).fg,
            env_badge_in(Env::Stage, Mode::Light).bg
        );
    }

    /// WCAG contrast of two `Rgb` colours.
    fn contrast(a: Color, b: Color) -> f64 {
        fn lum(c: Color) -> f64 {
            let Color::Rgb(r, g, b) = c else {
                panic!("not rgb")
            };
            let f = |v: u8| {
                let v = f64::from(v) / 255.0;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
        }
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn the_light_palette_is_readable_on_white() {
        let p = LIGHT_PALETTE;
        for (name, c) in [
            ("fg", p.fg),
            ("hi", p.hi),
            ("dim", p.dim),
            ("warning", p.warning),
            ("danger", p.danger),
            ("data", p.data),
            ("teal", p.teal),
            ("focus", p.focus),
            ("ph", p.ph),
        ] {
            assert!(contrast(c, p.bg) >= 4.5, "{name} on white");
        }
        // Text on the selected row, and badges' white text on their fills.
        for c in [p.fg, p.hi, p.warning, p.dim, p.focus, p.ph] {
            assert!(contrast(c, p.sel) >= 4.0, "{c:?} on the selection");
        }
        for fill in [p.danger, p.warning, p.teal] {
            assert!(contrast(p.bg, fill) >= 4.5, "{fill:?}");
        }
        assert!(contrast(p.hi, p.faint) >= 4.5);
    }

    #[test]
    fn no_colour_means_none_anywhere() {
        for r in ROLES {
            let s = role(r, Mode::Mono);
            assert_eq!((s.fg, s.bg), (None, None), "{r:?}");
            assert_eq!(
                (s.underline_color, s.sub_modifier),
                (None, Modifier::empty())
            );
        }
        for e in ENVS {
            let s = env_badge_in(e, Mode::Mono);
            assert_eq!((s.fg, s.bg), (None, None), "{e:?}");
            assert!(s.add_modifier.contains(Modifier::REVERSED | Modifier::BOLD));
        }
    }

    #[test]
    fn no_colour_keeps_selection_badges_and_the_active_tab_apart() {
        let m = Mode::Mono;
        assert!(role(Role::Sel, m).add_modifier.contains(Modifier::REVERSED));
        let on = role(Role::TabOn, m).add_modifier;
        assert!(on.contains(Modifier::REVERSED) && on.contains(Modifier::BOLD));
        // An inactive tab is neither reversed nor bold.
        let off = role(Role::Tab, m).add_modifier;
        assert!(!off.contains(Modifier::REVERSED) && !off.contains(Modifier::BOLD));
        // The brand and key caps stand out too; plain text does not.
        assert!(
            role(Role::Brand, m)
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(role(Role::Key, m).add_modifier.contains(Modifier::REVERSED));
        assert_eq!(role(Role::Fg, m), Style::new());
        // Errors are bold, and their words (✗ failed) are the UI's own.
        assert!(role(Role::Danger, m).add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn the_mode_follows_the_setting_and_no_color() {
        assert_eq!(Mode::resolve(Theme::Dark, false), Mode::Dark);
        assert_eq!(Mode::resolve(Theme::Light, false), Mode::Light);
        assert_eq!(Mode::resolve(Theme::Dark, true), Mode::Mono);
        assert_eq!(Mode::resolve(Theme::Light, true), Mode::Mono);
        use std::ffi::OsStr;
        assert!(!no_color_value(None));
        assert!(!no_color_value(Some(OsStr::new(""))));
        assert!(no_color_value(Some(OsStr::new("1"))));
        assert!(no_color_value(Some(OsStr::new("anything"))));
        for m in [Mode::Dark, Mode::Light, Mode::Mono] {
            assert_eq!(Mode::from_u8(m.to_u8()), m);
        }
    }
}
