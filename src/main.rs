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
use picha::wire::{DeviceKind, Endpoint};
use picha::mdns;

/// How long an offer waits for an answer before it is turned down.
const ASK_TIMEOUT: Duration = Duration::from_secs(90);

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
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("picha: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn receive_forever(config: &Config, dir: PathBuf) -> Result<()> {
    let lang = Lang(config.spanish());
    tokio::fs::create_dir_all(&dir).await?;
    let listener = TcpListener::bind("0.0.0.0:0").await?;
    let port = listener.local_addr()?.port();
    let me = Endpoint { name: config.name(), kind: DeviceKind::Laptop };
    let _advert = mdns::advertise(&me, &Config::hostname(), port)?;
    println!(
        "picha: {} \"{}\" (:{port}) · {} {}",
        lang.t("visible como", "visible as"),
        me.name,
        lang.t("guardando en", "saving to"),
        dir.display()
    );
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                println!("picha: {} {peer}", lang.t("conexión de", "connection from"));
                tokio::spawn(one_transfer(stream, dir.clone(), lang));
            }
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
    }
}

async fn one_transfer(stream: TcpStream, dir: PathBuf, lang: Lang) {
    let (tx, mut events) = mpsc::channel(64);
    let task = tokio::spawn(async move { receive::handle(stream, &dir, tx).await.map(|()| dir) });
    let mut shown = 0;
    while let Some(event) = events.recv().await {
        match event {
            Event::Offer(offer, answer) => {
                let accepted = ask(&offer, lang).await;
                println!("picha: {}", if accepted { lang.t("aceptado", "accepted") } else { lang.t("rechazado", "rejected") });
                let _ = answer.send(accepted);
            }
            Event::Progress { received, total } => {
                let pct = (received * 100).checked_div(total).unwrap_or(100);
                if pct >= shown + 10 || pct == 100 {
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
    match task.await {
        Ok(Ok(_)) | Ok(Err(Error::Rejected)) => {}
        Ok(Err(e)) => {
            eprintln!("picha: {e}");
            notify(lang.t("No se pudo recibir", "Couldn't receive"), &e.to_string(), &[]).await;
        }
        Err(e) => eprintln!("picha: {e}"),
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
    let mut cmd = Command::new("notify-send");
    cmd.args(["-a", "picha", "-i", "smartphone-symbolic"]);
    if !actions.is_empty() {
        cmd.args(["-t", &ASK_TIMEOUT.as_millis().to_string()]);
    }
    for (name, label) in actions {
        cmd.arg(format!("--action={name}={label}"));
    }
    cmd.args([title, body]).stdin(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(ASK_TIMEOUT, cmd.output()).await.ok()?.ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

async fn ask(offer: &Offer, lang: Lang) -> bool {
    let what = match (offer.files.as_slice(), offer.text) {
        ([one], _) => one.name.clone(),
        ([], Some(TextKind::Url)) => lang.t("un enlace", "a link").into(),
        ([], Some(_)) => lang.t("texto", "some text").into(),
        (files, _) => format!("{} {}", files.len(), lang.t("archivos", "files")),
    };
    let total: u64 = offer.files.iter().map(|f| f.size).sum();
    let title = format!("{} {} {what}", offer.from.name, lang.t("te picha", "wants to send"));
    let mut body = if offer.files.is_empty() { String::new() } else { format!("{} · ", size(total)) };
    body += &format!("PIN {}", offer.pin);
    println!("picha: {title} ({body})");
    let picked = notify(&title, &body, &[("accept", lang.t("Aceptar", "Accept")), ("reject", lang.t("Rechazar", "Reject"))]).await;
    picked.as_deref() == Some("accept")
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
