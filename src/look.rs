//! Looking like the rest of discada in a plain terminal: the colors of the kitty
//! theme kitty.conf includes (wallust's, or the look's) and the borders and case of
//! the look's [style] (rola/look.json), the way rola and fierro read them.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use unicode_width::UnicodeWidthStr;

use crate::config::config_dir;

type Rgb = (u8, u8, u8);

fn hex(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#')?;
    let v = u32::from_str_radix(h, 16).ok().filter(|_| h.len() == 6)?;
    Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |x: u8, y: u8| (x as f32 * t + y as f32 * (1.0 - t)).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// The theme kitty.conf includes now (its last `include` setting a background).
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

#[derive(Clone, Copy)]
pub enum Tone {
    Fg,
    Muted,
    Primary,
    Accent,
    Warn,
}

pub struct Look {
    colors: Option<[Rgb; 5]>,
    borders: String,
    case: String,
}

impl Look {
    pub fn load() -> Self {
        let k: HashMap<String, Rgb> = fs::read_to_string(kitty_theme())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some((it.next()?.to_string(), hex(it.next()?)?))
            })
            .collect();
        let style: serde_json::Value = ["picha/look.json", "rola/look.json"]
            .iter()
            .find_map(|rel| fs::read_to_string(config_dir().join(rel)).ok().and_then(|s| serde_json::from_str(&s).ok()))
            .unwrap_or_default();
        let get = |key: &str| style[key].as_str().unwrap_or("").to_string();
        let colors = k.get("background").zip(k.get("foreground")).map(|(&bg, &fg)| {
            let primary = k.get("color12").copied().unwrap_or(fg);
            let (fg, primary) = match get("tint").as_str() {
                "phosphor" => (primary, primary),
                "ink" => (fg, fg),
                _ => (fg, primary),
            };
            let accent = k.get("color11").copied().unwrap_or(primary);
            let warn = k.get("color1").copied().unwrap_or(accent);
            [fg, mix(fg, bg, 0.55), primary, accent, warn]
        });
        Self { colors, borders: get("borders"), case: get("case") }
    }

    pub fn paint(&self, tone: Tone, text: &str) -> String {
        let text = self.case(text);
        match self.colors {
            Some(c) => {
                let (r, g, b) = c[tone as usize];
                let bold = if matches!(tone, Tone::Primary) { "1;" } else { "" };
                format!("\x1b[{bold}38;2;{r};{g};{b}m{text}\x1b[0m")
            }
            None => text,
        }
    }

    pub fn case(&self, s: &str) -> String {
        match self.case.as_str() {
            "upper" => s.to_uppercase(),
            "lower" => s.to_lowercase(),
            _ => s.to_string(),
        }
    }

    /// (top-left, top-right, bottom-left, bottom-right, horizontal, vertical)
    fn corners(&self) -> Option<[&'static str; 6]> {
        match self.borders.as_str() {
            "none" => None,
            "plain" => Some(["┌", "┐", "└", "┘", "─", "│"]),
            "thick" => Some(["┏", "┓", "┗", "┛", "━", "┃"]),
            "double" => Some(["╔", "╗", "╚", "╝", "═", "║"]),
            "dashed" => Some(["┌", "┐", "└", "┘", "╌", "╎"]),
            _ => Some(["╭", "╮", "╰", "╯", "─", "│"]),
        }
    }

    /// A box titled `title` around `rows`, each row a list of (tone, text) pieces.
    pub fn card(&self, title: &str, rows: &[Vec<(Tone, String)>]) -> String {
        let width = |r: &Vec<(Tone, String)>| r.iter().map(|(_, t)| UnicodeWidthStr::width(self.case(t).as_str())).sum::<usize>();
        let inner = rows.iter().map(width).max().unwrap_or(0).max(UnicodeWidthStr::width(title) + 2);
        let mut out = String::new();
        let Some([tl, tr, bl, br, h, v]) = self.corners() else {
            out += &format!(" {}\n", self.paint(Tone::Primary, title));
            for r in rows {
                out += " ";
                out += &r.iter().map(|(tone, t)| self.paint(*tone, t)).collect::<String>();
                out += "\n";
            }
            return out;
        };
        let line = |s: &str| self.paint(Tone::Muted, s);
        let title_w = UnicodeWidthStr::width(title) + 2;
        out += &format!(
            "{}{}{}\n",
            line(&format!("{tl}{h} ")),
            self.paint(Tone::Primary, title),
            line(&format!(" {}{tr}", h.repeat(inner + 1 - title_w)))
        );
        for r in rows {
            let pad = inner - width(r);
            out += &format!(
                "{} {}{} {}\n",
                line(v),
                r.iter().map(|(tone, t)| self.paint(*tone, t)).collect::<String>(),
                " ".repeat(pad),
                line(v)
            );
        }
        out += &line(&format!("{bl}{}{br}", h.repeat(inner + 2)));
        out += "\n";
        out
    }
}
