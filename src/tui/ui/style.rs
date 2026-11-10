//! Colours from the design mockup.

use ratatui::style::{Color, Modifier, Style};

use crate::model::Env;

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

pub fn fg() -> Style {
    Style::new().fg(FG)
}
pub fn fgb() -> Style {
    Style::new().fg(HI).add_modifier(Modifier::BOLD)
}
pub fn dim() -> Style {
    Style::new().fg(DIM)
}
/// Column headings.
pub fn hd() -> Style {
    Style::new().fg(DIM).add_modifier(Modifier::BOLD)
}
pub fn warn() -> Style {
    Style::new().fg(RED)
}
pub fn ok() -> Style {
    Style::new().fg(TEAL)
}
pub fn amber() -> Style {
    Style::new().fg(AMBER)
}
/// `{placeholders}` in patterns.
pub fn ph() -> Style {
    Style::new().fg(BLUE)
}
pub fn phb() -> Style {
    Style::new().fg(BLUE).add_modifier(Modifier::BOLD)
}
pub fn track() -> Style {
    Style::new().fg(FAINT)
}
pub fn cmd() -> Style {
    Style::new().fg(HI)
}
pub fn sel() -> Style {
    Style::new().bg(SEL)
}
pub fn border() -> Style {
    Style::new().fg(BORDER)
}
pub fn title() -> Style {
    Style::new().fg(AMBER).add_modifier(Modifier::BOLD)
}
pub fn brand() -> Style {
    Style::new().bg(AMBER).fg(BG).add_modifier(Modifier::BOLD)
}
pub fn tab() -> Style {
    Style::new().fg(DIM)
}
pub fn tab_on() -> Style {
    Style::new().bg(SEL).fg(AMBER).add_modifier(Modifier::BOLD)
}
/// Key caps in the key bar.
pub fn key() -> Style {
    Style::new().bg(FAINT).fg(HI)
}

/// PROD red, STAGE amber, LOCAL teal, OTHER grey.
pub fn env_badge(env: Env) -> Style {
    let s = Style::new().add_modifier(Modifier::BOLD);
    match env {
        Env::Prod => s.bg(RED).fg(BG),
        Env::Stage => s.bg(AMBER).fg(BG),
        Env::Local => s.bg(TEAL).fg(BG),
        Env::Other => s.bg(FAINT).fg(HI),
    }
}
