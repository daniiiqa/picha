//! `~/.local/state/picha/`: what the background receiver leaves for the menu to
//! show — that it's running, and what arrived lately.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub fn dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"))
        .join("picha")
}

/// Marks the receiver as running (its pid) until `ReceiverMark` drops.
pub struct ReceiverMark;

impl ReceiverMark {
    pub fn new() -> Self {
        let _ = fs::create_dir_all(dir());
        let _ = fs::write(dir().join("recibe.pid"), std::process::id().to_string());
        ReceiverMark
    }
}

impl Default for ReceiverMark {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ReceiverMark {
    fn drop(&mut self) {
        let _ = fs::remove_file(dir().join("recibe.pid"));
    }
}

/// Whether a background receiver is alive.
pub fn receiver_running() -> bool {
    fs::read_to_string(dir().join("recibe.pid"))
        .ok()
        .and_then(|p| p.trim().parse::<u32>().ok())
        .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Received {
    /// Unix seconds.
    pub at: u64,
    pub from: String,
    pub path: PathBuf,
}

fn log() -> PathBuf {
    dir().join("recibidos.jsonl")
}

pub fn record(from: &str, paths: &[PathBuf]) {
    let at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let _ = fs::create_dir_all(dir());
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log()) {
        for path in paths {
            let line = serde_json::to_string(&Received { at, from: from.to_string(), path: path.clone() }).expect("serializes");
            let _ = writeln!(f, "{line}");
        }
    }
}

/// The last `n` received, newest first.
pub fn recent(n: usize) -> Vec<Received> {
    let text = fs::read_to_string(log()).unwrap_or_default();
    text.lines().rev().filter_map(|l| serde_json::from_str(l).ok()).take(n).collect()
}

fn hidden_file() -> PathBuf {
    dir().join("oculto")
}

/// Hidden: the receiver stops announcing itself and turns down Bluetooth files.
pub fn hidden() -> bool {
    hidden_file().exists()
}

pub fn set_hidden(on: bool) {
    let _ = fs::create_dir_all(dir());
    if on {
        let _ = fs::write(hidden_file(), "");
    } else {
        let _ = fs::remove_file(hidden_file());
    }
}

/// A transfer in progress, for the bar (empalme's `picha` module): one file per
/// transfer in $XDG_RUNTIME_DIR/picha, named `<pid>-<n>`, holding one line:
/// `in|out <percent> <who>`. Gone when the transfer ends (dropped).
pub struct Activity {
    path: PathBuf,
    incoming: bool,
    who: String,
    pct: u64,
}

fn activity_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("picha")
}

impl Activity {
    pub fn new(incoming: bool, who: &str) -> Activity {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = fs::create_dir_all(activity_dir());
        let path = activity_dir().join(format!("{}-{n}", std::process::id()));
        // one line, no newlines from a phone's name
        let who = who.replace(['\n', '\r'], " ");
        let a = Activity { path, incoming, who, pct: 0 };
        a.write();
        a
    }

    /// Updates the percentage (written only when it changes).
    pub fn progress(&mut self, done: u64, total: u64) {
        let pct = (done.saturating_mul(100)).checked_div(total).unwrap_or(0).min(100);
        if pct != self.pct {
            self.pct = pct;
            self.write();
        }
    }

    fn write(&self) {
        let tmp = self.path.with_extension("tmp");
        let line = format!("{} {} {}\n", if self.incoming { "in" } else { "out" }, self.pct, self.who);
        if fs::write(&tmp, line).is_ok() {
            let _ = fs::rename(&tmp, &self.path);
        }
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
