//! `lumbre`: when there's no network, the laptop makes one. A Wi-Fi hotspot through
//! NetworkManager (no root), on 2.4 GHz: Intel cards may not start a network on most
//! 5 GHz channels. Turning it off reconnects the Wi-Fi the laptop was on.

use std::process::{Command, Stdio};

use crate::config::Config;
use crate::crypto::random_bytes;
use crate::error::{protocol, Result};

const CONNECTION: &str = "picha-lumbre";

fn nmcli(args: &[&str]) -> Result<String> {
    let out = Command::new("nmcli")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| protocol(format!("nmcli: {e} (NetworkManager)")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(protocol(String::from_utf8_lossy(&out.stderr).trim().to_string()))
    }
}

/// The Wi-Fi card, and the network it's on (if any, and if not the lumbre).
#[derive(Debug, Clone)]
pub struct Wifi {
    pub device: String,
    pub network: Option<String>,
}

pub fn wifi() -> Option<Wifi> {
    nmcli(&["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "device"]).ok()?.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.get(1) == Some(&"wifi")).then(|| Wifi {
            device: f[0].to_string(),
            network: (f.get(2) == Some(&"connected") && f.get(3) != Some(&CONNECTION)).then(|| f[3].to_string()),
        })
    })
}

/// The network's name and password.
#[derive(Debug, Clone)]
pub struct Lumbre {
    pub ssid: String,
    pub password: String,
}

impl Lumbre {
    /// Its name and password, made once and kept in the config.
    pub fn get() -> Self {
        let mut config = Config::load();
        if config.lumbre_password.is_empty() {
            const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
            config.lumbre_password = random_bytes::<10>().iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
            let _ = config.save();
        }
        Lumbre { ssid: format!("{}-picha", config.name()), password: config.lumbre_password }
    }

    /// What a phone camera reads to join (the Wi-Fi QR format).
    pub fn qr_text(&self) -> String {
        let esc = |s: &str| s.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace(':', "\\:");
        format!("WIFI:T:WPA;S:{};P:{};;", esc(&self.ssid), esc(&self.password))
    }
}

pub fn is_on() -> bool {
    nmcli(&["-t", "-f", "NAME", "connection", "show", "--active"]).is_ok_and(|o| o.lines().any(|l| l == CONNECTION))
}

fn previous_file() -> std::path::PathBuf {
    crate::state::dir().join("lumbre-antes")
}

fn radio_off_file() -> std::path::PathBuf {
    crate::state::dir().join("lumbre-sin-radio")
}

/// Whether airplane mode (an rfkill soft block) has the Wi-Fi radio off.
fn radio_blocked() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/rfkill") else { return false };
    dir.flatten().any(|e| {
        let read = |f: &str| std::fs::read_to_string(e.path().join(f)).unwrap_or_default();
        read("type").trim() == "wlan" && read("soft").trim() == "1"
    })
}

fn rfkill(action: &str) -> Result<()> {
    let ok = Command::new("rfkill").args([action, "wifi"]).stdin(Stdio::null()).status().is_ok_and(|s| s.success());
    if ok { Ok(()) } else { Err(protocol(format!("rfkill {action} wifi failed"))) }
}

/// Waits (up to 10 s) for NetworkManager to see the card again after the radio comes on.
fn wait_for_card() -> Option<Wifi> {
    for _ in 0..40 {
        let available = nmcli(&["-t", "-f", "TYPE,STATE", "device"])
            .is_ok_and(|o| o.lines().any(|l| l.starts_with("wifi:") && !l.ends_with(":unavailable")));
        if available {
            return wifi();
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    None
}

/// Lights it: leaves the current Wi-Fi (remembered for `stop`) and opens the lumbre.
/// Airplane mode is the usual way to be "without network", so if the radio is off it
/// turns it on for the lumbre (and `stop` turns it back off).
pub fn start() -> Result<Lumbre> {
    // NetworkManager's hotspot hands out addresses with dnsmasq; without it the hotspot
    // comes up and then fails at "IP config", after the Wi-Fi was already dropped.
    let has_dnsmasq = std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).chain(["/usr/bin".into(), "/usr/sbin".into()]).any(|d| d.join("dnsmasq").is_file()));
    if !has_dnsmasq {
        return Err(protocol("lumbre needs dnsmasq: sudo pacman -S dnsmasq"));
    }
    let _ = std::fs::create_dir_all(crate::state::dir());
    // leftovers from an attempt that died halfway
    let _ = std::fs::remove_file(radio_off_file());
    let _ = std::fs::remove_file(previous_file());
    if radio_blocked() {
        rfkill("unblock")?;
        let _ = std::fs::write(radio_off_file(), "");
        wait_for_card().ok_or_else(|| protocol("the Wi-Fi card didn't come back after leaving airplane mode"))?;
    }
    let card = wifi().ok_or_else(|| protocol("no Wi-Fi card"))?;
    if let Some(net) = &card.network {
        let _ = std::fs::write(previous_file(), net);
    }
    let l = Lumbre::get();
    // What `nmcli device wifi hotspot` makes, plus link-local IPv6: on a network without
    // internet the S21's Quick Share server refuses IPv4 and only its fe80:: address is
    // left, which the laptop can't reach if the hotspot has IPv6 off (NM's default).
    let settings = [
        "connection.interface-name", card.device.as_str(),
        "connection.autoconnect", "no",
        "802-11-wireless.ssid", l.ssid.as_str(),
        "802-11-wireless.mode", "ap",
        "802-11-wireless.band", "bg",
        "802-11-wireless-security.key-mgmt", "wpa-psk",
        "802-11-wireless-security.proto", "rsn",
        "802-11-wireless-security.pairwise", "ccmp",
        "802-11-wireless-security.group", "ccmp",
        "802-11-wireless-security.psk", l.password.as_str(),
        "ipv4.method", "shared",
        "ipv6.method", "link-local",
    ];
    let exists = nmcli(&["-t", "-f", "NAME", "connection", "show"]).is_ok_and(|o| o.lines().any(|n| n == CONNECTION));
    let mut args = if exists {
        vec!["connection", "modify", CONNECTION]
    } else {
        vec!["connection", "add", "type", "wifi", "con-name", CONNECTION]
    };
    args.extend(settings);
    let lit = nmcli(&args).and_then(|_| nmcli(&["connection", "up", CONNECTION, "ifname", &card.device]));
    if let Err(e) = lit {
        let _ = stop(); // back to the Wi-Fi, or to airplane mode
        return Err(e);
    }
    Ok(l)
}

/// Puts it out and goes back to how the laptop was: its Wi-Fi, or airplane mode.
pub fn stop() -> Result<()> {
    if is_on() {
        nmcli(&["connection", "down", CONNECTION])?;
    }
    if radio_off_file().exists() {
        let _ = std::fs::remove_file(radio_off_file());
        let _ = std::fs::remove_file(previous_file());
        return rfkill("block");
    }
    if let Ok(previous) = std::fs::read_to_string(previous_file()) {
        let _ = std::fs::remove_file(previous_file());
        let _ = nmcli(&["connection", "up", previous.trim()]);
    }
    Ok(())
}

/// The QR as text rows, two modules per character cell (▀ ▄ █ for dark modules):
/// draw them dark on a light background, which every phone camera reads.
pub fn qr_lines(text: &str) -> Vec<String> {
    let Ok(code) = qrcode::QrCode::new(text.as_bytes()) else { return Vec::new() };
    let w = code.width();
    let dark = |x: isize, y: isize| {
        x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < w && code[(x as usize, y as usize)] == qrcode::Color::Dark
    };
    // a 2-module quiet zone around it
    let mut rows = Vec::new();
    let mut y = -2isize;
    while y < w as isize + 2 {
        let row: String = (-2..=w as isize + 1)
            .map(|x| match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            })
            .collect();
        rows.push(row);
        y += 2;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_qr_escapes() {
        let l = Lumbre { ssid: "a;b".into(), password: "p:w".into() };
        assert_eq!(l.qr_text(), "WIFI:T:WPA;S:a\\;b;P:p\\:w;;");
        let rows = qr_lines(&l.qr_text());
        assert!(rows.len() > 10 && rows.iter().all(|r| r.chars().count() == rows[0].chars().count()));
    }
}
