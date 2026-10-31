//! discada's shared TUI style (the `[style]` of each look, written to rola/look.json)
//! mapped onto ratatui. Colors come from the kitty theme kitty.conf includes right now
//! (wallust's, or the look's), the same way rola reads them.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders};
use serde::Deserialize;

use picha::config::config_dir;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Look {
    pub borders: String,
    pub bar: String,
    pub labels: String,
    pub case: String,
    pub select: String,
    pub tint: String,
    pub transparent: bool,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            borders: "auto".into(),
            bar: "line".into(),
            labels: "icons".into(),
            case: "normal".into(),
            select: "fill".into(),
            tint: "color".into(),
            transparent: true,
        }
    }
}

impl Look {
    /// picha's own look.json, else discada's shared one, else defaults.
    pub fn load() -> Self {
        let dir = config_dir();
        ["picha/look.json", "rola/look.json"]
            .iter()
            .find_map(|rel| fs::read_to_string(dir.join(rel)).ok().and_then(|s| serde_json::from_str(&s).ok()))
            .unwrap_or_default()
    }
}

/// The theme kitty.conf includes now (its last `include` that sets a background),
/// or kitty.conf itself when the colors live there.
fn kitty_theme() -> PathBuf {
    let dir = config_dir().join("kitty");
    let conf = dir.join("kitty.conf");
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    fs::read_to_string(&conf)
        .ok()
        .and_then(|text| {
            text.lines()
                .filter_map(|l| l.trim().strip_prefix("include"))
                .map(|p| match p.trim().strip_prefix("~/") {
                    Some(rest) => home.join(rest),
                    None => dir.join(p.trim()),
                })
                .rev()
                .find(|p| fs::read_to_string(p).is_ok_and(|t| t.lines().any(|l| l.starts_with("background"))))
        })
        .unwrap_or(conf)
}

/// Modification times of everything the theme is built from; a change means restyle.
pub fn stamp() -> Vec<Option<SystemTime>> {
    let dir = config_dir();
    ["picha/look.json", "rola/look.json", "empalme/colors.json"]
        .iter()
        .map(|rel| dir.join(rel))
        .chain([kitty_theme()])
        .map(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
        .collect()
}

type Rgb = (u8, u8, u8);

fn hex(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#')?;
    let v = u32::from_str_radix(h, 16).ok().filter(|_| h.len() == 6)?;
    Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

fn rgb((r, g, b): Rgb) -> Color {
    Color::Rgb(r, g, b)
}

/// `t` of `a` plus `1 - t` of `b`.
fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |x: u8, y: u8| (x as f32 * t + y as f32 * (1.0 - t)).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

fn luma((r, g, b): Rgb) -> f32 {
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
}

/// `background`, `foreground`, `color0`…`color15` of a kitty theme file.
fn kitty_colors() -> HashMap<String, Rgb> {
    fs::read_to_string(kitty_theme())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), hex(it.next()?)?))
        })
        .collect()
}

/// `critical` and `alert` from discada's palette file.
fn discada_colors() -> HashMap<String, String> {
    fs::read_to_string(config_dir().join("empalme/colors.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub struct Theme {
    pub look: Look,
    pub fg: Style,
    pub dim: Style,
    /// Unfocused borders, fainter than dim text.
    pub border: Style,
    pub acc: Style,
    pub acc2: Style,
    /// The active option in a row: the primary color, or bold text when the palette's
    /// primary would read dimmer than the inactive options (monochrome wallust looks).
    pub current: Style,
    pub warn: Style,
    bg: Color,
}

impl Theme {
    pub fn new(look: Look) -> Self {
        let k = kitty_colors();
        let d = discada_colors();
        let named = |key: &str| d.get(key).and_then(|s| hex(s)).map(rgb);
        let bold = Modifier::BOLD;
        let (Some(&bg), Some(&fg)) = (k.get("background"), k.get("foreground")) else {
            // No kitty theme: the terminal's own palette, by index.
            return Self {
                fg: Style::new(),
                dim: Style::new().fg(Color::Indexed(8)),
                border: Style::new().fg(Color::Indexed(8)),
                acc: Style::new().fg(Color::Indexed(12)).add_modifier(bold),
                acc2: Style::new().fg(Color::Indexed(13)),
                current: Style::new().add_modifier(bold | Modifier::UNDERLINED),
                warn: Style::new().fg(named("alert").unwrap_or(Color::Yellow)),
                bg: Color::Reset,
                look,
            };
        };
        let primary = k.get("color12").copied().unwrap_or(fg);
        let accent = k.get("color11").copied().unwrap_or(primary);
        let (fg, primary) = match look.tint.as_str() {
            // a phosphor monitor: the text in the primary color too
            "phosphor" => (primary, primary),
            // paper and ink: greys, color only for warnings
            "ink" => (fg, fg),
            _ => (fg, primary),
        };
        let muted = mix(fg, bg, 0.55);
        let stands_out = (luma(primary) - luma(bg)).abs() > (luma(muted) - luma(bg)).abs() + 0.08;
        Self {
            fg: Style::new().fg(rgb(fg)),
            dim: Style::new().fg(rgb(muted)),
            border: Style::new().fg(rgb(mix(fg, bg, 0.3))),
            acc: Style::new().fg(rgb(primary)).add_modifier(bold),
            acc2: Style::new().fg(rgb(accent)),
            current: Style::new().fg(rgb(if stands_out { primary } else { fg })).add_modifier(bold),
            warn: Style::new().fg(named("alert").unwrap_or(rgb(accent))),
            bg: rgb(bg),
            look,
        }
    }

    pub fn background(&self) -> Option<Style> {
        (!self.look.transparent).then(|| Style::new().bg(self.bg))
    }

    pub fn case(&self, s: &str) -> String {
        match self.look.case.as_str() {
            "upper" => s.to_uppercase(),
            "lower" => s.to_lowercase(),
            _ => s.to_string(),
        }
    }

    pub fn label(&self, icon: &str, short: &str, word: &str) -> String {
        self.case(&match self.look.labels.as_str() {
            "short" => short.to_string(),
            "words" => word.to_string(),
            _ => format!("{icon} {word}"),
        })
    }

    pub fn block(&self, title: String, focused: bool) -> Block<'static> {
        let border_type = match self.look.borders.as_str() {
            "plain" => BorderType::Plain,
            "double" => BorderType::Double,
            "thick" => BorderType::Thick,
            "dashed" => BorderType::LightDoubleDashed,
            _ => BorderType::Rounded,
        };
        let borders = if self.look.borders == "none" { Borders::NONE } else { Borders::ALL };
        let edge = if focused { self.acc } else { self.border };
        Block::new()
            .borders(borders)
            .border_type(border_type)
            .border_style(edge.remove_modifier(Modifier::BOLD))
            .title(Span::styled(format!(" {title} "), if focused { self.acc } else { self.fg }))
    }

    /// (filled, empty, knob) glyphs for the `bar` style.
    fn glyphs(&self) -> (&'static str, &'static str, &'static str) {
        match self.look.bar.as_str() {
            "knob" => ("━", "─", "●"),
            "blocks" => ("█", "░", "▓"),
            "dots" => ("•", "·", "●"),
            "ascii" => ("#", "-", "|"),
            _ => ("━", "─", "━"),
        }
    }

    pub fn bar(&self, ratio: f64, width: u16, style: Style) -> Vec<Span<'static>> {
        let (fill, empty, knob) = self.glyphs();
        let w = width as usize;
        let n = ((ratio.clamp(0.0, 1.0) * w as f64).round() as usize).min(w);
        let mut spans = Vec::new();
        if self.look.bar == "ascii" {
            spans.push(Span::styled("[", self.dim));
        }
        if n > 0 {
            spans.push(Span::styled(fill.repeat(n - 1), style));
            spans.push(Span::styled(if n < w { knob } else { fill }, style));
        }
        spans.push(Span::styled(empty.repeat(w - n), self.dim));
        if self.look.bar == "ascii" {
            spans.push(Span::styled("]", self.dim));
        }
        spans
    }

    /// One choice in a row of options: `current` is the hardware's state, `focused` the cursor row.
    pub fn choice(&self, text: &str, current: bool, focused: bool) -> Vec<Span<'static>> {
        let text = self.case(text);
        if !current {
            return vec![Span::styled(format!(" {text} "), self.dim)];
        }
        if !focused {
            return vec![Span::styled(format!(" {text} "), self.current)];
        }
        match self.look.select.as_str() {
            "bar" => vec![Span::styled("▌", self.acc2), Span::styled(format!("{text} "), self.current)],
            "inverse" => vec![Span::styled(format!(" {text} "), self.current.add_modifier(Modifier::REVERSED))],
            _ => {
                let fill = self.current.fg.unwrap_or(Color::Reset);
                if fill == Color::Reset {
                    vec![Span::styled(format!(" {text} "), self.current.add_modifier(Modifier::REVERSED))]
                } else {
                    vec![Span::styled(format!(" {text} "), Style::new().bg(fill).fg(self.bg).add_modifier(Modifier::BOLD))]
                }
            }
        }
    }
}
