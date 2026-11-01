//! Sending: connecting to a Quick Share server and pitching it files.

use std::net::SocketAddr;
use std::path::PathBuf;

use prost::Message;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use crate::crypto::{random_bytes, ClientHandshake};
use crate::error::{protocol, Error, Result};
use crate::proto::connections::{payload_transfer_frame::payload_header::PayloadType, v1_frame, OfflineFrame};
use crate::proto::share::{self as proto_share, connection_response_frame, v1_frame as share_v1};
use crate::wire::{self, share, Assembler, Endpoint};

const CHUNK: usize = 512 * 1024;

/// (file index, bytes of it sent, its size).
pub type Progress = (usize, u64, u64);

/// Sends `files` and maybe `text` over `stream`, connected to a Quick Share server.
/// A text lands as a notification (and the clipboard) on picha, not as a file. `on_pin` gets the PIN as soon as both sides
/// know it, so it can be shown while the other side decides.
pub async fn send(
    mut stream: TcpStream,
    me: &Endpoint,
    files: &[PathBuf],
    text: Option<&str>,
    on_pin: impl FnOnce(&str),
    progress: Option<tokio::sync::mpsc::Sender<Progress>>,
) -> Result<()> {
    wire::write_frame(&mut stream, &wire::connection_request(&wire::endpoint_id(), me)).await?;
    let (handshake, client_init) = ClientHandshake::new();
    wire::write_frame(&mut stream, &client_init).await?;
    let (mut channel, pin, client_finish) = handshake.finish(&wire::read_frame(&mut stream).await?)?;
    wire::write_frame(&mut stream, &client_finish).await?;
    on_pin(&pin);
    wire::write_frame(&mut stream, &wire::connection_response()).await?;
    let response = OfflineFrame::decode(&*wire::read_frame(&mut stream).await?)?;
    if response.v1.and_then(|v| v.r#type) != Some(v1_frame::FrameType::ConnectionResponse as i32) {
        return Err(protocol("expected a connection response"));
    }

    let mut offered = Vec::new();
    for path in files {
        let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| protocol("file without a name"))?;
        let size = tokio::fs::metadata(path).await?.len() as i64;
        offered.push((i64::from_le_bytes(random_bytes::<8>()), name.to_string(), size, String::new()));
    }

    let text = text.map(|t| (i64::from_le_bytes(random_bytes::<8>()), t));
    let mut assembler = Assembler::default();
    loop {
        let raw = wire::read_frame(&mut stream).await?;
        let frame = OfflineFrame::decode(&*channel.open(&raw)?)?;
        let v1 = frame.v1.unwrap_or_default();
        match v1.r#type.and_then(|t| v1_frame::FrameType::try_from(t).ok()) {
            Some(v1_frame::FrameType::KeepAlive) => {
                if v1.keep_alive.and_then(|k| k.ack) != Some(true) {
                    wire::write_frame(&mut stream, &channel.seal(&wire::keep_alive(true))).await?;
                }
            }
            Some(v1_frame::FrameType::Disconnection) => return Ok(()),
            Some(v1_frame::FrameType::PayloadTransfer) => {
                let transfer = v1.payload_transfer.ok_or_else(|| protocol("empty payload"))?;
                let Some((_, payload)) = assembler.feed(&transfer)? else { continue };
                let inner = proto_share::Frame::decode(&*payload)?.v1.unwrap_or_default();
                let reply = match inner.r#type.and_then(|t| share_v1::FrameType::try_from(t).ok()) {
                    Some(share_v1::FrameType::PairedKeyEncryption) => vec![share::paired_key_encryption()],
                    Some(share_v1::FrameType::PairedKeyResult) => {
                        vec![share::paired_key_result(), share::introduction(&offered, text)]
                    }
                    Some(share_v1::FrameType::Response) => {
                        let status = inner.connection_response.and_then(|r| r.status);
                        if status != Some(connection_response_frame::Status::Accept as i32) {
                            return Err(Error::Rejected);
                        }
                        for (i, ((id, _, size, _), path)) in offered.iter().zip(files).enumerate() {
                            stream_file(&mut stream, &mut channel, *id, *size, path, |sent| {
                                if let Some(p) = &progress {
                                    let _ = p.try_send((i, sent, *size as u64));
                                }
                            })
                            .await?;
                        }
                        if let Some((id, t)) = text {
                            for chunk in wire::bytes_payload_with_id(id, t.as_bytes().to_vec()) {
                                wire::write_frame(&mut stream, &channel.seal(&chunk)).await?;
                            }
                        }
                        Vec::new()
                    }
                    Some(share_v1::FrameType::Cancel) => return Err(Error::Cancelled),
                    _ => Vec::new(),
                };
                for share_frame in reply {
                    for chunk in wire::bytes_payload(share_frame) {
                        wire::write_frame(&mut stream, &channel.seal(&chunk)).await?;
                    }
                }
            }
            _ => {}
        }
    }
}

async fn stream_file(
    stream: &mut TcpStream,
    channel: &mut crate::crypto::SecureChannel,
    id: i64,
    size: i64,
    path: &PathBuf,
    mut sent: impl FnMut(u64),
) -> Result<()> {
    let mut file = File::open(path).await?;
    let mut offset = 0i64;
    let mut buf = vec![0; CHUNK];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let frame = wire::payload_chunk(id, PayloadType::File, size, offset, buf[..n].to_vec(), false);
        wire::write_frame(stream, &channel.seal(&frame)).await?;
        offset += n as i64;
        sent(offset as u64);
    }
    let last = wire::payload_chunk(id, PayloadType::File, size, offset, Vec::new(), true);
    wire::write_frame(stream, &channel.seal(&last)).await
}

/// One line in ~/.local/state/picha/manda.log: what sending tried, for when it fails.
pub fn log(what: &str) {
    use std::io::Write;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let _ = std::fs::create_dir_all(crate::state::dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(crate::state::dir().join("manda.log")) {
        let _ = writeln!(f, "[{}] {what}", now);
    }
}

/// A Quick Share receiver announced on the network: a phone woken by the nudge, or
/// another computer running picha (or Quick Share for Windows).
#[derive(Debug, Clone)]
pub struct Seen {
    /// its mDNS name (phones change it on every wake-up)
    pub key: String,
    /// how it describes itself; hidden phones give none
    pub endpoint: Option<Endpoint>,
    /// IPv4 first, then IPv6 (link-local ones scoped to their interface)
    pub addrs: Vec<SocketAddr>,
}

impl Seen {
    /// A phone or tablet, or one that won't say (Android hides its name until it
    /// has accepted something from us): these go in "the phone" row.
    pub fn is_phone(&self) -> bool {
        self.endpoint.as_ref().is_none_or(|e| matches!(e.kind, crate::wire::DeviceKind::Phone | crate::wire::DeviceKind::Tablet))
    }
}

/// Nudges nearby phones over Bluetooth LE and, for `wait`, reports every Quick
/// Share receiver that shows up on the network (not this laptop's own) to `found`.
/// Stops early when `found` is closed.
pub async fn discover(wait: std::time::Duration, found: tokio::sync::mpsc::UnboundedSender<Seen>) {
    use base64::Engine;
    use mdns_sd::{ServiceDaemon, ServiceEvent};

    let nudge = match crate::ble::nudge().await {
        Ok(n) => {
            log("nudge: advertising fe2c");
            Some(n)
        }
        Err(e) => {
            log(&format!("nudge failed: {e}"));
            None
        }
    };
    let Ok(mdns) = ServiceDaemon::new() else { return };
    let Ok(events) = mdns.browse("_FC9F5ED42C8A._tcp.local.") else { return };
    let me = crate::config::Config::load();
    let deadline = std::time::Instant::now() + wait;
    while !found.is_closed() {
        let left = deadline.saturating_duration_since(std::time::Instant::now()).min(std::time::Duration::from_millis(500));
        if left.is_zero() {
            break;
        }
        let Ok(event) = tokio::task::block_in_place(|| events.recv_timeout(left)) else { continue };
        let ServiceEvent::ServiceResolved(info) = event else { continue };
        let raw = info
            .get_property_val_str("n")
            .and_then(|t| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(t).ok())
            .unwrap_or_default();
        let endpoint = Endpoint::decode(&raw).ok();
        log(&format!(
            "mdns: {} name={:?} addrs={:?} port={}",
            info.get_fullname(),
            endpoint.as_ref().map(|e| e.name.clone()),
            info.get_addresses(),
            info.get_port()
        ));
        if endpoint.as_ref().is_some_and(|e| e.name == me.name()) && info.get_port() == me.port() {
            continue;
        }
        let port = info.get_port();
        let mut addrs: Vec<SocketAddr> =
            info.get_addresses_v4().into_iter().map(|ip| SocketAddr::new(std::net::IpAddr::V4(ip), port)).collect();
        addrs.extend(info.get_addresses().iter().filter_map(|a| match a {
            mdns_sd::ScopedIp::V6(v6) => {
                let scope = if v6.addr().is_unicast_link_local() { v6.scope_id().index } else { 0 };
                Some(SocketAddr::V6(std::net::SocketAddrV6::new(*v6.addr(), port, 0, scope)))
            }
            _ => None,
        }));
        let _ = found.send(Seen { key: info.get_fullname().to_string(), endpoint, addrs });
    }
    let _ = mdns.shutdown();
    if let Some(n) = nudge {
        n.stop().await;
    }
}

/// Why dialing got nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Miss {
    /// Nothing answered: not on the same network, or asleep.
    NotSeen,
    /// It announced itself but refused the connection: Quick Share is off on it.
    /// (A phone can also answer mDNS with a port it no longer serves.)
    Refused,
}

/// The first of `addrs` that takes a connection (2 s each).
pub async fn dial(addrs: &[SocketAddr]) -> std::result::Result<(TcpStream, SocketAddr), Miss> {
    let mut miss = Miss::NotSeen;
    for &addr in addrs {
        match tokio::time::timeout(std::time::Duration::from_secs(2), TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                log(&format!("dial {addr}: connected"));
                return Ok((stream, addr));
            }
            Ok(Err(e)) => {
                if e.kind() == std::io::ErrorKind::ConnectionRefused {
                    miss = Miss::Refused;
                }
                log(&format!("dial {addr}: {e}"));
            }
            Err(_) => log(&format!("dial {addr}: timed out")),
        }
    }
    Err(miss)
}
