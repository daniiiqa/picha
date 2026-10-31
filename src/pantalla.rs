//! `picha pantalla`: the phone's screen on the laptop (scrcpy), over Android's
//! Wireless debugging, or USB when the cable is in. scrcpy brings the clipboard
//! in both directions and drag-and-drop onto the phone; picha finds the phone,
//! pairs it once, and connects it.

use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};

use crate::error::{protocol, Result};

/// Android's Wireless debugging, once paired: where to connect.
pub const CONNECT: &str = "_adb-tls-connect._tcp.local.";
/// While "Pair device with pairing code" is open on the phone.
pub const PAIRING: &str = "_adb-tls-pairing._tcp.local.";

fn adb(args: &[&str]) -> Result<String> {
    let out = Command::new("adb")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| protocol(format!("adb: {e} (sudo pacman -S android-tools)")))?;
    Ok(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// Serials adb can use right now.
pub fn ready_devices() -> Result<Vec<String>> {
    Ok(parse_devices(&adb(&["devices"])?))
}

fn parse_devices(out: &str) -> Vec<String> {
    out.lines()
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let serial = it.next()?;
            (it.next() == Some("device")).then(|| serial.to_string())
        })
        .collect()
}

/// The first `service` on the network, as `ip:port`.
pub fn find(service: &str, wait: Duration) -> Result<Option<String>> {
    let mdns = ServiceDaemon::new().map_err(|e| protocol(format!("mDNS: {e}")))?;
    let events = mdns.browse(service).map_err(|e| protocol(format!("mDNS: {e}")))?;
    let deadline = Instant::now() + wait;
    let mut found = None;
    while let Ok(event) = events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if let ServiceEvent::ServiceResolved(info) = event {
            // IPv4 first: adb handles it everywhere
            let ip = info.get_addresses_v4().into_iter().next().map(IpAddr::V4);
            if let Some(ip) = ip {
                found = Some(format!("{ip}:{}", info.get_port()));
                break;
            }
        }
    }
    let _ = mdns.shutdown();
    Ok(found)
}

/// One-time pairing with the 6-digit code the phone shows.
pub fn pair(addr: &str, code: &str) -> Result<()> {
    let out = adb(&["pair", addr, code])?;
    if out.contains("Successfully paired") {
        Ok(())
    } else {
        Err(protocol(out.trim().to_string()))
    }
}

/// Connects to a paired phone; returns its serial.
pub fn connect(addr: &str) -> Result<String> {
    let out = adb(&["connect", addr])?;
    if out.contains("connected to") {
        Ok(addr.to_string())
    } else {
        Err(protocol(out.trim().to_string()))
    }
}

/// Where the last wireless connection went: tried first next time, since phones
/// don't always announce Wireless debugging over mDNS.
fn last_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    base.join("picha/pantalla")
}

pub fn last_address() -> Option<String> {
    std::fs::read_to_string(last_path()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn remember(addr: &str) {
    let path = last_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{addr}\n"));
}

/// "Daniel's S21", or the model when the phone doesn't say.
pub fn phone_name(serial: &str) -> String {
    let get = |args: &[&str]| adb(&[&["-s", serial, "shell"], args].concat()).ok().map(|s| s.trim().to_string());
    get(&["settings", "get", "global", "device_name"])
        .filter(|s| !s.is_empty() && s != "null")
        .or_else(|| get(&["getprop", "ro.product.model"]))
        .unwrap_or_else(|| serial.to_string())
}

/// scrcpy's arguments for `serial`.
pub fn scrcpy_args(serial: &str, title: &str, audio: bool, screen_off: bool) -> Vec<String> {
    let mut args = vec!["--serial".into(), serial.into(), "--window-title".into(), title.into(), "--stay-awake".into()];
    if !audio {
        // the phone's sound usually comes over Bluetooth already
        args.push("--no-audio".into());
    }
    if screen_off {
        args.push("--turn-screen-off".into());
    }
    args
}

fn shell(serial: &str, cmd: &str) -> String {
    adb(&["-s", serial, "shell", cmd]).unwrap_or_default()
}

/// Connected by cable (`adb devices -l` names a usb: path), not over Wi-Fi.
pub fn is_usb(serial: &str) -> bool {
    adb(&["devices", "-l"])
        .unwrap_or_default()
        .lines()
        .any(|l| l.split_whitespace().next() == Some(serial) && l.contains(" usb:"))
}

/// adb's own mDNS view: (instance, `ip:port`) of each phone announcing Wireless
/// debugging; instances are `adb-<serial number>-<random>`.
fn adb_mdns() -> Vec<(String, String)> {
    adb(&["mdns", "services"])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (name, kind, addr) = (it.next()?, it.next()?, it.next()?);
            kind.starts_with("_adb-tls-connect").then(|| (name.to_string(), addr.to_string()))
        })
        .collect()
}

/// Where the phone's Wireless debugging listens right now (`ip:port`): the serial
/// itself when connected by address, else adb's mDNS list, matched by the phone's
/// serial number. (Samsung leaves service.adb.tls.port empty.)
pub fn wifi_address(serial: &str) -> Option<String> {
    if serial.parse::<std::net::SocketAddr>().is_ok() {
        return Some(serial.to_string());
    }
    let number = shell(serial, "getprop ro.serialno").trim().to_string();
    adb_mdns()
        .into_iter()
        .find(|(name, _)| serial.starts_with(name.as_str()) || (!number.is_empty() && name.contains(&number)))
        .map(|(_, addr)| addr)
}

/// Over the cable: turns Wireless debugging back on (Samsung switches it off on its
/// own) and returns where it listens.
pub fn arm_wireless(serial: &str) -> Option<String> {
    if shell(serial, "settings get global adb_wifi_enabled").trim() != "1" {
        shell(serial, "settings put global adb_wifi_enabled 1");
    }
    // the announcement takes a moment to reach adb
    (0..24).find_map(|_| {
        let addr = wifi_address(serial);
        if addr.is_none() {
            std::thread::sleep(Duration::from_millis(250));
        }
        addr
    })
}

/// Wireless debugging moves to a new port each time it's switched on. Its open
/// ports on the phone's last known IP are few: try adb on each.
pub async fn probe(ip: &str) -> Option<String> {
    use tokio::net::TcpStream;
    use tokio::task::JoinSet;
    use tokio::time::timeout;
    let ip: IpAddr = ip.parse().ok()?;
    let mut open = Vec::new();
    // Android picks it from the ephemeral range
    let ports: Vec<u16> = (30_000..=50_000).collect();
    for batch in ports.chunks(2_000) {
        let mut set = JoinSet::new();
        for &port in batch {
            set.spawn(async move {
                matches!(timeout(Duration::from_millis(400), TcpStream::connect((ip, port))).await, Ok(Ok(_))).then_some(port)
            });
        }
        while let Some(found) = set.join_next().await {
            open.extend(found.ok().flatten());
        }
    }
    open.sort();
    open.into_iter().map(|port| format!("{ip}:{port}")).find(|addr| connect(addr).is_ok())
}

/// How the laptop reaches the phone this time.
#[derive(Debug, Clone)]
pub struct Link {
    pub serial: String,
    pub cable: bool,
    /// Where Wireless debugging listens (remembered for next time).
    pub wifi: Option<String>,
}

/// Finds the phone: a given address; one already connected (by cable, which also
/// re-arms Wireless debugging, or Wi-Fi); the last address; a new port on the last
/// IP; an announcement. `Err(None)`: no phone anywhere; `Err(Some(why))`: adb said no.
pub async fn reach(addr: Option<String>) -> std::result::Result<Link, Option<String>> {
    let err = |e: crate::error::Error| Some(e.to_string());
    let (serial, cable) = match addr {
        Some(addr) => (connect(&addr).map_err(err)?, false),
        None => match ready_devices().map_err(err)?.into_iter().next() {
            Some(serial) => {
                let cable = is_usb(&serial);
                (serial, cable)
            }
            None => {
                let last = last_address();
                let mut found = last.as_deref().and_then(|a| connect(a).ok());
                if found.is_none()
                    && let Some(ip) = last.as_deref().and_then(|a| a.rsplit_once(':')).map(|(ip, _)| ip.to_string())
                {
                    found = probe(&ip).await;
                }
                if found.is_none() {
                    found = find(CONNECT, Duration::from_secs(4)).ok().flatten().and_then(|a| connect(&a).ok());
                }
                (found.ok_or(None)?, false)
            }
        },
    };
    let wifi = if cable { arm_wireless(&serial) } else { wifi_address(&serial) };
    if let Some(w) = &wifi {
        remember(w);
    }
    Ok(Link { serial, cable, wifi })
}

/// Opens the phone's window, detached so the terminal that asked can close.
pub fn open(serial: &str, title: &str, audio: bool, screen_off: bool) -> bool {
    Command::new("setsid")
        .arg("-f")
        .arg("scrcpy")
        .args(scrcpy_args(serial, title, audio, screen_off))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// What the card before the phone's window shows.
#[derive(Debug, Default)]
pub struct Info {
    pub name: String,
    pub model: String,
    pub android: String,
    pub one_ui: Option<String>,
    pub battery: Option<u8>,
    pub charging: bool,
    pub temperature: Option<f32>,
    pub free: Option<String>,
}

pub fn info(serial: &str) -> Info {
    const SEP: &str = "@@picha@@";
    let out = shell(
        serial,
        &[
            "settings get global device_name",
            "getprop ro.product.model",
            "getprop ro.build.version.release",
            "getprop ro.build.version.oneui",
            "dumpsys battery",
            "df -h /data | tail -1",
        ]
        .join(&format!("; echo {SEP}; ")),
    );
    let parts: Vec<&str> = out.split(SEP).map(str::trim).collect();
    let part = |i: usize| parts.get(i).copied().unwrap_or("");
    let battery_field = |key: &str| part(4).lines().find_map(|l| l.trim().strip_prefix(key)).map(str::trim);
    // One UI: 70000 → 7.0
    let one_ui = part(3).parse::<u32>().ok().filter(|v| *v > 0).map(|v| format!("{}.{}", v / 10_000, v / 100 % 100));
    Info {
        name: Some(part(0)).filter(|s| !s.is_empty() && *s != "null").unwrap_or(part(1)).to_string(),
        model: part(1).to_string(),
        android: part(2).to_string(),
        one_ui,
        battery: battery_field("level:").and_then(|v| v.parse().ok()),
        charging: ["AC powered:", "USB powered:", "Wireless powered:"].iter().any(|k| battery_field(k) == Some("true")),
        temperature: battery_field("temperature:").and_then(|v| v.parse::<f32>().ok()).map(|t| t / 10.0),
        // Filesystem Size Used Avail Use% Mounted
        free: part(5).split_whitespace().nth(3).map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_adb_devices() {
        let out = "List of devices attached\nR5CR413PJ8N\tdevice\n192.168.1.68:39555\toffline\nemulator\tunauthorized\n\n";
        assert_eq!(parse_devices(out), ["R5CR413PJ8N"]);
        assert!(parse_devices("List of devices attached\n\n").is_empty());
    }

    #[test]
    fn audio_is_opt_in() {
        let args = scrcpy_args("R5CR", "Daniel's S21", false, true);
        assert!(args.contains(&"--no-audio".to_string()) && args.contains(&"--turn-screen-off".to_string()));
        assert!(!scrcpy_args("R5CR", "S21", true, false).contains(&"--no-audio".to_string()));
    }
}
