//! `picha`'s screens, in discada's look (borders, bars, labels, case, selection,
//! tint, all from the look's [style], restyled live): the menu (Mandar · Recibir ·
//! Pantalla del cel · Lumbre), the file picker and its send, what arrived, and the
//! phone's screen. `picha foto.jpg` and `picha pantalla` open straight into a page.

mod theme;

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use picha::bt::{self, Phone};
use picha::config::Config;
use picha::lumbre;
use picha::pantalla::{self, Info, Link};
use picha::state::{self, Received};

use theme::{Look, Theme};

#[derive(Clone, Copy)]
pub struct Lang(pub bool);

impl Lang {
    fn t(self, es: &'static str, en: &'static str) -> &'static str {
        if self.0 { es } else { en }
    }
}

/// Where `picha` opens.
pub enum Start {
    Menu,
    /// `picha foto.jpg …`: send these right away.
    Send(Vec<PathBuf>),
    /// `picha pantalla`: the phone's screen page.
    Screen { addr: Option<String>, audio: bool, screen_off: bool },
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Menu,
    Pick,
    Sending,
    Receive,
    Screen,
    Lumbre,
}

const MENU: [Page; 4] = [Page::Pick, Page::Receive, Page::Screen, Page::Lumbre];

enum ScreenStage {
    Searching(Instant),
    Ready(Link, Info),
    Failed(Option<String>),
    Opening(Instant),
}

#[derive(Clone, Copy, PartialEq)]
enum ScreenItem {
    Open,
    Audio,
    PhoneScreen,
    Retry,
    Back,
}

struct Entry {
    path: PathBuf,
    name: String,
    dir: bool,
}

/// How a send is going, from the task doing it.
enum Update {
    /// The route: Quick Share over Wi-Fi (true) or Bluetooth (false), and to whom.
    Via(bool, String),
    /// why it isn't Quick Share
    Why(picha::send::Miss),
    Pin(String),
    Progress(usize, u64, u64),
    Done(Result<(), String>),
}

struct Sending {
    files: Vec<PathBuf>,
    started: Instant,
    /// None while looking for the phone.
    via: Option<(bool, String)>,
    why: Option<picha::send::Miss>,
    pin: Option<String>,
    progress: (usize, u64, u64),
    rx: mpsc::Receiver<Update>,
    result: Option<Result<(), String>>,
}

type Found = std::result::Result<(Link, Info), Option<String>>;

struct App {
    lang: Lang,
    theme: Theme,
    stamp: Vec<Option<SystemTime>>,
    restyled: bool,
    quit: bool,
    page: Page,
    /// The page `picha` opened on: leaving it quits, unless that was the menu.
    first: Page,
    sel: usize,
    phones: Vec<Phone>,
    phones_rx: mpsc::Receiver<Vec<Phone>>,
    phones_tx: mpsc::Sender<Vec<Phone>>,
    phones_at: Instant,
    // the picker
    dir: PathBuf,
    entries: Vec<Entry>,
    chosen: BTreeSet<PathBuf>,
    scroll: usize,
    sending: Option<Sending>,
    // what arrived
    recent: Vec<Received>,
    trusted: Vec<String>,
    // the phone's screen
    stage: ScreenStage,
    audio: bool,
    screen_off: bool,
    addr: Option<String>,
    found: mpsc::Receiver<Found>,
    found_tx: mpsc::Sender<Found>,
    /// Clickable rows from the last draw.
    rows: Vec<(Rect, usize)>,
    note: Option<(String, bool)>,
    /// Mandar opens taco (discada's explorer) to choose, when it's installed.
    taco: bool,
    want_taco: bool,
    // the lumbre
    lumbre_on: bool,
    lumbre_busy: Option<Instant>,
    lumbre_rx: Option<mpsc::Receiver<std::result::Result<bool, String>>>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn list_dir(dir: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if name.starts_with('.') {
                return None;
            }
            let path = e.path();
            Some(Entry { dir: path.is_dir(), path, name })
        })
        .collect();
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    entries
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{} KB", b >> 10),
        b => format!("{b} B"),
    }
}

/// "hoy 14:05", "ayer", "hace 3 días".
fn when(at: u64, lang: Lang) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let ago = now.saturating_sub(at);
    match ago {
        0..=59 => lang.t("ahora", "just now").into(),
        60..=3599 => format!("{} min", ago / 60),
        3600..=86_399 => format!("{} h", ago / 3600),
        _ => format!("{} {}", ago / 86_400, lang.t("d", "d")),
    }
}

impl App {
    fn new(lang: Lang, start: Start) -> Self {
        let (phones_tx, phones_rx) = mpsc::channel();
        let (found_tx, found) = mpsc::channel();
        let config = Config::load();
        let dir = home();
        let mut app = App {
            lang,
            theme: Theme::new(Look::load()),
            stamp: theme::stamp(),
            restyled: false,
            quit: false,
            page: Page::Menu,
            first: Page::Menu,
            sel: 0,
            phones: Vec::new(),
            phones_rx,
            phones_tx,
            phones_at: Instant::now(),
            entries: list_dir(&dir),
            dir,
            chosen: BTreeSet::new(),
            scroll: 0,
            sending: None,
            recent: state::recent(8),
            trusted: config.trusted,
            stage: ScreenStage::Searching(Instant::now()),
            audio: false,
            screen_off: false,
            addr: None,
            found,
            found_tx,
            rows: Vec::new(),
            note: None,
            taco: which("taco"),
            want_taco: false,
            lumbre_on: lumbre::is_on(),
            lumbre_busy: None,
            lumbre_rx: None,
        };
        app.load_phones();
        match start {
            Start::Menu => {}
            Start::Send(files) => {
                app.first = Page::Sending;
                app.send(files);
            }
            Start::Screen { addr, audio, screen_off } => {
                app.first = Page::Screen;
                (app.addr, app.audio, app.screen_off) = (addr, audio, screen_off);
                app.go(Page::Screen);
            }
        }
        app
    }

    fn load_phones(&mut self) {
        let tx = self.phones_tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(bt::phones().await);
        });
        self.phones_at = Instant::now();
    }

    fn go(&mut self, page: Page) {
        self.page = page;
        self.sel = 0;
        self.scroll = 0;
        self.note = None;
        match page {
            Page::Receive => {
                self.recent = state::recent(8);
                self.trusted = Config::load().trusted;
            }
            Page::Pick if self.taco => {
                // taco runs in this same terminal; the main loop hands it over
                self.page = Page::Menu;
                self.want_taco = true;
            }
            Page::Pick => self.entries = list_dir(&self.dir),
            Page::Screen => self.search_screen(),
            Page::Lumbre => self.lumbre_on = lumbre::is_on(),
            _ => {}
        }
    }

    /// Lights or puts out the lumbre; NetworkManager takes a few seconds.
    fn toggle_lumbre(&mut self) {
        if self.lumbre_busy.is_some() {
            return;
        }
        let light = !self.lumbre_on;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let r = if light { lumbre::start().map(|_| true) } else { lumbre::stop().map(|()| false) };
            let _ = tx.send(r.map_err(|e| e.to_string()));
        });
        self.lumbre_busy = Some(Instant::now());
        self.lumbre_rx = Some(rx);
        self.note = None;
    }

    /// Esc/back: to the menu, or out when `picha` opened on this page.
    fn back(&mut self) {
        if self.page == self.first || self.page == Page::Menu {
            self.quit = true;
        } else {
            self.go(Page::Menu);
        }
    }

    fn search_screen(&mut self) {
        self.stage = ScreenStage::Searching(Instant::now());
        let (tx, addr) = (self.found_tx.clone(), self.addr.clone());
        tokio::spawn(async move {
            let found = match pantalla::reach(addr).await {
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

    fn screen_items(&self) -> Vec<ScreenItem> {
        match self.stage {
            ScreenStage::Ready(..) => vec![ScreenItem::Open, ScreenItem::Audio, ScreenItem::PhoneScreen, ScreenItem::Back],
            ScreenStage::Failed(_) => vec![ScreenItem::Retry, ScreenItem::Back],
            _ => vec![ScreenItem::Back],
        }
    }

    fn screen_choose(&mut self, item: ScreenItem) {
        match item {
            ScreenItem::Open => {
                if let ScreenStage::Ready(link, info) = &self.stage {
                    self.stage = if pantalla::open(&link.serial, &info.name, self.audio, self.screen_off) {
                        ScreenStage::Opening(Instant::now())
                    } else {
                        ScreenStage::Failed(Some("scrcpy (sudo pacman -S scrcpy)".into()))
                    };
                }
            }
            ScreenItem::Audio => self.audio = !self.audio,
            ScreenItem::PhoneScreen => self.screen_off = !self.screen_off,
            ScreenItem::Retry => self.search_screen(),
            ScreenItem::Back => self.back(),
        }
    }

    /// Sends by the fastest route: Quick Share when the phone answers the nudge on
    /// the network (the lumbre's too), else Bluetooth to the paired phone.
    fn send(&mut self, files: Vec<PathBuf>) {
        self.page = Page::Sending;
        let (tx, rx) = mpsc::channel();
        let list = files.clone();
        let paired = self.phones.first().cloned();
        tokio::spawn(async move {
            let done = route_and_send(list, paired, tx.clone()).await;
            let _ = tx.send(Update::Done(done));
        });
        self.sending = Some(Sending { files, started: Instant::now(), via: None, why: None, pin: None, progress: (0, 0, 0), rx, result: None });
    }

    fn pick_open(&mut self) {
        let Some(e) = self.entries.get(self.sel) else { return };
        if e.dir {
            self.dir = e.path.clone();
            self.entries = list_dir(&self.dir);
            self.sel = 0;
            self.scroll = 0;
        } else {
            let files: Vec<PathBuf> =
                if self.chosen.is_empty() { vec![e.path.clone()] } else { self.chosen.iter().cloned().collect() };
            self.chosen.clear();
            self.send(files);
        }
    }

    fn pick_up(&mut self) {
        if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
            let from = self.dir.clone();
            self.dir = parent;
            self.entries = list_dir(&self.dir);
            self.sel = self.entries.iter().position(|e| e.path == from).unwrap_or(0);
        }
    }

    fn rows_len(&self) -> usize {
        match self.page {
            Page::Menu => MENU.len(),
            Page::Pick => self.entries.len(),
            // the hide switch, what arrived, the trusted phones
            Page::Receive => 1 + self.recent.len() + self.trusted.len(),
            Page::Screen => self.screen_items().len(),
            Page::Lumbre => 2,
            _ => 0,
        }
    }

    fn activate(&mut self, i: usize) {
        self.sel = i;
        match self.page {
            Page::Menu => self.go(MENU[i]),
            Page::Pick => self.pick_open(),
            Page::Receive if i == 0 => state::set_hidden(!state::hidden()),
            Page::Receive => {
                if let Some(r) = self.recent.get(i - 1) {
                    let _ = std::process::Command::new("xdg-open")
                        .arg(&r.path)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                }
            }
            Page::Screen => {
                if let Some(&item) = self.screen_items().get(i) {
                    self.screen_choose(item);
                }
            }
            Page::Lumbre if i == 0 => self.toggle_lumbre(),
            Page::Lumbre => self.back(),
            _ => {}
        }
    }

    fn key(&mut self, code: KeyCode) {
        let n = self.rows_len();
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.back(),
            KeyCode::Up | KeyCode::Char('k') if n > 0 => self.sel = (self.sel + n - 1) % n,
            KeyCode::Down | KeyCode::Char('j') if n > 0 => self.sel = (self.sel + 1) % n,
            KeyCode::Enter if n > 0 => self.activate(self.sel),
            _ => self.page_key(code),
        }
    }

    fn page_key(&mut self, code: KeyCode) {
        match (self.page, code) {
            (Page::Pick, KeyCode::Char(' ')) => {
                if let Some(e) = self.entries.get(self.sel).filter(|e| !e.dir) {
                    if !self.chosen.remove(&e.path) {
                        self.chosen.insert(e.path.clone());
                    }
                    self.sel = (self.sel + 1).min(self.entries.len().saturating_sub(1));
                }
            }
            (Page::Pick, KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h')) => self.pick_up(),
            (Page::Pick, KeyCode::Right | KeyCode::Char('l')) => {
                if self.entries.get(self.sel).is_some_and(|e| e.dir) {
                    self.pick_open();
                }
            }
            (Page::Pick, KeyCode::Char('~')) => {
                self.dir = home();
                self.entries = list_dir(&self.dir);
                self.sel = 0;
            }
            (Page::Pick, KeyCode::Char('s')) if !self.chosen.is_empty() => {
                let files = std::mem::take(&mut self.chosen).into_iter().collect();
                self.send(files);
            }
            (Page::Sending, KeyCode::Enter) if self.sending.as_ref().is_none_or(|s| s.result.is_some()) => self.back(),
            (Page::Receive, KeyCode::Char('d')) => {
                if let Some(phone) = self.sel.checked_sub(1 + self.recent.len()).and_then(|i| self.trusted.get(i)).cloned() {
                    let mut config = Config::load();
                    config.trusted.retain(|t| *t != phone);
                    let _ = config.save();
                    self.trusted = config.trusted;
                    self.sel = self.sel.min(self.rows_len().saturating_sub(1));
                }
            }
            (Page::Receive, KeyCode::Char('o')) => {
                let _ = std::process::Command::new("xdg-open")
                    .arg(Config::load().dir())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
            (Page::Screen, KeyCode::Char('a')) if matches!(self.stage, ScreenStage::Ready(..)) => self.audio = !self.audio,
            (Page::Screen, KeyCode::Char('o')) if matches!(self.stage, ScreenStage::Ready(..)) => self.screen_off = !self.screen_off,
            (Page::Screen, KeyCode::Char('r')) if matches!(self.stage, ScreenStage::Failed(_)) => self.search_screen(),
            _ => {}
        }
    }

    fn click(&mut self, x: u16, y: u16) {
        if let Some(&(_, i)) = self.rows.iter().find(|(r, _)| r.contains((x, y).into())) {
            self.activate(i);
        }
    }

    fn tick(&mut self) {
        if let Ok(p) = self.phones_rx.try_recv() {
            self.phones = p;
        }
        if self.phones_at.elapsed() > Duration::from_secs(10) {
            self.load_phones();
        }
        if let Ok(found) = self.found.try_recv() {
            self.stage = match found {
                Ok((link, info)) => ScreenStage::Ready(link, info),
                Err(why) => ScreenStage::Failed(why),
            };
            if self.page == Page::Screen {
                self.sel = 0;
            }
        }
        if let ScreenStage::Opening(at) = self.stage
            && at.elapsed() > Duration::from_millis(900)
        {
            self.stage = ScreenStage::Searching(Instant::now());
            self.back();
        }
        if let Some(r) = self.lumbre_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.lumbre_rx = None;
            self.lumbre_busy = None;
            match r {
                Ok(on) => self.lumbre_on = on,
                Err(e) => self.note = Some((e, true)),
            }
        }
        if let Some(s) = self.sending.as_mut() {
            while let Ok(u) = s.rx.try_recv() {
                match u {
                    Update::Via(wifi, to) => s.via = Some((wifi, to)),
                    Update::Why(why) => s.why = Some(why),
                    Update::Pin(pin) => s.pin = Some(pin),
                    Update::Progress(i, sent, total) => s.progress = (i, sent, total),
                    Update::Done(r) => s.result = Some(r),
                }
            }
        }
        let stamp = theme::stamp();
        if stamp != self.stamp {
            self.stamp = stamp;
            self.theme = Theme::new(Look::load());
            self.restyled = true;
        }
    }
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(program).is_file()))
}

/// taco (discada's explorer) in choose mode, in this terminal: the files picked,
/// starting where the last choice was made.
fn choose_with_taco() -> Vec<PathBuf> {
    let dir = state::dir();
    let _ = std::fs::create_dir_all(&dir);
    let (out, cwd) = (dir.join("taco-elegidos"), dir.join("taco-carpeta"));
    let _ = std::fs::remove_file(&out);
    // where the last choice was made; the first time, the pictures folder
    let pictures = std::fs::read_to_string(home().join(".config/user-dirs.dirs"))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("XDG_PICTURES_DIR="))
                .map(|v| PathBuf::from(v.trim_matches('"').replace("$HOME", &home().to_string_lossy())))
        })
        .filter(|p| p.is_dir())
        .unwrap_or_else(home);
    let start = std::fs::read_to_string(&cwd).ok().map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or(pictures);
    let _ = std::process::Command::new("taco").arg(&start).arg("--choose").arg(&out).arg("--cwd-file").arg(&cwd).status();
    std::fs::read_to_string(&out).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect()
}

/// Quick Share if the phone shows up on the network after the nudge, else Bluetooth.
async fn route_and_send(files: Vec<PathBuf>, paired: Option<Phone>, tx: mpsc::Sender<Update>) -> Result<(), String> {
    let wifi = lumbre::wifi();
    picha::send::log(&format!("send {} file(s) · wifi {:?} · lumbre {}", files.len(), wifi.and_then(|w| w.network), lumbre::is_on()));
    let r = route_and_send_inner(files, paired, tx).await;
    picha::send::log(&format!("result: {r:?}"));
    r
}

async fn route_and_send_inner(files: Vec<PathBuf>, paired: Option<Phone>, tx: mpsc::Sender<Update>) -> Result<(), String> {
    use picha::wire::{DeviceKind, Endpoint};
    let found = picha::send::find_phone(Duration::from_secs(10)).await;
    if let Err(picha::send::Miss::Refused) = found {
        let _ = tx.send(Update::Why(picha::send::Miss::Refused));
    }
    if let Ok((stream, _addr, endpoint)) = found {
        let to = endpoint.map(|e| e.name).or_else(|| paired.as_ref().map(|p| p.name.clone())).unwrap_or_else(|| "cel".into());
        let _ = tx.send(Update::Via(true, to));
        let me = Endpoint { name: Config::load().name(), kind: DeviceKind::Laptop };
        let (ptx, mut prx) = tokio::sync::mpsc::channel::<picha::send::Progress>(64);
        let fwd = tx.clone();
        tokio::spawn(async move {
            while let Some((i, sent, total)) = prx.recv().await {
                let _ = fwd.send(Update::Progress(i, sent, total));
            }
        });
        let pin_tx = tx.clone();
        return picha::send::send(stream, &me, &files, move |pin| drop(pin_tx.send(Update::Pin(pin.to_string()))), Some(ptx))
            .await
            .map_err(|e| e.to_string());
    }
    let phone = match paired {
        Some(p) => p,
        None => bt::phones().await.into_iter().next().ok_or("no phone found, on the network or paired over Bluetooth")?,
    };
    picha::send::log(&format!("bluetooth to {} {}", phone.name, phone.address));
    let _ = tx.send(Update::Via(false, phone.name.clone()));
    let (ptx, mut prx) = tokio::sync::mpsc::channel::<bt::SendProgress>(64);
    let fwd = tx.clone();
    tokio::spawn(async move {
        while let Some((i, sent, total)) = prx.recv().await {
            let _ = fwd.send(Update::Progress(i, sent, total));
        }
    });
    bt::send(&phone.address, &files, ptx).await.map_err(|e| e.to_string())
}

pub async fn run(lang: Lang, start: Start) -> io::Result<()> {
    let mut app = App::new(lang, start);
    tokio::task::block_in_place(|| {
        let mut terminal = ratatui::init();
        let _ = execute!(io::stdout(), EnableMouseCapture);
        let result = (|| -> io::Result<()> {
            while !app.quit {
                if std::mem::take(&mut app.want_taco) {
                    // hand the terminal to taco, then take it back
                    let _ = execute!(io::stdout(), DisableMouseCapture);
                    ratatui::restore();
                    let files = choose_with_taco();
                    terminal = ratatui::init();
                    let _ = execute!(io::stdout(), EnableMouseCapture);
                    terminal.clear()?;
                    if !files.is_empty() {
                        app.send(files);
                    }
                }
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
    let title = match app.page {
        Page::Menu => th.label("󰄜", "picha", "picha"),
        Page::Pick => th.label("󰒋", lang.t("mandar", "send"), lang.t("mandar al cel", "send to the phone")),
        Page::Sending => th.label("󰒋", lang.t("mandar", "send"), lang.t("mandando", "sending")),
        Page::Receive => th.label("󰇚", lang.t("recibir", "receive"), lang.t("recibir", "receive")),
        Page::Screen => th.label("󰹑", lang.t("pantalla", "screen"), lang.t("pantalla del cel", "phone screen")),
        Page::Lumbre => th.label("󱠡", "lumbre", "lumbre"),
    };
    let block = th.block(title, true);
    let inner = block.inner(body).inner(Margin { horizontal: 1, vertical: 0 });
    f.render_widget(block, body);
    app.rows.clear();

    let hints = match app.page {
        Page::Menu => draw_menu(f, app, inner),
        Page::Pick => draw_pick(f, app, inner),
        Page::Sending => draw_sending(f, app, inner),
        Page::Receive => draw_receive(f, app, inner),
        Page::Screen => draw_screen(f, app, inner),
        Page::Lumbre => draw_lumbre(f, app, inner),
    };
    let th = &app.theme;
    let line = match &app.note {
        Some((text, true)) => Span::styled(th.case(text), th.warn),
        Some((text, false)) => Span::styled(th.case(text), th.acc2),
        None => Span::styled(th.case(hints), th.dim),
    };
    f.render_widget(Paragraph::new(line), footer);
}

/// The phone at the top of the menu and the picker: name, Bluetooth, battery.
fn phone_lines(app: &App) -> Vec<Line<'static>> {
    let (th, lang) = (&app.theme, app.lang);
    let Some(p) = app.phones.first() else {
        return vec![Line::styled(
            th.case(lang.t("sin cel emparejado por Bluetooth", "no phone paired over Bluetooth")),
            th.dim,
        )];
    };
    let mut row = vec![Span::styled(format!("{}{}", icon(th, "󰄜"), p.name), th.current)];
    if let Some(b) = p.battery {
        row.push(Span::raw("   "));
        row.extend(th.bar(b as f64 / 100.0, 10, if b <= 15 { th.warn } else { th.acc.remove_modifier(Modifier::BOLD) }));
        row.push(Span::styled(format!(" {b}%"), th.fg));
    }
    let link = if p.connected { lang.t("bluetooth conectado", "bluetooth connected") } else { lang.t("emparejado", "paired") };
    vec![Line::from(row), Line::styled(th.case(link), th.dim)]
}

fn menu_rows(f: &mut Frame, app: &mut App, area: Rect, items: Vec<String>) {
    for (i, text) in items.into_iter().enumerate() {
        let y = area.y + i as u16;
        if y >= area.bottom() {
            break;
        }
        let rect = Rect::new(area.x, y, area.width, 1);
        f.render_widget(Paragraph::new(Line::from(app.theme.choice(&text, i == app.sel, true))), rect);
        app.rows.push((rect, i));
    }
}

fn draw_menu(f: &mut Frame, app: &mut App, area: Rect) -> &'static str {
    let lang = app.lang;
    let mut lines = phone_lines(app);
    lines.push(Line::default());
    let top = lines.len() as u16;
    f.render_widget(Paragraph::new(lines), area);
    let th = &app.theme;
    let receiving = match (state::receiver_running(), state::hidden()) {
        (false, _) => lang.t("apagado", "off"),
        (true, true) => lang.t("oculto", "hidden"),
        (true, false) => lang.t("visible", "visible"),
    };
    let items = vec![
        format!("{}{}", icon(th, "󰒋"), lang.t("mandar al cel", "send to the phone")),
        format!("{}{}  · {receiving}", icon(th, "󰇚"), lang.t("recibir", "receive")),
        format!("{}{}", icon(th, "󰹑"), lang.t("pantalla del cel", "phone screen")),
        format!(
            "{}lumbre  · {}",
            icon(th, "󱠡"),
            if app.lumbre_on { lang.t("prendida", "lit") } else { lang.t("sin red", "no network") }
        ),
    ];
    let menu = Rect { y: area.y + top, height: area.height.saturating_sub(top), ..area };
    menu_rows(f, app, menu, items);
    lang.t("↑↓ mover · enter elegir · q salir", "↑↓ move · enter choose · q quit")
}

fn draw_pick(f: &mut Frame, app: &mut App, area: Rect) -> &'static str {
    let (th, lang) = (&app.theme, app.lang);
    let shown = app.dir.strip_prefix(home()).map(|p| format!("~/{}", p.display())).unwrap_or_else(|_| app.dir.display().to_string());
    let mut head = vec![Span::styled(shown, th.fg)];
    if !app.chosen.is_empty() {
        head.push(Span::styled(th.case(&format!("   {} {}", app.chosen.len(), lang.t("elegidos", "chosen"))), th.acc2));
    }
    f.render_widget(Paragraph::new(Line::from(head)), Rect { height: 1, ..area });
    let list = Rect { y: area.y + 2, height: area.height.saturating_sub(2), ..area };
    let h = list.height as usize;
    if app.sel < app.scroll {
        app.scroll = app.sel;
    } else if h > 0 && app.sel >= app.scroll + h {
        app.scroll = app.sel + 1 - h;
    }
    let th = &app.theme;
    let rows: Vec<(usize, String)> = app
        .entries
        .iter()
        .enumerate()
        .skip(app.scroll)
        .take(h)
        .map(|(i, e)| {
            let mark = if app.chosen.contains(&e.path) { "● " } else { "  " };
            let glyph = if e.dir { "󰉋" } else { "󰈔" };
            let tail = if e.dir { "/" } else { "" };
            (i, format!("{mark}{}{}{tail}", icon(th, glyph), e.name))
        })
        .collect();
    for (row, (i, text)) in rows.into_iter().enumerate() {
        let rect = Rect::new(list.x, list.y + row as u16, list.width, 1);
        let spans = app.theme.choice(&text, i == app.sel, true);
        f.render_widget(Paragraph::new(Line::from(spans)), rect);
        app.rows.push((rect, i));
    }
    if app.chosen.is_empty() {
        lang.t("enter abrir/mandar · espacio elegir varios · ← atrás · esc menú", "enter open/send · space pick several · ← up · esc menu")
    } else {
        lang.t("s mandar los elegidos · espacio elegir · ← atrás · esc menú", "s send the chosen · space pick · ← up · esc menu")
    }
}

fn draw_sending(f: &mut Frame, app: &mut App, area: Rect) -> &'static str {
    let (th, lang) = (&app.theme, app.lang);
    let Some(s) = &app.sending else {
        return lang.t("enter volver", "enter back");
    };
    let (i, sent, total) = s.progress;
    let mut lines = vec![match &s.via {
        None => Line::from(vec![
            Span::styled(format!("{} ", spinner(th, s.started)), th.acc),
            Span::styled(th.case(lang.t("buscando el cel…", "looking for the phone…")), th.fg),
        ]),
        Some((wifi, to)) => Line::from(vec![
            Span::styled(th.case(&format!("{} ", lang.t("a", "to"))), th.dim),
            Span::styled(to.clone(), th.current),
            Span::styled(
                th.case(&format!(
                    "  · {}",
                    if *wifi { lang.t("Quick Share, rápido", "Quick Share, fast") } else { lang.t("por Bluetooth", "over Bluetooth") }
                )),
                th.dim,
            ),
        ]),
    }];
    if s.why == Some(picha::send::Miss::Refused) {
        lines.push(Line::styled(th.case(lang.t("¿Quick Share apagado en el cel?", "Quick Share off on the phone?")), th.dim));
    }
    if let Some(pin) = &s.pin {
        lines.push(Line::styled(th.case(&format!("PIN {pin} · {}", lang.t("el mismo que en el cel", "same as on the phone"))), th.acc2));
    }
    lines.push(Line::default());
    for (n, file) in s.files.iter().enumerate() {
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let state = match (&s.result, n.cmp(&i)) {
            (Some(Ok(())), _) | (_, std::cmp::Ordering::Less) => Span::styled("  ✓", th.acc),
            (Some(Err(_)), std::cmp::Ordering::Equal) => Span::styled("  ✗", th.warn),
            _ => Span::raw(""),
        };
        lines.push(Line::from(vec![Span::styled(format!("{}{name}", icon(th, "󰈔")), th.fg), state]));
    }
    lines.push(Line::default());
    match &s.result {
        None => {
            let frac = if total > 0 { sent as f64 / total as f64 } else { 0.0 };
            let mut bar = th.bar(frac, area.width.saturating_sub(14).min(30), th.acc.remove_modifier(Modifier::BOLD));
            bar.push(Span::styled(format!(" {}", size(sent)), th.fg));
            lines.push(Line::from(bar));
            lines.push(Line::styled(th.case(lang.t("acepta en el cel", "accept on the phone")), th.dim));
        }
        Some(Ok(())) => lines.push(Line::styled(th.case(lang.t("listo, ya está en el cel", "done, it's on the phone")), th.acc)),
        Some(Err(e)) => {
            lines.push(Line::styled(th.case(lang.t("no se pudo mandar", "couldn't send")), th.warn));
            lines.push(Line::styled(e.clone(), th.dim));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
    if s.result.is_some() { lang.t("enter volver · q salir", "enter back · q quit") } else { lang.t("q salir", "q quit") }
}

fn draw_receive(f: &mut Frame, app: &mut App, area: Rect) -> &'static str {
    let (th, lang) = (&app.theme, app.lang);
    let running = state::receiver_running();
    let hidden = state::hidden();
    let mut lines = vec![if !running {
        Line::styled(th.case(lang.t("no está recibiendo (arranca con Hyprland)", "not receiving (starts with Hyprland)")), th.warn)
    } else if hidden {
        Line::styled(th.case(lang.t("oculto: nadie te ve ni te puede mandar", "hidden: nobody sees you or can send")), th.warn)
    } else {
        Line::from(vec![
            Span::styled(format!("{}{}", icon(th, "󰇚"), th.case(lang.t("visible como", "visible as"))), th.fg),
            Span::styled(format!(" {}", Config::load().name()), th.current),
            Span::styled(th.case(lang.t("  · Wi-Fi y Bluetooth", "  · Wi-Fi and Bluetooth")), th.dim),
        ])
    }];
    lines.push(Line::default());
    let top = lines.len() as u16;
    f.render_widget(Paragraph::new(lines), area);

    // row 0: the switch; then what arrived; then the trusted phones
    let switch = if hidden { lang.t("mostrarme", "show me") } else { lang.t("ocultarme", "hide me") };
    let switch = format!("{}{switch}", icon(th, if hidden { "󰈈" } else { "󰈉" }));
    let mut y = area.y + top;
    let rect = Rect::new(area.x, y, area.width, 1);
    f.render_widget(Paragraph::new(Line::from(app.theme.choice(&switch, app.sel == 0, true))), rect);
    app.rows.push((rect, 0));
    y += 2;

    let th = &app.theme;
    f.render_widget(Paragraph::new(Span::styled(th.case(lang.t("llegó", "arrived")), th.acc)), Rect::new(area.x, y, area.width, 1));
    y += 1;
    if app.recent.is_empty() && y < area.bottom() {
        f.render_widget(Paragraph::new(Span::styled(th.case(lang.t("  nada todavía", "  nothing yet")), th.dim)), Rect::new(area.x, y, area.width, 1));
        y += 1;
    }
    let mut rows: Vec<(usize, String)> = app
        .recent
        .iter()
        .enumerate()
        .map(|(k, r)| {
            let name = r.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            (1 + k, format!("{name}  · {} · {}", r.from, when(r.at, lang)))
        })
        .collect();
    let first_trusted = 1 + app.recent.len();
    rows.extend(app.trusted.iter().enumerate().map(|(k, t)| (first_trusted + k, t.clone())));
    for (i, text) in rows {
        if i == first_trusted {
            y += 1;
            if y < area.bottom() {
                f.render_widget(
                    Paragraph::new(Span::styled(app.theme.case(lang.t("de confianza (sin preguntar)", "trusted (no prompt)")), app.theme.acc)),
                    Rect::new(area.x, y, area.width, 1),
                );
            }
            y += 1;
        }
        if y >= area.bottom() {
            break;
        }
        let rect = Rect::new(area.x, y, area.width, 1);
        f.render_widget(Paragraph::new(Line::from(app.theme.choice(&text, i == app.sel, true))), rect);
        app.rows.push((rect, i));
        y += 1;
    }
    lang.t("enter elegir · o carpeta · d olvidar · esc menú", "enter choose · o folder · d forget · esc menu")
}

fn draw_screen(f: &mut Frame, app: &mut App, area: Rect) -> &'static str {
    let (th, lang) = (&app.theme, app.lang);
    let mut lines: Vec<Line> = Vec::new();
    match &app.stage {
        ScreenStage::Searching(since) => {
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", spinner(th, *since)), th.acc),
                Span::styled(th.case(lang.t("buscando el cel…", "looking for the phone…")), th.fg),
            ]));
            lines.push(Line::default());
            lines.push(Line::styled(th.case(lang.t("por Wi-Fi o por cable", "over Wi-Fi or the cable")), th.dim));
        }
        ScreenStage::Failed(why) => {
            match why {
                None => {
                    lines.push(Line::styled(th.case(lang.t("no encontré el cel", "no phone found")), th.warn));
                    lines.push(Line::default());
                    lines.push(Line::styled(th.case(lang.t("prende Depuración inalámbrica", "turn on Wireless debugging")), th.fg));
                    lines.push(Line::styled(th.case(lang.t("  (búscala en Ajustes del cel)", "  (search for it in the phone's Settings)")), th.dim));
                    lines.push(Line::styled(th.case(lang.t("o conecta el cable", "or plug in the cable")), th.fg));
                }
                Some(why) => {
                    lines.push(Line::styled(th.case(lang.t("algo falló", "something failed")), th.warn));
                    lines.push(Line::styled(why.clone(), th.dim));
                }
            }
            lines.push(Line::default());
        }
        ScreenStage::Ready(link, info) => {
            lines.push(Line::from(vec![
                Span::styled(format!("{}{}", icon(th, "󰄜"), info.name), th.current),
                Span::styled(format!("  {}", info.model), th.dim),
            ]));
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
            lines.push(Line::from(match (link.cable, &link.wifi) {
                (true, Some(w)) => vec![
                    Span::styled(format!("{}{}", icon(th, "󰕓"), th.case(lang.t("cable", "cable"))), th.fg),
                    Span::styled(th.case(&format!("  · {} {w}", lang.t("Wi-Fi lista en", "Wi-Fi ready at"))), th.dim),
                ],
                (true, None) => vec![Span::styled(format!("{}{}", icon(th, "󰕓"), th.case(lang.t("cable", "cable"))), th.fg)],
                (false, w) => vec![
                    Span::styled(format!("{}Wi-Fi {}", icon(th, "󰖩"), w.as_deref().unwrap_or(&link.serial)), th.fg),
                    Span::styled(th.case(&format!("  · {}", lang.t("cifrado", "encrypted"))), th.dim),
                ],
            }));
            if let Some(free) = &info.free {
                lines.push(Line::styled(format!("{}{free} {}", icon(th, "󰋊"), th.case(lang.t("libres", "free"))), th.fg));
            }
            lines.push(Line::default());
        }
        ScreenStage::Opening(since) => {
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", spinner(th, *since)), th.acc),
                Span::styled(th.case(lang.t("abriendo la pantalla…", "opening the screen…")), th.fg),
            ]));
        }
    }
    let top = lines.len() as u16;
    f.render_widget(Paragraph::new(lines), area);
    if matches!(app.stage, ScreenStage::Ready(..) | ScreenStage::Failed(_)) {
        let on_off = |on: bool| lang.t(if on { "sí" } else { "no" }, if on { "on" } else { "off" });
        let items: Vec<String> = app
            .screen_items()
            .into_iter()
            .map(|item| {
                let th = &app.theme;
                match item {
                    ScreenItem::Open => format!("{}{}", icon(th, "󰹑"), lang.t("abrir la pantalla", "open the screen")),
                    ScreenItem::Audio => format!("{}{}  {}", icon(th, "󰕾"), lang.t("sonido aquí", "sound here"), on_off(app.audio)),
                    ScreenItem::PhoneScreen => format!(
                        "{}{}  {}",
                        icon(th, "󰄜"),
                        lang.t("pantalla del cel", "phone's own screen"),
                        if app.screen_off { lang.t("apagada", "off") } else { lang.t("prendida", "on") }
                    ),
                    ScreenItem::Retry => format!("{}{}", icon(th, "󰑐"), lang.t("buscar otra vez", "look again")),
                    ScreenItem::Back => format!("{}{}", icon(th, "󰁍"), if app.first == Page::Screen { lang.t("salir", "quit") } else { lang.t("volver", "back") }),
                }
            })
            .collect();
        let menu = Rect { y: area.y + top, height: area.height.saturating_sub(top), ..area };
        menu_rows(f, app, menu, items);
    }
    match app.stage {
        ScreenStage::Ready(..) => lang.t("↑↓ mover · enter elegir · a sonido · o pantalla · esc volver", "↑↓ move · enter choose · a sound · o screen · esc back"),
        ScreenStage::Failed(_) => lang.t("r buscar otra vez · esc volver", "r look again · esc back"),
        _ => lang.t("esc volver", "esc back"),
    }
}

fn draw_lumbre(f: &mut Frame, app: &mut App, area: Rect) -> &'static str {
    use ratatui::style::{Color, Style};
    let (th, lang) = (&app.theme, app.lang);
    let mut lines: Vec<Line> = Vec::new();
    if let Some(since) = app.lumbre_busy {
        let what = if app.lumbre_on { lang.t("apagando la lumbre…", "putting the lumbre out…") } else { lang.t("prendiendo la lumbre…", "lighting the lumbre…") };
        lines.push(Line::from(vec![Span::styled(format!("{} ", spinner(th, since)), th.acc), Span::styled(th.case(what), th.fg)]));
        f.render_widget(Paragraph::new(lines), area);
        return lang.t("esc menú", "esc menu");
    }
    let l = lumbre::Lumbre::get();
    if app.lumbre_on {
        lines.push(Line::from(vec![
            Span::styled(th.case(lang.t("red ", "network ")), th.dim),
            Span::styled(l.ssid.clone(), th.current),
            Span::styled(th.case(lang.t("   clave ", "   password ")), th.dim),
            Span::styled(l.password.clone(), th.fg),
        ]));
        lines.push(Line::styled(th.case(lang.t("apunta la cámara del cel al código", "point the phone's camera at the code")), th.dim));
        let top = lines.len() as u16;
        f.render_widget(Paragraph::new(lines), area);
        // black on white whatever the look: what phone cameras read
        let qr: Vec<Line> = lumbre::qr_lines(&l.qr_text())
            .into_iter()
            .map(|r| Line::styled(r, Style::new().fg(Color::Black).bg(Color::White)))
            .collect();
        let qh = qr.len() as u16;
        let qw = qr.first().map_or(0, |r| r.width() as u16);
        let qr_area = Rect { x: area.x, y: area.y + top + 1, width: qw.min(area.width), height: qh.min(area.height.saturating_sub(top + 1)) };
        f.render_widget(Paragraph::new(qr), qr_area);
        let menu = Rect { y: qr_area.bottom() + 1, height: area.bottom().saturating_sub(qr_area.bottom() + 1), ..area };
        let th = &app.theme;
        let items = vec![
            format!("{}{}", icon(th, "󱠡"), lang.t("apagar la lumbre", "put the lumbre out")),
            format!("{}{}", icon(th, "󰁍"), lang.t("volver", "back")),
        ];
        menu_rows(f, app, menu, items);
    } else {
        lines.push(Line::styled(th.case(lang.t("cuando no hay red", "when there's no network")), th.current));
        lines.push(Line::styled(th.case(lang.t("la laptop pone su propio Wi-Fi", "the laptop makes its own Wi-Fi")), th.fg));
        lines.push(Line::styled(th.case(&format!("{} {}", lang.t("se llama", "called"), l.ssid)), th.dim));
        if let Some(net) = lumbre::wifi().and_then(|w| w.network) {
            lines.push(Line::default());
            lines.push(Line::styled(th.case(&format!("{} {net} {}", lang.t("la laptop deja", "the laptop leaves"), lang.t("mientras", "meanwhile"))), th.warn));
        }
        lines.push(Line::default());
        let top = lines.len() as u16;
        f.render_widget(Paragraph::new(lines), area);
        let th = &app.theme;
        let items = vec![
            format!("{}{}", icon(th, "󱠡"), lang.t("prender la lumbre", "light the lumbre")),
            format!("{}{}", icon(th, "󰁍"), lang.t("volver", "back")),
        ];
        let menu = Rect { y: area.y + top, height: area.height.saturating_sub(top), ..area };
        menu_rows(f, app, menu, items);
    }
    lang.t("enter elegir · esc menú", "enter choose · esc menu")
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;

    /// The lit lumbre page as text: `cargo test lumbre_preview -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn lumbre_preview() {
        let mut app = App::new(Lang(false), Start::Menu);
        app.page = Page::Lumbre;
        app.lumbre_on = true;
        let mut terminal = Terminal::new(TestBackend::new(68, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        for y in 0..buf.area.height {
            println!("{}", (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end());
        }
    }
}
