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
#[command(name = "picha", version, about = "Quick Share for Linux: your phone te picha files, a la carne asada")]
struct Cli {
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
    let result = match cli.cmd.unwrap_or(Cmd::Recibe { dir: None }) {
        Cmd::Recibe { dir } => receive_forever(&config, dir.unwrap_or_else(|| config.dir())).await,
        Cmd::Pantalla { addr, empareja, audio, apagada } => pantalla(Lang(config.spanish()), addr, empareja, audio, apagada),
        Cmd::Confianza => {
            for phone in &config.trusted {
                println!("{phone}");
            }
            Ok(())
        }
        Cmd::Olvida { phone } => {
            let mut config = config;
            let before = config.trusted.len();
            config.trusted.retain(|t| *t != phone);
            if config.trusted.len() == before {
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

async fn receive_forever(config: &Config, dir: PathBuf) -> Result<()> {
    let lang = Lang(config.spanish());
    tokio::fs::create_dir_all(&dir).await?;
    // Dual-stack: phones often dial the IPv6 address mDNS also publishes.
    let listener = TcpListener::bind(("::", config.port())).await?;
    let port = listener.local_addr()?.port();
    let me = Endpoint { name: config.name(), kind: DeviceKind::Laptop };
    let id = wire::stable_endpoint_id(&format!("{}/{}", Config::hostname(), me.name));
    let _advert = mdns::advertise(&me, &id, &Config::hostname(), port)?;
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
    while let Some(event) = events.recv().await {
        match event {
            Event::Offer(offer, answer) => {
                offered = true;
                shown = 0;
                let accepted = if Config::load().is_trusted(&offer.trust_key()) {
                    println!("picha: {} · {}", offer.from.name, lang.t("de confianza, aceptado solo", "trusted, accepted automatically"));
                    true
                } else {
                    match ask(&offer, lang).await {
                        Answer::Always => {
                            trust(&offer.trust_key(), lang);
                            true
                        }
                        Answer::Accept => true,
                        Answer::Reject => false,
                    }
                };
                println!("picha: {}", if accepted { lang.t("aceptado", "accepted") } else { lang.t("rechazado", "rejected") });
                let _ = answer.send(accepted);
            }
            Event::Progress { received, total } => {
                let pct = (received * 100).checked_div(total).unwrap_or(100);
                if pct >= shown + 10 || (pct == 100 && shown < 100) {
                    shown = pct;
                    println!("picha: {pct}% ({})", size(received));
                }
            }
            // Their notifications wait for a click; don't hold the connection for them.
            Event::Files(paths) => {
                tokio::spawn(files_arrived(paths, lang));
            }
            Event::Text(kind, text) => {
                tokio::spawn(text_arrived(kind, text, lang));
            }
        }
    }
    offered
}

fn pantalla(lang: Lang, addr: Option<String>, empareja: bool, audio: bool, screen_off: bool) -> Result<()> {
    use picha::pantalla;
    use std::os::unix::process::CommandExt;

    if empareja {
        println!(
            "picha: {}",
            lang.t(
                "en el cel: Opciones de desarrollador → Depuración inalámbrica → Vincular con código…",
                "on the phone: Developer options → Wireless debugging → Pair device with pairing code…"
            )
        );
        let addr = pantalla::find(pantalla::PAIRING, Duration::from_secs(90))?
            .ok_or_else(|| picha::error::Error::Protocol(lang.t("no apareció el cel", "the phone didn't show up").into()))?;
        print!("picha: {} ", lang.t("código de 6 dígitos:", "6-digit code:"));
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut code = String::new();
        std::io::stdin().read_line(&mut code)?;
        pantalla::pair(&addr, code.trim())?;
        println!("picha: {}", lang.t("emparejado", "paired"));
    }

    // a given address, the cable or a phone already connected, the last address,
    // then whatever the phone announces
    let ready = pantalla::ready_devices()?.into_iter().next();
    let serial = match (addr, ready) {
        (Some(addr), _) => pantalla::connect(&addr)?,
        (None, Some(serial)) => serial,
        (None, None) => {
            let last = pantalla::last_address().and_then(|a| pantalla::connect(&a).ok());
            match last {
                Some(serial) => serial,
                None => {
                    println!("picha: {}", lang.t("buscando el cel…", "looking for the phone…"));
                    let found = pantalla::find(pantalla::CONNECT, Duration::from_secs(8))?.ok_or_else(|| {
                        picha::error::Error::Protocol(
                            lang.t(
                                "no encontré el cel: prende Depuración inalámbrica y pásame la «Dirección IP y puerto» que enseña (picha pantalla 192.168.1.20:41235), o conecta el cable",
                                "no phone found: turn on Wireless debugging and give me the \"IP address & Port\" it shows (picha pantalla 192.168.1.20:41235), or plug in the cable",
                            )
                            .into(),
                        )
                    })?;
                    pantalla::connect(&found)?
                }
            }
        }
    };
    if serial.contains(':') {
        pantalla::remember(&serial);
    }
    let name = pantalla::phone_name(&serial);
    println!("picha: {} {name}", lang.t("pantalla de", "screen of"));
    let err = std::process::Command::new("scrcpy").args(pantalla::scrcpy_args(&serial, &name, audio, screen_off)).exec();
    Err(picha::error::Error::Protocol(format!("scrcpy: {err}")))
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
            Ok(()) => println!("picha: \"{phone}\" {}", lang.t("ahora es de confianza", "is now trusted")),
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
    let actions = [
        ("accept", lang.t("Aceptar", "Accept")),
        ("always", lang.t("Siempre de este cel", "Always from this phone")),
        ("reject", lang.t("Rechazar", "Reject")),
    ];
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
