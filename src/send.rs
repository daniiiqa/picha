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

/// Sends `files` to the server at `addr`. `on_pin` gets the PIN as soon as both sides
/// know it, so it can be shown while the other side decides.
pub async fn send(addr: SocketAddr, me: &Endpoint, files: &[PathBuf], on_pin: impl FnOnce(&str)) -> Result<()> {
    let mut stream = TcpStream::connect(addr).await?;
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
                        vec![share::paired_key_result(), share::introduction(&offered)]
                    }
                    Some(share_v1::FrameType::Response) => {
                        let status = inner.connection_response.and_then(|r| r.status);
                        if status != Some(connection_response_frame::Status::Accept as i32) {
                            return Err(Error::Rejected);
                        }
                        for ((id, _, size, _), path) in offered.iter().zip(files) {
                            stream_file(&mut stream, &mut channel, *id, *size, path).await?;
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
    }
    let last = wire::payload_chunk(id, PayloadType::File, size, offset, Vec::new(), true);
    wire::write_frame(stream, &channel.seal(&last)).await
}
