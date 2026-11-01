//! Receiving over Bluetooth: picha is the agent BlueZ's obexd asks before it accepts a
//! file a paired phone pushes (Share → Bluetooth). No network needed.

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use zbus::zvariant::OwnedObjectPath;
use zbus::{fdo, interface, Connection, Proxy};

use crate::crypto::random_bytes;
use crate::error::{protocol, Result};
use crate::receive::{safe_name, unique_path, Event, Offer, OfferedFile, Via};
use crate::wire::{DeviceKind, Endpoint};

const OBEX: &str = "org.bluez.obex";
const AGENT_PATH: &str = "/picha/obex_agent";

struct Agent {
    session: Connection,
    system: Option<Connection>,
    dir: PathBuf,
    events: mpsc::Sender<Event>,
}

async fn property<T>(conn: &Connection, dest: &'static str, path: &str, iface: &'static str, name: &str) -> zbus::Result<T>
where
    T: TryFrom<zbus::zvariant::OwnedValue>,
    T::Error: Into<zbus::Error>,
{
    Proxy::new(conn, dest, path.to_string(), iface).await?.get_property(name).await
}

impl Agent {
    /// The phone's name as BlueZ knows it, else its address.
    async fn phone_name(&self, address: &str) -> String {
        let Some(system) = &self.system else { return address.to_string() };
        let path = format!("/org/bluez/hci0/dev_{}", address.replace(':', "_"));
        property(system, "org.bluez", &path, "org.bluez.Device1", "Alias").await.unwrap_or_else(|_| address.to_string())
    }
}

#[interface(name = "org.bluez.obex.Agent1")]
impl Agent {
    /// Returns where obexd should store the file, or an error to reject it.
    async fn authorize_push(&self, transfer: OwnedObjectPath) -> fdo::Result<String> {
        if crate::state::hidden() {
            return Err(fdo::Error::AccessDenied("hidden".into()));
        }
        let path = transfer.as_str();
        let name: String = property(&self.session, OBEX, path, "org.bluez.obex.Transfer1", "Name").await.unwrap_or_default();
        let size: u64 = property(&self.session, OBEX, path, "org.bluez.obex.Transfer1", "Size").await.unwrap_or(0);
        let session: OwnedObjectPath = property(&self.session, OBEX, path, "org.bluez.obex.Transfer1", "Session")
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        let address: String = property(&self.session, OBEX, session.as_str(), "org.bluez.obex.Session1", "Destination")
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;

        let name = safe_name(&name);
        let offer = Offer {
            from: Endpoint { name: self.phone_name(&address).await, kind: DeviceKind::Phone },
            pin: None,
            files: vec![OfferedFile { name: name.clone(), size, mime: String::new() }],
            text: None,
            via: Via::Bluetooth(address),
        };
        let (tx, rx) = oneshot::channel();
        self.events.send(Event::Offer(offer, tx)).await.map_err(|_| fdo::Error::Failed("nobody to ask".into()))?;
        if !rx.await.unwrap_or(false) {
            return Err(fdo::Error::AccessDenied("rejected".into()));
        }

        // obexd writes into its own folder; a unique name there, moved into place when done.
        let staging = format!("picha-{}-{name}", u32::from_le_bytes(random_bytes::<4>()));
        tokio::spawn(finish(self.session.clone(), transfer, name, size, self.dir.clone(), self.events.clone()));
        Ok(staging)
    }

    async fn cancel(&self) {}

    async fn release(&self) {}
}

/// Waits for the transfer to end, then moves the file into the download folder.
async fn finish(session: Connection, transfer: OwnedObjectPath, name: String, size: u64, dir: PathBuf, events: mpsc::Sender<Event>) {
    let path = transfer.as_str();
    let mut stored: Option<String> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if stored.is_none() {
            stored = property(&session, OBEX, path, "org.bluez.obex.Transfer1", "Filename").await.ok();
        }
        let status: zbus::Result<String> = property(&session, OBEX, path, "org.bluez.obex.Transfer1", "Status").await;
        let transferred: u64 = property(&session, OBEX, path, "org.bluez.obex.Transfer1", "Transferred").await.unwrap_or(0);
        if size > 0 {
            let _ = events.try_send(Event::Progress { received: transferred, total: size });
        }
        match status.as_deref() {
            Ok("complete") => break,
            Ok("error") => {
                if let Some(f) = &stored {
                    let _ = tokio::fs::remove_file(f).await;
                }
                return;
            }
            Ok(_) => {}
            // obexd drops the transfer object right after it ends: judge by the file.
            Err(_) => {
                let len = match &stored {
                    Some(f) => tokio::fs::metadata(f).await.map(|m| m.len()).ok(),
                    None => None,
                };
                if len.is_some_and(|l| size == 0 || l == size) {
                    break;
                }
                if let Some(f) = &stored {
                    let _ = tokio::fs::remove_file(f).await;
                }
                return;
            }
        }
    }
    let Some(from) = stored else { return };
    let to = unique_path(&dir, &name);
    // rename fails across filesystems; copy then remove covers that.
    let moved = match tokio::fs::rename(&from, &to).await {
        Ok(()) => Ok(()),
        Err(_) => tokio::fs::copy(&from, &to).await.and(tokio::fs::remove_file(&from).await),
    };
    if moved.is_ok() {
        let _ = events.send(Event::Files(vec![to])).await;
    }
}

/// Keeps picha registered with obexd while alive.
pub struct Receiver {
    _session: Connection,
}

/// Registers picha as obexd's agent. Fails when BlueZ's OBEX service isn't installed.
pub async fn receive(dir: PathBuf, events: mpsc::Sender<Event>) -> Result<Receiver> {
    let err = |e: zbus::Error| protocol(format!("Bluetooth: {e}"));
    let system = Connection::system().await.ok();
    let session = Connection::session().await.map_err(err)?;
    let agent = Agent { session: session.clone(), system, dir, events };
    session.object_server().at(AGENT_PATH, agent).await.map_err(err)?;
    let manager = Proxy::new(&session, OBEX, "/org/bluez/obex", "org.bluez.obex.AgentManager1").await.map_err(err)?;
    let path = OwnedObjectPath::try_from(AGENT_PATH).expect("valid object path");
    manager.call::<_, _, ()>("RegisterAgent", &(path,)).await.map_err(err)?;
    Ok(Receiver { _session: session })
}

/// A paired phone that takes Bluetooth file pushes.
#[derive(Debug, Clone)]
pub struct Phone {
    pub name: String,
    pub address: String,
    pub connected: bool,
    /// What it reports over Bluetooth (hands-free battery level), when it does.
    pub battery: Option<u8>,
}

/// Paired devices offering OBEX Object Push, the connected ones first.
pub async fn phones() -> Vec<Phone> {
    let Ok(system) = Connection::system().await else { return Vec::new() };
    let Ok(om) = fdo::ObjectManagerProxy::builder(&system).destination("org.bluez").and_then(|b| b.path("/")) else {
        return Vec::new();
    };
    let Ok(om) = om.build().await else { return Vec::new() };
    let Ok(objects) = om.get_managed_objects().await else { return Vec::new() };
    let mut found: Vec<Phone> = objects
        .into_values()
        .filter_map(|ifaces| {
            let dev = ifaces.get("org.bluez.Device1")?;
            let get = |k: &str| dev.get(k);
            let paired = get("Paired").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
            let uuids: Vec<String> = get("UUIDs").and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok()).unwrap_or_default();
            if !paired || !uuids.iter().any(|u| u.starts_with("00001105")) {
                return None;
            }
            let text = |k: &str| get(k).and_then(|v| String::try_from(v.try_clone().ok()?).ok()).unwrap_or_default();
            let battery = ifaces.get("org.bluez.Battery1").and_then(|b| b.get("Percentage")).and_then(|v| u8::try_from(v).ok());
            Some(Phone {
                name: text("Alias"),
                address: text("Address"),
                connected: get("Connected").and_then(|v| bool::try_from(v).ok()).unwrap_or(false),
                battery,
            })
        })
        .collect();
    found.sort_by_key(|p| !p.connected);
    found
}

/// Progress of a send: (file index, bytes of that file sent, its size).
pub type SendProgress = (usize, u64, u64);

/// Pushes `files` to a paired phone over Bluetooth, one after another. The phone
/// asks its user to accept each one.
/// complete → Some(true), error → Some(false), still going → None.
fn finished(status: &str) -> Option<bool> {
    match status {
        "complete" => Some(true),
        "error" => Some(false),
        _ => None,
    }
}

/// The Status in a PropertiesChanged signal from the transfer at `path`.
fn transfer_status(msg: &zbus::Message, path: &str) -> Option<String> {
    use std::collections::HashMap;
    if msg.header().path().map(|p| p.as_str()) != Some(path) {
        return None;
    }
    let (iface, changed, _): (String, HashMap<String, zbus::zvariant::OwnedValue>, Vec<String>) = msg.body().deserialize().ok()?;
    if iface != "org.bluez.obex.Transfer1" {
        return None;
    }
    String::try_from(changed.get("Status")?.try_clone().ok()?).ok()
}

pub async fn send(address: &str, files: &[PathBuf], progress: mpsc::Sender<SendProgress>) -> Result<()> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;
    let err = |e: zbus::Error| protocol(format!("Bluetooth: {e}"));
    let session = Connection::session().await.map_err(err)?;
    let client = Proxy::new(&session, OBEX, "/org/bluez/obex", "org.bluez.obex.Client1").await.map_err(err)?;
    let mut args: HashMap<&str, Value> = HashMap::new();
    args.insert("Target", Value::from("opp"));
    let obex_session: OwnedObjectPath = client.call("CreateSession", &(address, args)).await.map_err(err)?;
    // obexd removes a transfer the moment it ends, cancelled or not, so its last Status
    // only reaches us as a PropertiesChanged signal: listen before the first send.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(OBEX)
        .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
        .and_then(|b| b.member("PropertiesChanged"))
        .and_then(|b| b.path_namespace(obex_session.as_str()))
        .map(|b| b.build())
        .map_err(err)?;
    let mut signals = zbus::MessageStream::for_match_rule(rule, &session, Some(64)).await.map_err(err)?;
    let result = async {
        let push = Proxy::new(&session, OBEX, obex_session.as_str(), "org.bluez.obex.ObjectPush1").await.map_err(err)?;
        for (i, file) in files.iter().enumerate() {
            let path = std::fs::canonicalize(file).map_err(crate::error::Error::Io)?;
            let (transfer, _props): (OwnedObjectPath, HashMap<String, zbus::zvariant::OwnedValue>) =
                push.call("SendFile", &(path.to_string_lossy().as_ref(),)).await.map_err(err)?;
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let t = transfer.as_str();
            let mut sent = 0u64;
            let done = loop {
                tokio::select! {
                    msg = futures_util::StreamExt::next(&mut signals) => {
                        if let Some(done) = msg.and_then(|m| m.ok()).and_then(|m| transfer_status(&m, t)).and_then(|s| finished(&s)) {
                            break done;
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(300)) => {
                        let status: zbus::Result<String> = property(&session, OBEX, t, "org.bluez.obex.Transfer1", "Status").await;
                        sent = property(&session, OBEX, t, "org.bluez.obex.Transfer1", "Transferred").await.unwrap_or(sent);
                        let _ = progress.try_send((i, sent, size));
                        match status.as_deref().map(finished) {
                            Ok(Some(done)) => break done,
                            Ok(None) => {}
                            // gone: its last word is already queued as a signal, if it said one
                            Err(_) => {
                                let mut last = None;
                                while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_millis(300), futures_util::StreamExt::next(&mut signals)).await {
                                    if let Some(status) = msg.ok().and_then(|m| transfer_status(&m, t)) {
                                        last = finished(&status);
                                    }
                                }
                                break last.unwrap_or(sent >= size);
                            }
                        }
                    }
                }
            };
            if !done {
                return Err(protocol("cancelled or declined on the phone"));
            }
            let _ = progress.try_send((i, size, size));
        }
        Ok(())
    }
    .await;
    let _ = client.call::<_, _, ()>("RemoveSession", &(obex_session,)).await;
    result
}
