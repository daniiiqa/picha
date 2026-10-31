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
