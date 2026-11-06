//! Colours, taken from the current Omarchy theme when there is one and reloaded when it changes.

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone)]
pub struct Theme {
    pub accent: Color,
    /// The sun highlight: cursor, focused labels, status line, stars.
    pub sun: Color,
    pub muted: Color,
    pub fg: Color,
    pub bright: Color,
    /// Text on top of coloured chips (avatars, the Reply pill).
    pub on_chip: Color,
    pub selection: Color,
    pub popup_bg: Color,
    pub rule: Color,
    pub error: Color,
    pub avatars: [Color; 6],
    /// Where it came from and when, so `refresh` can pick up a theme switch.
    source: Option<(PathBuf, SystemTime)>,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            accent: Color::Rgb(59, 91, 255),
            sun: Color::Rgb(255, 200, 40),
            muted: Color::Rgb(142, 142, 153),
            fg: Color::Reset,
            bright: Color::White,
            on_chip: Color::Black,
            selection: Color::Rgb(38, 40, 52),
            popup_bg: Color::Rgb(24, 25, 32),
            rule: Color::Rgb(60, 60, 66),
            error: Color::Rgb(239, 108, 87),
            avatars: [
                Color::Rgb(239, 108, 87),
                Color::Rgb(94, 172, 112),
                Color::Rgb(91, 130, 230),
                Color::Rgb(201, 120, 220),
                Color::Rgb(232, 168, 56),
                Color::Rgb(70, 180, 190),
            ],
            source: None,
        }
    }
}

fn colors_path() -> Option<PathBuf> {
    let p = dirs::home_dir()?.join(".local/state/omarchy/current/theme/colors.toml");
    p.exists().then_some(p)
}

fn hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

impl Theme {
    pub fn load() -> Theme {
        Self::from_omarchy().unwrap_or_default()
    }

    fn from_omarchy() -> Option<Theme> {
        let path = colors_path()?;
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
        let raw: HashMap<String, toml::Value> = toml::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
        let d = Theme::default();
        let c = |k: &str, fallback: Color| raw.get(k).and_then(|v| v.as_str()).and_then(hex).unwrap_or(fallback);
        let bg = c("background", Color::Black);
        Some(Theme {
            accent: c("accent", d.accent),
            sun: c("yellow", d.sun),
            muted: c("dark_foreground", d.muted),
            fg: c("foreground", d.fg),
            bright: c("bright_foreground", d.bright),
            on_chip: bg,
            selection: c("selection", d.selection),
            popup_bg: c("lighter_background", d.popup_bg),
            rule: c("muted", d.rule),
            error: c("red", d.error),
            avatars: [
                c("red", d.avatars[0]),
                c("green", d.avatars[1]),
                c("blue", d.avatars[2]),
                c("magenta", d.avatars[3]),
                c("orange", d.avatars[4]),
                c("cyan", d.avatars[5]),
            ],
            source: Some((path, mtime)),
        })
    }

    /// Reload if the Omarchy theme changed on disk. Returns true when it did.
    pub fn refresh(&mut self) -> bool {
        let now = colors_path().and_then(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (p, t)));
        if now == self.source {
            return false;
        }
        *self = Self::load();
        true
    }

    pub fn avatar(&self, email: &str) -> Color {
        self.avatars[email.bytes().fold(0usize, |a, b| a.wrapping_add(b as usize)) % self.avatars.len()]
    }

    pub fn popup<'a>(&self, title: &'a str) -> Block<'a> {
        Block::bordered()
            .style(Style::new().bg(self.popup_bg).fg(self.fg))
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(self.sun))
            .title(Span::styled(title, Style::new().bold()))
    }
}
