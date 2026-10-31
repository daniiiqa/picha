//! Receiving: the laptop as the Quick Share server a phone connects to.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use prost::Message;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use crate::crypto::{SecureChannel, ServerHandshake};
use crate::error::{protocol, Error, Result};
use crate::proto::connections::{payload_transfer_frame::payload_header::PayloadType, v1_frame, OfflineFrame};
use crate::proto::share::{self as proto_share, text_metadata, v1_frame as share_v1};
use crate::wire::{self, share, Assembler, Endpoint};

/// What the phone wants to send, for the accept prompt.
#[derive(Debug, Clone)]
pub struct Offer {
    pub from: Endpoint,
    /// The 4 digits the phone shows too.
    pub pin: String,
    pub files: Vec<OfferedFile>,
    pub text: Option<TextKind>,
}

#[derive(Debug, Clone)]
pub struct OfferedFile {
    pub name: String,
    pub size: u64,
    pub mime: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Text,
    Url,
    Address,
    Phone,
}

#[derive(Debug)]
pub enum Event {
    /// Answer through the sender: true accepts. Dropping it rejects.
    Offer(Offer, oneshot::Sender<bool>),
    Progress { received: u64, total: u64 },
    Files(Vec<PathBuf>),
    Text(TextKind, String),
}

/// The files (by payload id) and text an offer announced, kept until the user answers.
type Offered = (Vec<(i64, OfferedFile)>, Option<(i64, TextKind)>);

struct Incoming {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    written: u64,
}

/// Deletes half-received files if the transfer dies.
struct Partial(Vec<PathBuf>);

impl Drop for Partial {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// A file name from the phone, made safe to join onto the download folder.
fn safe_name(name: &str) -> String {
    let base = Path::new(name).file_name().and_then(|n| n.to_str()).unwrap_or("");
    let clean: String = base.chars().filter(|c| !c.is_control()).collect();
    match clean.trim() {
        "" | "." | ".." => "archivo".into(),
        s => s.to_string(),
    }
}

/// `dir/name`, or `dir/name (1).ext`… if that exists.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    (1..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).expect("some name is free")
}

struct Conn {
    wr: OwnedWriteHalf,
    channel: SecureChannel,
}

impl Conn {
    async fn send(&mut self, frame: &[u8]) -> Result<()> {
        let sealed = self.channel.seal(frame);
        wire::write_frame(&mut self.wr, &sealed).await
    }

    async fn send_share(&mut self, frame: Vec<u8>) -> Result<()> {
        for chunk in wire::bytes_payload(frame) {
            self.send(&chunk).await?;
        }
        Ok(())
    }
}

fn frame_type(frame: &OfflineFrame) -> Option<v1_frame::FrameType> {
    frame.v1.as_ref()?.r#type.and_then(|t| v1_frame::FrameType::try_from(t).ok())
}

/// Runs one incoming connection to the end: handshake, offer, transfer.
pub async fn handle(stream: TcpStream, dir: &Path, events: mpsc::Sender<Event>) -> Result<()> {
    let (mut rd, wr) = stream.into_split();
    // Frames are read on their own task so waiting for the user never cuts one in half.
    let (tx, mut frames) = mpsc::channel::<Result<Vec<u8>>>(32);
    let reader = tokio::spawn(async move {
        loop {
            let frame = wire::read_frame(&mut rd).await;
            let failed = frame.is_err();
            if tx.send(frame).await.is_err() || failed {
                break;
            }
        }
    });
    let result = run(&mut frames, wr, dir, &events).await;
    reader.abort();
    result
}

async fn next(frames: &mut mpsc::Receiver<Result<Vec<u8>>>) -> Result<Vec<u8>> {
    frames.recv().await.unwrap_or_else(|| Err(protocol("connection closed")))
}

async fn run(
    frames: &mut mpsc::Receiver<Result<Vec<u8>>>,
    mut wr: OwnedWriteHalf,
    dir: &Path,
    events: &mpsc::Sender<Event>,
) -> Result<()> {
    // Plaintext: who's calling, then the UKEY2 exchange.
    let request = OfflineFrame::decode(&*next(frames).await?)?;
    if frame_type(&request) != Some(v1_frame::FrameType::ConnectionRequest) {
        return Err(protocol("expected a connection request"));
    }
    let info = request.v1.and_then(|v| v.connection_request).and_then(|r| r.endpoint_info);
    let from = Endpoint::decode(&info.ok_or_else(|| protocol("no endpoint info"))?)?;

    let (handshake, server_init) = ServerHandshake::new(&next(frames).await?)?;
    wire::write_frame(&mut wr, &server_init).await?;
    let (channel, pin) = handshake.finish(&next(frames).await?)?;

    let response = OfflineFrame::decode(&*next(frames).await?)?;
    if frame_type(&response) != Some(v1_frame::FrameType::ConnectionResponse) {
        return Err(protocol("expected a connection response"));
    }
    wire::write_frame(&mut wr, &wire::connection_response()).await?;

    // Encrypted from here on.
    let mut conn = Conn { wr, channel };
    conn.send_share(share::paired_key_encryption()).await?;

    let mut assembler = Assembler::default();
    let mut incoming: HashMap<i64, Incoming> = HashMap::new();
    let mut partial = Partial(Vec::new());
    let mut text: Option<(i64, TextKind)> = None;
    let mut offered: Option<Offered> = None;
    let mut answer: Option<oneshot::Receiver<bool>> = None;
    let (mut received, mut total) = (0u64, 0u64);

    loop {
        let waiting = async {
            match answer.as_mut() {
                Some(rx) => rx.await.unwrap_or(false),
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            accepted = waiting => {
                answer = None;
                let (files, offered_text) = offered.take().expect("an answer follows an offer");
                if !accepted {
                    conn.send_share(share::response(share::Status::Reject)).await?;
                    conn.send(&wire::disconnection()).await?;
                    return Err(Error::Rejected);
                }
                for (id, f) in files {
                    let path = unique_path(dir, &f.name);
                    let file = File::create(&path).await?;
                    partial.0.push(path.clone());
                    total += f.size;
                    incoming.insert(id, Incoming { path, file: Some(file), size: f.size, written: 0 });
                }
                text = offered_text;
                conn.send_share(share::response(share::Status::Accept)).await?;
            }
            raw = next(frames) => {
                let frame = OfflineFrame::decode(&*conn.channel.open(&raw?)?)?;
                match frame_type(&frame) {
                    Some(v1_frame::FrameType::KeepAlive) => {
                        let ack = frame.v1.as_ref().and_then(|v| v.keep_alive.as_ref()).and_then(|k| k.ack);
                        if ack != Some(true) {
                            conn.send(&wire::keep_alive(true)).await?;
                        }
                    }
                    Some(v1_frame::FrameType::Disconnection) => return Err(Error::Cancelled),
                    Some(v1_frame::FrameType::PayloadTransfer) => {
                        let transfer = frame.v1.and_then(|v| v.payload_transfer).ok_or_else(|| protocol("empty payload"))?;
                        let kind = transfer.payload_header.as_ref().and_then(|h| h.r#type);
                        if kind == Some(PayloadType::File as i32) {
                            let chunk = transfer.payload_chunk.unwrap_or_default();
                            let id = transfer.payload_header.and_then(|h| h.id).unwrap_or_default();
                            let f = incoming.get_mut(&id).ok_or_else(|| protocol("file nobody accepted"))?;
                            if chunk.offset != Some(f.written as i64) {
                                return Err(protocol("file chunk out of order"));
                            }
                            let body = chunk.body.unwrap_or_default();
                            if f.written + body.len() as u64 > f.size {
                                return Err(protocol("file bigger than announced"));
                            }
                            if let Some(file) = f.file.as_mut() {
                                file.write_all(&body).await?;
                            }
                            f.written += body.len() as u64;
                            received += body.len() as u64;
                            let _ = events.try_send(Event::Progress { received, total });
                            if chunk.flags.unwrap_or(0) & 1 == 1
                                && let Some(mut file) = f.file.take() {
                                    file.flush().await?;
                                }
                            if incoming.values().all(|f| f.file.is_none()) {
                                partial.0.clear();
                                let paths = incoming.values().map(|f| f.path.clone()).collect();
                                let _ = events.send(Event::Files(paths)).await;
                                conn.send(&wire::disconnection()).await?;
                                return Ok(());
                            }
                        } else if let Some((id, payload)) = assembler.feed(&transfer)? {
                            if let Some((text_id, kind)) = text
                                && id == text_id {
                                    let _ = events.send(Event::Text(kind, String::from_utf8_lossy(&payload).into_owned())).await;
                                    conn.send(&wire::disconnection()).await?;
                                    return Ok(());
                                }
                            let inner = proto_share::Frame::decode(&*payload)?;
                            let v1 = inner.v1.unwrap_or_default();
                            match v1.r#type.and_then(|t| share_v1::FrameType::try_from(t).ok()) {
                                Some(share_v1::FrameType::PairedKeyEncryption) => {
                                    conn.send_share(share::paired_key_result()).await?;
                                }
                                Some(share_v1::FrameType::Introduction) => {
                                    let intro = v1.introduction.unwrap_or_default();
                                    let files: Vec<(i64, OfferedFile)> = intro
                                        .file_metadata
                                        .iter()
                                        .map(|m| (m.payload_id.unwrap_or_default(), OfferedFile {
                                            name: safe_name(m.name.as_deref().unwrap_or_default()),
                                            size: m.size.unwrap_or(0).max(0) as u64,
                                            mime: m.mime_type.clone().unwrap_or_default(),
                                        }))
                                        .collect();
                                    let offered_text = intro.text_metadata.first().and_then(|t| {
                                        let kind = match text_metadata::Type::try_from(t.r#type?).ok()? {
                                            text_metadata::Type::Url => TextKind::Url,
                                            text_metadata::Type::Address => TextKind::Address,
                                            text_metadata::Type::PhoneNumber => TextKind::Phone,
                                            _ => TextKind::Text,
                                        };
                                        Some((t.payload_id?, kind))
                                    });
                                    if files.is_empty() && offered_text.is_none() {
                                        conn.send_share(share::response(share::Status::UnsupportedAttachmentType)).await?;
                                        conn.send(&wire::disconnection()).await?;
                                        return Err(protocol("nothing picha can receive"));
                                    }
                                    let offer = Offer {
                                        from: from.clone(),
                                        pin: pin.clone(),
                                        files: files.iter().map(|(_, f)| f.clone()).collect(),
                                        text: offered_text.map(|(_, k)| k),
                                    };
                                    let (tx, rx) = oneshot::channel();
                                    events.send(Event::Offer(offer, tx)).await.map_err(|_| protocol("nobody to ask"))?;
                                    offered = Some((files, offered_text));
                                    answer = Some(rx);
                                }
                                Some(share_v1::FrameType::Cancel) => return Err(Error::Cancelled),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_the_phone_stay_in_the_folder() {
        assert_eq!(safe_name("foto.jpg"), "foto.jpg");
        assert_eq!(safe_name("../../.bashrc"), ".bashrc");
        assert_eq!(safe_name("/etc/passwd"), "passwd");
        assert_eq!(safe_name(".."), "archivo");
        assert_eq!(safe_name("a\nb.txt"), "ab.txt");
    }

    #[test]
    fn duplicate_names_get_a_number() {
        let dir = std::env::temp_dir().join(format!("picha-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("foto.jpg"), "").unwrap();
        std::fs::write(dir.join("foto (1).jpg"), "").unwrap();
        assert_eq!(unique_path(&dir, "foto.jpg"), dir.join("foto (2).jpg"));
        assert_eq!(unique_path(&dir, "nuevo"), dir.join("nuevo"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
