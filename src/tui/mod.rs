//! `picha pantalla`: a small screen in discada's look (borders, bars, labels, case,
//! selection, tint, all from the look's [style]) that finds the phone, shows it, and
//! opens its screen when the user picks "open". Restyles itself when the look changes.

mod theme;

use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use picha::pantalla::{self, Info, Link};

use theme::{Look, Theme};

#[derive(Clone, Copy)]
pub struct Lang(pub bool);

impl Lang {
    fn t(self, es: &'static str, en: &'static str) -> &'static str {
        if self.0 { es } else { en }
    }
}

enum Stage {
    Searching(Instant),
    Ready(Link, Info),
    /// None: no phone anywhere; Some: what adb said.
    Failed(Option<String>),
    Opening(Instant),
}

#[derive(Clone, Copy, PartialEq)]
enum Item {
    Open,
    Audio,
    PhoneScreen,
    Retry,
    Quit,
}

struct App {
    lang: Lang,
    theme: Theme,
    stamp: Vec<Option<SystemTime>>,
    restyled: bool,
    stage: Stage,
    sel: usize,
    audio: bool,
    screen_off: bool,
    addr: Option<String>,
    found: mpsc::Receiver<std::result::Result<(Link, Info), Option<String>>>,
    found_tx: mpsc::Sender<std::result::Result<(Link, Info), Option<String>>>,
    /// Where each menu row was drawn, for clicks.
    rows: Vec<(Rect, Item)>,
    quit: bool,
}

impl App {
    fn items(&self) -> Vec<Item> {
        match self.stage {
            Stage::Ready(..) => vec![Item::Open, Item::Audio, Item::PhoneScreen, Item::Quit],
            Stage::Failed(_) => vec![Item::Retry, Item::Quit],
            _ => vec![Item::Quit],
        }
    }

    fn search(&mut self) {
        self.stage = Stage::Searching(Instant::now());
        self.sel = 0;
        let (tx, addr) = (self.found_tx.clone(), self.addr.clone());
        tokio::spawn(async move {
            let found = pantalla::reach(addr).await;
            let found = match found {
                Ok(link) => {
                    let serial = link.serial.clone();
                    let info = tokio::task::spawn_blocking(move || pantalla::info(&serial)).await.unwrap_or_default();
                    Ok((link, info))
                }
                Err(e) => Err(e),
            };
            let _ = tx.send(found);
        });
    }

    fn choose(&mut self, item: Item) {
        match item {
            Item::Open => {
                if let Stage::Ready(link, info) = &self.stage {
                    if pantalla::open(&link.serial, &info.name, self.audio, self.screen_off) {
                        self.stage = Stage::Opening(Instant::now());
                    } else {
                        self.stage = Stage::Failed(Some("scrcpy (sudo pacman -S scrcpy)".into()));
                    }
                }
            }
            Item::Audio => self.audio = !self.audio,
            Item::PhoneScreen => self.screen_off = !self.screen_off,
            Item::Retry => self.search(),
            Item::Quit => self.quit = true,
        }
    }

    fn key(&mut self, code: KeyCode) {
        let n = self.items().len();
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => self.sel = (self.sel + n - 1) % n,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => self.sel = (self.sel + 1) % n,
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(&item) = self.items().get(self.sel) {
                    self.choose(item);
                }
            }
            KeyCode::Char('a') if matches!(self.stage, Stage::Ready(..)) => self.choose(Item::Audio),
            KeyCode::Char('o') if matches!(self.stage, Stage::Ready(..)) => self.choose(Item::PhoneScreen),
            KeyCode::Char('r') if matches!(self.stage, Stage::Failed(_)) => self.choose(Item::Retry),
            _ => {}
        }
    }

    fn click(&mut self, x: u16, y: u16) {
        let hit = self.rows.iter().position(|(r, _)| r.contains((x, y).into()));
        if let Some(i) = hit {
            self.sel = i;
            let item = self.rows[i].1;
            self.choose(item);
        }
    }

    fn tick(&mut self) {
        if let Ok(found) = self.found.try_recv() {
            self.stage = match found {
                Ok((link, info)) => Stage::Ready(link, info),
                Err(why) => Stage::Failed(why),
            };
            self.sel = 0;
        }
        if let Stage::Opening(at) = self.stage
            && at.elapsed() > Duration::from_millis(900)
        {
            self.quit = true;
        }
        let stamp = theme::stamp();
        if stamp != self.stamp {
            self.stamp = stamp;
            self.theme = Theme::new(Look::load());
            self.restyled = true;
        }
    }
}

pub async fn run(lang: Lang, addr: Option<String>, audio: bool, screen_off: bool) -> io::Result<()> {
    let (found_tx, found) = mpsc::channel();
    let mut app = App {
        lang,
        theme: Theme::new(Look::load()),
        stamp: theme::stamp(),
        restyled: false,
        stage: Stage::Searching(Instant::now()),
        sel: 0,
        audio,
        screen_off,
        addr,
        found,
        found_tx,
        rows: Vec::new(),
        quit: false,
    };
    app.search();
    tokio::task::block_in_place(|| {
        let mut terminal = ratatui::init();
        let _ = execute!(io::stdout(), EnableMouseCapture);
        let result = (|| -> io::Result<()> {
            while !app.quit {
                if std::mem::take(&mut app.restyled) {
                    terminal.clear()?;
                }
                terminal.draw(|f| draw(f, &mut app))?;
                if event::poll(Duration::from_millis(100))? {
                    match event::read()? {
                        Event::Key(k) if k.kind == KeyEventKind::Press => app.key(k.code),
                        Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => app.click(m.column, m.row),
                        _ => {}
                    }
                }
                app.tick();
            }
            Ok(())
        })();
        let _ = execute!(io::stdout(), DisableMouseCapture);
        ratatui::restore();
        result
    })
}

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner(th: &Theme, since: Instant) -> &'static str {
    let i = (since.elapsed().as_millis() / 90) as usize;
    if th.look.bar == "ascii" { ["|", "/", "-", "\\"][i % 4] } else { SPIN[i % SPIN.len()] }
}

/// An icon in front of a line when the look shows icons.
fn icon(th: &Theme, glyph: &str) -> String {
    if th.look.labels == "icons" { format!("{glyph}  ") } else { String::new() }
}

fn draw(f: &mut Frame, app: &mut App) {
    let th = &app.theme;
    let lang = app.lang;
    if let Some(bg) = th.background() {
        f.render_widget(Block::new().style(bg), f.area());
    }
    let area = f.area().inner(Margin { horizontal: 1, vertical: 0 });
    let [body, footer] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    let block = th.block(th.label("󰄜", "cel", lang.t("pantalla del cel", "phone screen")), true);
    let inner = block.inner(body).inner(Margin { horizontal: 1, vertical: 0 });
    f.render_widget(block, body);

    let mut lines: Vec<Line> = Vec::new();
    match &app.stage {
        Stage::Searching(since) => {
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", spinner(th, *since)), th.acc),
                Span::styled(th.case(lang.t("buscando el cel…", "looking for the phone…")), th.fg),
            ]));
            lines.push(Line::default());
            lines.push(Line::styled(th.case(lang.t("por Wi-Fi o por cable", "over Wi-Fi or the cable")), th.dim));
        }
        Stage::Failed(why) => {
            match why {
                None => {
                    lines.push(Line::styled(th.case(lang.t("no encontré el cel", "no phone found")), th.warn));
                    lines.push(Line::default());
                    lines.push(Line::styled(th.case(lang.t("prende Depuración inalámbrica", "turn on Wireless debugging")), th.fg));
                    lines.push(Line::styled(
                        th.case(lang.t("  (búscala en Ajustes del cel)", "  (search for it in the phone's Settings)")),
                        th.dim,
                    ));
                    lines.push(Line::styled(th.case(lang.t("o conecta el cable", "or plug in the cable")), th.fg));
                }
                Some(why) => {
                    lines.push(Line::styled(th.case(lang.t("algo falló", "something failed")), th.warn));
                    lines.push(Line::styled(why.clone(), th.dim));
                }
            }
            lines.push(Line::default());
        }
        Stage::Ready(link, info) => {
            let mut name = vec![Span::styled(format!("{}{}", icon(th, "󰄜"), info.name), th.current)];
            name.push(Span::styled(format!("  {}", info.model), th.dim));
            lines.push(Line::from(name));
            let mut system = format!("Android {}", info.android);
            if let Some(v) = &info.one_ui {
                system += &format!(" · One UI {v}");
            }
            lines.push(Line::styled(th.case(&system), th.dim));
            lines.push(Line::default());
            if let Some(level) = info.battery {
                let tone = if level <= 15 { th.warn } else { th.acc };
                let mut row = vec![Span::styled(icon(th, "󰁹"), th.fg)];
                row.extend(th.bar(level as f64 / 100.0, 14, tone.remove_modifier(Modifier::BOLD)));
                row.push(Span::styled(format!(" {level}%"), th.fg));
                if info.charging {
                    row.push(Span::styled(th.case(&format!("  {}", lang.t("cargando", "charging"))), th.acc2));
                }
                if let Some(t) = info.temperature {
                    row.push(Span::styled(format!("  {t:.0}°C"), if t >= 40.0 { th.warn } else { th.dim }));
                }
                lines.push(Line::from(row));
            }
            let link_line = match (link.cable, &link.wifi) {
                (true, Some(w)) => vec![
                    Span::styled(format!("{}{}", icon(th, "󰕓"), th.case(lang.t("cable", "cable"))), th.fg),
                    Span::styled(th.case(&format!("  · {} {w}", lang.t("Wi-Fi lista en", "Wi-Fi ready at"))), th.dim),
                ],
                (true, None) => vec![Span::styled(format!("{}{}", icon(th, "󰕓"), th.case(lang.t("cable", "cable"))), th.fg)],
                (false, w) => vec![
                    Span::styled(format!("{}Wi-Fi {}", icon(th, "󰖩"), w.as_deref().unwrap_or(&link.serial)), th.fg),
                    Span::styled(th.case(&format!("  · {}", lang.t("cifrado", "encrypted"))), th.dim),
                ],
            };
            lines.push(Line::from(link_line));
            if let Some(free) = &info.free {
                lines.push(Line::styled(format!("{}{free} {}", icon(th, "󰋊"), th.case(lang.t("libres", "free"))), th.fg));
            }
            lines.push(Line::default());
        }
        Stage::Opening(since) => {
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", spinner(th, *since)), th.acc),
                Span::styled(th.case(lang.t("abriendo la pantalla…", "opening the screen…")), th.fg),
            ]));
        }
    }
    let info_height = lines.len() as u16;
    let [top, menu] = Layout::vertical([Constraint::Length(info_height), Constraint::Fill(1)]).areas(inner);
    f.render_widget(Paragraph::new(lines), top);

    // the menu: the selected row in the look's select style
    app.rows.clear();
    if matches!(app.stage, Stage::Ready(..) | Stage::Failed(_)) {
        let on_off = |on: bool| lang.t(if on { "sí" } else { "no" }, if on { "on" } else { "off" });
        for (i, item) in app.items().into_iter().enumerate() {
            let (glyph, text) = match item {
                Item::Open => ("󰹑", lang.t("abrir la pantalla", "open the screen").to_string()),
                Item::Audio => ("󰕾", format!("{}  {}", lang.t("sonido aquí", "sound here"), on_off(app.audio))),
                Item::PhoneScreen => (
                    "󰄜",
                    format!(
                        "{}  {}",
                        lang.t("pantalla del cel", "phone's own screen"),
                        if app.screen_off { lang.t("apagada", "off") } else { lang.t("prendida", "on") }
                    ),
                ),
                Item::Retry => ("󰑐", lang.t("buscar otra vez", "look again").to_string()),
                Item::Quit => ("󰅖", lang.t("salir", "quit").to_string()),
            };
            let y = menu.y + i as u16;
            if y >= menu.bottom() {
                break;
            }
            let rect = Rect::new(menu.x, y, menu.width, 1);
            let spans = th.choice(&format!("{}{text}", icon(th, glyph)), i == app.sel, true);
            f.render_widget(Paragraph::new(Line::from(spans)), rect);
            app.rows.push((rect, item));
        }
    }

    let hints = match app.stage {
        Stage::Ready(..) => lang.t("↑↓ mover · enter elegir · a sonido · o pantalla · q salir", "↑↓ move · enter choose · a sound · o screen · q quit"),
        Stage::Failed(_) => lang.t("r buscar otra vez · q salir", "r look again · q quit"),
        _ => lang.t("q salir", "q quit"),
    };
    f.render_widget(Paragraph::new(Span::styled(th.case(hints), th.dim)), footer);
}
