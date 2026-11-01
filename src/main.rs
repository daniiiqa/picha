mod tui;

use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::mpsc;

use picha::config::Config;
use picha::error::{Error, Result};
use picha::receive::{self, Event, Offer, TextKind};
use picha::wire::{self, DeviceKind, Endpoint};
use picha::mdns;

/// How long an offer waits for an answer. Phones give up after about 15 seconds
/// (the S21 does at 15), so waiting longer only shows a prompt that can't work.
const ASK_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the notifications that only report something stay up.
const INFO_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(
    name = "picha",
    version,
    about = "Your phone and your computers, a la carne asada: picha opens the menu, picha foto.jpg sends it, picha -t \"…\" sends a text",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Files to send (you pick where to)
    files: Vec<PathBuf>,
    /// A text to send: it arrives as a notification and in the clipboard, not as a file
    #[arg(short = 't', long = "texto", value_name = "TEXT", conflicts_with = "files")]
    text: Option<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Be visible to phones on this network and receive what they share (the default)
    Recibe {
        /// Save here instead of the downloads folder
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// The phone's screen here (scrcpy): clipboard both ways, drag files onto it
    Pantalla {
        /// The phone's address from Wireless debugging ("IP address & Port"); remembered
        addr: Option<String>,
        /// Pair the phone first (once): Wireless debugging → Pair device with pairing code
        #[arg(long)]
        empareja: bool,
        /// Play the phone's sound here too
        #[arg(long)]
        audio: bool,
        /// Turn the phone's own screen off while it shows here
        #[arg(long)]
        apagada: bool,
        /// Open it right away instead of asking first
        #[arg(long)]
        ya: bool,
    },
    /// List the phones whose transfers are accepted without asking
    Confianza,
    /// Stop trusting a phone: its transfers ask again
    Olvida {
        /// The phone's name, as `picha confianza` lists it
        phone: String,
    },
}

#[derive(Clone, Copy)]
struct Lang(bool);

impl Lang {
    fn t(self, es: &'static str, en: &'static str) -> &'static str {
        if self.0 { es } else { en }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = Config::load();
    let lang = Lang(config.spanish());
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    // in a terminal: the menu, or sending; without one (Hyprland's startup): receiving
    if cli.cmd.is_none() && (interactive || !cli.files.is_empty() || cli.text.is_some()) {
        let start = match cli.text {
            Some(text) => tui::Start::Text(text),
            None if cli.files.is_empty() => tui::Start::Menu,
            None => tui::Start::Send(cli.files),
        };
        return match tui::run(tui::Lang(lang.0), start).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("picha: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let result = match cli.cmd.unwrap_or(Cmd::Recibe { dir: None }) {
        Cmd::Recibe { dir } => {
            log_to_file();
            receive_forever(&config, dir.unwrap_or_else(|| config.dir())).await
        }
        Cmd::Pantalla { addr, empareja, audio, apagada, ya } => pantalla(Lang(config.spanish()), addr, empareja, audio, apagada, ya).await,
        Cmd::Confianza => {
            for key in &config.trusted {
                println!("{}", Config::label(key));
            }
            Ok(())
        }
        Cmd::Olvida { phone } => {
            let mut config = config;
            if !config.forget(&phone) {
                eprintln!("picha: \"{phone}\" {}", Lang(config.spanish()).t("no estaba en confianza", "wasn't trusted"));
            }
            config.save().map_err(Error::from)
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("picha: {e}");
            // from a keybind nobody sees stderr
            if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
                let _ = std::process::Command::new("notify-send")
                    .args(["-a", "picha", "-i", "smartphone-symbolic", "picha", &e.to_string()])
                    .status();
            }
            ExitCode::FAILURE
        }
    }
}

/// Started by Hyprland (no terminal), the receiver's output would go nowhere:
/// it goes to ~/.local/state/picha/recibe.log instead (the last ~1 MB, plus .old).
fn log_to_file() {
    use std::os::fd::AsRawFd;
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return;
    }
    let dir = picha::state::dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("recibe.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1_000_000) {
        let _ = std::fs::rename(&path, dir.join("recibe.log.old"));
    }
    if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        // SAFETY: dup2 on fds this process owns; the file stays open through 1 and 2
        unsafe {
            libc::dup2(file.as_raw_fd(), 1);
            libc::dup2(file.as_raw_fd(), 2);
        }
    }
}

async fn receive_forever(config: &Config, dir: PathBuf) -> Result<()> {
    let lang = Lang(config.spanish());
    tokio::fs::create_dir_all(&dir).await?;
    // Dual-stack: phones often dial the IPv6 address mDNS also publishes.
    let listener = TcpListener::bind(("::", config.port())).await?;
    let port = listener.local_addr()?.port();
    let me = Endpoint { name: config.name(), kind: DeviceKind::Laptop };
    let id = wire::stable_endpoint_id(&format!("{}/{}", Config::hostname(), me.name));
    let advertise = || mdns::advertise(&me, &id, &Config::hostname(), port);
    let mut advert = if picha::state::hidden() { None } else { Some(advertise()?) };
    let mut check = tokio::time::interval(Duration::from_secs(2));
    let _mark = picha::state::ReceiverMark::new();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    println!(
        "picha: {} \"{}\" (:{port}) · {} {}",
        lang.t("visible como", "visible as"),
        me.name,
        lang.t("guardando en", "saving to"),
        dir.display()
    );
    // Bluetooth is a bonus: without obexd picha still receives over Wi-Fi.
    let (bt_events, bt_rx) = mpsc::channel(64);
    let _bluetooth = match picha::bt::receive(dir.clone(), bt_events).await {
        Ok(receiver) => {
            println!("picha: {}", lang.t("también por Bluetooth (celulares emparejados)", "also over Bluetooth (paired phones)"));
            tokio::spawn(serve_events(bt_rx, lang));
            Some(receiver)
        }
        Err(e) => {
            eprintln!("picha: {e} · {}", lang.t("sin Bluetooth (¿falta bluez-obex?)", "no Bluetooth (is bluez-obex installed?)"));
            None
        }
    };
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                println!("picha: {} {peer}", lang.t("conexión de", "connection from"));
                tokio::spawn(one_transfer(stream, dir.clone(), lang));
            }
            // Returning drops the advert, which tells phones picha is gone.
            // the menu's hide switch
            _ = check.tick() => match (picha::state::hidden(), advert.is_some()) {
                (true, true) => advert = None,
                (false, false) => advert = advertise().ok(),
                _ => {}
            },
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = terminate.recv() => return Ok(()),
        }
    }
}

async fn one_transfer(stream: TcpStream, dir: PathBuf, lang: Lang) {
    let (tx, events) = mpsc::channel(64);
    let task = tokio::spawn(async move { receive::handle(stream, &dir, tx).await.map(|()| dir) });
    let offered = serve_events(events, lang).await;
    match task.await {
        Ok(Ok(_)) | Ok(Err(Error::Rejected)) => {}
        Ok(Err(e)) if !offered => eprintln!("picha: {e} {}", lang.t("(antes de ofrecer nada)", "(before offering anything)")),
        Ok(Err(e)) => {
            eprintln!("picha: {e}");
            notify(lang.t("No se pudo recibir", "Couldn't receive"), &e.to_string(), &[]).await;
        }
        Err(e) => eprintln!("picha: {e}"),
    }
}

/// Answers offers and reports progress for one source of events; returns whether
/// anything was offered.
async fn serve_events(mut events: mpsc::Receiver<Event>, lang: Lang) -> bool {
    let mut shown = 0;
    let mut offered = false;
    let mut from = String::new();
    // for the bar, while files come in
    let mut activity: Option<picha::state::Activity> = None;
    while let Some(event) = events.recv().await {
        match event {
            Event::Offer(offer, answer) => {
                offered = true;
                shown = 0;
                from = offer.from.name.clone();
                let accepted = if picha::state::hidden() {
                    false
                } else if offer.trust_key().is_some_and(|k| Config::load().is_trusted(&k)) {
                    println!("picha: {} · {}", offer.from.name, lang.t("de confianza, aceptado solo", "trusted, accepted automatically"));
                    true
                } else {
                    match ask(&offer, lang).await {
                        Answer::Always => {
                            if let Some(key) = offer.trust_key() {
                                trust(&key, lang);
                            }
                            true
                        }
                        Answer::Accept => true,
                        Answer::Reject => false,
                    }
                };
                println!("picha: {}", if accepted { lang.t("aceptado", "accepted") } else { lang.t("rechazado", "rejected") });
                activity = accepted.then(|| picha::state::Activity::new(true, &from));
                let _ = answer.send(accepted);
            }
            Event::Progress { received, total } => {
                if let Some(a) = activity.as_mut() {
                    a.progress(received, total);
                }
                let pct = (received * 100).checked_div(total).unwrap_or(100);
                if pct >= shown + 10 || (pct == 100 && shown < 100) {
                    shown = pct;
                    println!("picha: {pct}% ({})", size(received));
                }
            }
            // Their notifications wait for a click; don't hold the connection for them.
            Event::Files(paths) => {
                activity = None;
                picha::state::record(&from, &paths);
                tokio::spawn(files_arrived(paths, lang));
            }
            Event::Ended => activity = None,
            Event::Text(kind, text) => {
                activity = None;
                tokio::spawn(text_arrived(kind, text, lang));
            }
        }
    }
    offered
}

/// `picha pantalla`: pairing (if asked) in plain prompts, then the TUI that finds
/// the phone and opens its screen when the user picks it; `--ya` skips the TUI.
async fn pantalla(lang: Lang, addr: Option<String>, empareja: bool, audio: bool, screen_off: bool, ya: bool) -> Result<()> {
    use picha::look::{Look, Tone};
    use picha::pantalla;

    if empareja {
        let look = Look::load();
        println!(" {}", look.paint(Tone::Muted, lang.t(
            "en el cel: Opciones de desarrollador → Depuración inalámbrica → Vincular con código…",
            "on the phone: Developer options → Wireless debugging → Pair device with pairing code…",
        )));
        let found = pantalla::find(pantalla::PAIRING, Duration::from_secs(90))?;
        let pair_addr = found.ok_or_else(|| Error::Protocol(lang.t("no apareció el cel", "the phone didn't show up").into()))?;
        print!(" {} ", look.paint(Tone::Primary, lang.t("código de 6 dígitos:", "6-digit code:")));
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut code = String::new();
        std::io::stdin().read_line(&mut code)?;
        pantalla::pair(&pair_addr, code.trim())?;
        println!(" {}", look.paint(Tone::Accent, lang.t("emparejado", "paired")));
    }

    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    if !ya && interactive {
        return Ok(tui::run(tui::Lang(lang.0), tui::Start::Screen { addr, audio, screen_off }).await?);
    }
    let link = pantalla::reach(addr).await.map_err(|why| {
        Error::Protocol(why.unwrap_or_else(|| lang.t("no encontré el cel", "no phone found").into()))
    })?;
    let info = pantalla::info(&link.serial);
    if pantalla::open(&link.serial, &info.name, audio, screen_off) {
        Ok(())
    } else {
        Err(Error::Protocol("scrcpy (sudo pacman -S scrcpy)".into()))
    }
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{} KB", b >> 10),
        b => format!("{b} B"),
    }
}

/// Shows a notification; with actions, waits and returns the one picked.
async fn notify(title: &str, body: &str, actions: &[(&str, &str)]) -> Option<String> {
    notify_for(title, body, actions, INFO_TIMEOUT).await
}

async fn notify_for(title: &str, body: &str, actions: &[(&str, &str)], timeout: Duration) -> Option<String> {
    let mut cmd = Command::new("notify-send");
    cmd.args(["-a", "picha", "-i", "smartphone-symbolic", "-t", &timeout.as_millis().to_string()]);
    for (name, label) in actions {
        cmd.arg(format!("--action={name}={label}"));
    }
    cmd.args([title, body]).stdin(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(timeout, cmd.output()).await.ok()?.ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

enum Answer {
    Accept,
    /// Accept, and from now on accept this phone without asking.
    Always,
    Reject,
}

fn trust(phone: &str, lang: Lang) {
    let mut config = Config::load();
    if !config.is_trusted(phone) {
        config.trusted.push(phone.to_string());
        match config.save() {
            Ok(()) => println!("picha: \"{}\" {}", Config::label(phone), lang.t("ahora es de confianza", "is now trusted")),
            Err(e) => eprintln!("picha: {e}"),
        }
    }
}

async fn ask(offer: &Offer, lang: Lang) -> Answer {
    let what = match (offer.files.as_slice(), offer.text) {
        ([one], _) => one.name.clone(),
        ([], Some(TextKind::Url)) => lang.t("un enlace", "a link").into(),
        ([], Some(_)) => lang.t("texto", "some text").into(),
        (files, _) => format!("{} {}", files.len(), lang.t("archivos", "files")),
    };
    let total: u64 = offer.files.iter().map(|f| f.size).sum();
    let title = format!("{} {} {what}", offer.from.name, lang.t("te picha", "wants to send"));
    let mut parts = Vec::new();
    if !offer.files.is_empty() {
        parts.push(size(total));
    }
    match &offer.pin {
        Some(pin) => parts.push(format!("PIN {pin}")),
        None => parts.push("Bluetooth".into()),
    }
    let body = parts.join(" · ");
    println!("picha: {title} ({body})");
    // short: arre puts the buttons on one line and drops what doesn't fit
    // "Always" only where the phone can be told apart (see net.rs)
    let mut actions = vec![("accept", lang.t("Aceptar", "Accept")), ("reject", lang.t("Rechazar", "Reject"))];
    if offer.trust_key().is_some() {
        actions.push(("always", lang.t("Siempre", "Always")));
    }
    match notify_for(&title, &body, &actions, ASK_TIMEOUT).await.as_deref() {
        Some("accept") => Answer::Accept,
        Some("always") => Answer::Always,
        _ => Answer::Reject,
    }
}

async fn files_arrived(paths: Vec<PathBuf>, lang: Lang) {
    let Some(first) = paths.first() else { return };
    let title = match paths.len() {
        1 => format!("{} {}", lang.t("Llegó", "Received"), first.file_name().unwrap_or_default().to_string_lossy()),
        n => format!("{} {n} {}", lang.t("Llegaron", "Received"), lang.t("archivos", "files")),
    };
    let folder = first.parent().map(PathBuf::from).unwrap_or_default();
    println!("picha: {title} → {}", folder.display());
    let picked = notify(&title, &folder.display().to_string(), &[("open", lang.t("Abrir carpeta", "Open folder"))]).await;
    if picked.as_deref() == Some("open") {
        let _ = Command::new("xdg-open").arg(&folder).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
}

async fn copy(text: &str) -> bool {
    let Ok(mut child) = Command::new("wl-copy").stdin(Stdio::piped()).spawn() else { return false };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes()).await;
    }
    child.wait().await.is_ok_and(|s| s.success())
}

async fn text_arrived(kind: TextKind, text: String, lang: Lang) {
    println!("picha: {text}");
    if kind == TextKind::Url {
        let actions = [("open", lang.t("Abrir", "Open")), ("copy", lang.t("Copiar", "Copy"))];
        match notify(lang.t("Llegó un enlace", "Received a link"), &text, &actions).await.as_deref() {
            Some("open") => {
                let _ = Command::new("xdg-open").arg(&text).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
            }
            Some("copy") => {
                copy(&text).await;
            }
            _ => {}
        }
        return;
    }
    let title = if copy(&text).await {
        lang.t("Texto copiado al portapapeles", "Text copied to the clipboard")
    } else {
        lang.t("Llegó texto", "Received text")
    };
    notify(title, &text, &[]).await;
}
