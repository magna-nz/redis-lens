//! Colours from the design mockup, in three modes: the dark palette (the default), a
//! light one for light terminals (`[settings] theme = "light"`), and none at all when
//! `NO_COLOR` is set and not empty (<https://no-color.org>).
//!
//! The style functions (`dim()`, `warn()` …) read one process-wide mode, set once at TUI
//! start by [`init`]. Each is a thin wrapper over a pure `role(Role, Mode)` that the
//! tests call with an explicit mode, so they never touch the global. Nothing in the UI
//! tells a state by colour alone: every state has its words (`read-only`, `✗ failed`,
//! `PROD`), so the colourless mode only has to keep selection, badges and the active tab
//! apart, with reverse video and bold.

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
pub const AMBER: Color = Color::Rgb(0xe8, 0xb4, 0x5c);
pub const RED: Color = Color::Rgb(0xf0, 0x76, 0x62);
pub const TEAL: Color = Color::Rgb(0x5f, 0xc6, 0xb4);
pub const BLUE: Color = Color::Rgb(0x84, 0xaa, 0xf0);

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
    pub amber: Color,
    pub red: Color,
    pub teal: Color,
    pub blue: Color,
}

pub const DARK_PALETTE: Palette = Palette {
    bg: BG,
    fg: FG,
    hi: HI,
    dim: DIM,
    faint: FAINT,
    border: BORDER,
    sel: SEL,
    amber: AMBER,
    red: RED,
    teal: TEAL,
    blue: BLUE,
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
    amber: Color::Rgb(0x8f, 0x5e, 0x00),
    red: Color::Rgb(0xc0, 0x39, 0x2b),
    teal: Color::Rgb(0x0f, 0x76, 0x6e),
    blue: Color::Rgb(0x2f, 0x5f, 0xc4),
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
    Mode::from_u8(MODE.load(Ordering::Relaxed))
}

/// The roles a style is asked for by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Fg,
    Fgb,
    Dim,
    Hd,
    Warn,
    Ok,
    Amber,
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
}

/// The style of `role` in `mode`. Pure.
pub fn role(role: Role, mode: Mode) -> Style {
    let bold = Modifier::BOLD;
    let Some(p) = mode.palette() else {
        // No colour: weight and reverse video only. A state's words stay on screen.
        let plain = Style::new();
        return match role {
            Role::Fg | Role::Ok | Role::Amber | Role::Ph | Role::Cmd => plain,
            Role::Fgb | Role::Hd | Role::Warn | Role::Phb | Role::Title => plain.add_modifier(bold),
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
        Role::Warn => Style::new().fg(p.red),
        Role::Ok => Style::new().fg(p.teal),
        Role::Amber => Style::new().fg(p.amber),
        Role::Ph => Style::new().fg(p.blue),
        Role::Phb => Style::new().fg(p.blue).add_modifier(bold),
        Role::Track => Style::new().fg(p.faint),
        Role::Cmd => Style::new().fg(p.hi),
        Role::Sel => Style::new().bg(p.sel),
        Role::Border => Style::new().fg(p.border),
        Role::Title => Style::new().fg(p.amber).add_modifier(bold),
        Role::Brand => Style::new().bg(p.amber).fg(p.bg).add_modifier(bold),
        Role::TabOn => Style::new().bg(p.sel).fg(p.amber).add_modifier(bold),
        Role::Key => Style::new().bg(p.faint).fg(p.hi),
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
        Env::Prod => s.bg(p.red).fg(p.bg),
        Env::Stage => s.bg(p.amber).fg(p.bg),
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
pub fn warn() -> Style {
    role(Role::Warn, mode())
}
pub fn ok() -> Style {
    role(Role::Ok, mode())
}
pub fn amber() -> Style {
    role(Role::Amber, mode())
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
pub fn title() -> Style {
    role(Role::Title, mode())
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

/// PROD red, STAGE amber, LOCAL teal, OTHER grey.
pub fn env_badge(env: Env) -> Style {
    env_badge_in(env, mode())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLES: [Role; 18] = [
        Role::Fg,
        Role::Fgb,
        Role::Dim,
        Role::Hd,
        Role::Warn,
        Role::Ok,
        Role::Amber,
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
    ];

    const ENVS: [Env; 4] = [Env::Prod, Env::Stage, Env::Local, Env::Other];

    #[test]
    fn dark_is_the_default_and_matches_the_constants() {
        assert_eq!(Mode::default(), Mode::Dark);
        assert_eq!(mode(), Mode::Dark);
        assert_eq!(role(Role::Dim, Mode::Dark).fg, Some(DIM));
        assert_eq!(role(Role::Warn, Mode::Dark).fg, Some(RED));
        assert_eq!(role(Role::Ok, Mode::Dark).fg, Some(TEAL));
        assert_eq!(role(Role::Amber, Mode::Dark).fg, Some(AMBER));
        assert_eq!(role(Role::Sel, Mode::Dark).bg, Some(SEL));
        assert_eq!(env_badge_in(Env::Prod, Mode::Dark).bg, Some(RED));
        assert_eq!(env_badge_in(Env::Prod, Mode::Dark).fg, Some(BG));
        // The wrappers are the dark roles while nothing sets the mode.
        assert_eq!(dim(), role(Role::Dim, Mode::Dark));
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
            ("amber", p.amber),
            ("red", p.red),
            ("teal", p.teal),
            ("blue", p.blue),
        ] {
            assert!(contrast(c, p.bg) >= 4.5, "{name} on white");
        }
        // Text on the selected row, and badges' white text on their fills.
        for c in [p.fg, p.hi, p.amber, p.dim] {
            assert!(contrast(c, p.sel) >= 4.0, "{c:?} on the selection");
        }
        for fill in [p.red, p.amber, p.teal] {
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
        assert!(role(Role::Warn, m).add_modifier.contains(Modifier::BOLD));
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
