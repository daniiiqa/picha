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

/// Lights it: leaves the current Wi-Fi (remembered for `stop`) and opens the lumbre.
pub fn start() -> Result<Lumbre> {
    let card = wifi().ok_or_else(|| protocol("no Wi-Fi card"))?;
    if let Some(net) = &card.network {
        let _ = std::fs::create_dir_all(crate::state::dir());
        let _ = std::fs::write(previous_file(), net);
    }
    let l = Lumbre::get();
    nmcli(&[
        "device", "wifi", "hotspot", "ifname", &card.device, "con-name", CONNECTION, "ssid", &l.ssid, "band", "bg", "password", &l.password,
    ])?;
    Ok(l)
}

/// Puts it out and goes back to the Wi-Fi the laptop was on.
pub fn stop() -> Result<()> {
    if is_on() {
        nmcli(&["connection", "down", CONNECTION])?;
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
