//! What goes on the TCP connection: length-prefixed protobufs, the Nearby Connections
//! offline frames, and the endpoint info that names a device.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::crypto::random_bytes;
use crate::error::{protocol, Result};
use crate::proto::connections::{
    connection_request_frame, connection_response_frame, offline_frame, os_info, payload_transfer_frame, v1_frame,
    ConnectionRequestFrame, ConnectionResponseFrame, DisconnectionFrame, KeepAliveFrame, OfflineFrame, OsInfo,
    PayloadTransferFrame, V1Frame,
};

/// Anything bigger is a broken or hostile peer; file data comes in ~512 KiB chunks.
pub const MAX_FRAME: usize = 5 * 1024 * 1024;

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let len = r.read_u32().await? as usize;
    if len > MAX_FRAME {
        return Err(protocol(format!("frame of {len} bytes")));
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    w.write_u32(data.len() as u32).await?;
    w.write_all(data).await?;
    w.flush().await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Unknown,
    Phone,
    Tablet,
    Laptop,
}

impl DeviceKind {
    fn bits(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Phone => 1,
            Self::Tablet => 2,
            Self::Laptop => 3,
        }
    }

    fn from_bits(b: u8) -> Self {
        match b {
            1 => Self::Phone,
            2 => Self::Tablet,
            3 => Self::Laptop,
            _ => Self::Unknown,
        }
    }
}

/// A device as Quick Share describes it: kind and visible name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub name: String,
    pub kind: DeviceKind,
}

impl Endpoint {
    /// Kind bits, 16 bytes that only matter to Google's servers, then the length-prefixed name.
    pub fn encode(&self) -> Vec<u8> {
        let name = &self.name.as_bytes()[..self.name.len().min(255)];
        let mut out = vec![self.kind.bits() << 1];
        out.extend(random_bytes::<16>());
        out.push(name.len() as u8);
        out.extend(name);
        out
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let len = *b.get(17).ok_or_else(|| protocol("endpoint info too short"))? as usize;
        let name = b.get(18..18 + len).ok_or_else(|| protocol("endpoint name cut short"))?;
        Ok(Self {
            name: String::from_utf8_lossy(name).into_owned(),
            kind: DeviceKind::from_bits((b[0] >> 1) & 7),
        })
    }
}

/// 4 random alphanumerics naming this side of a connection.
pub fn endpoint_id() -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    random_bytes::<4>().iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}

/// The mDNS instance name: PCP byte, endpoint id, Quick Share's service id hash, two zeros.
pub fn mdns_name(endpoint_id: &str) -> String {
    let mut b = vec![0x23];
    b.extend(endpoint_id.as_bytes());
    b.extend([0xFC, 0x9F, 0x5E, 0, 0]);
    URL_SAFE_NO_PAD.encode(b)
}

pub fn mdns_txt(endpoint: &Endpoint) -> String {
    URL_SAFE_NO_PAD.encode(endpoint.encode())
}

fn offline(kind: v1_frame::FrameType, fill: impl FnOnce(&mut V1Frame)) -> Vec<u8> {
    let mut v1 = V1Frame { r#type: Some(kind as i32), ..Default::default() };
    fill(&mut v1);
    OfflineFrame { version: Some(offline_frame::Version::V1 as i32), v1: Some(v1) }.encode_to_vec()
}

pub fn connection_request(id: &str, me: &Endpoint) -> Vec<u8> {
    offline(v1_frame::FrameType::ConnectionRequest, |v1| {
        v1.connection_request = Some(ConnectionRequestFrame {
            endpoint_id: Some(id.into()),
            endpoint_name: Some(me.name.clone()),
            endpoint_info: Some(me.encode()),
            mediums: vec![connection_request_frame::Medium::WifiLan as i32],
            ..Default::default()
        })
    })
}

pub fn connection_response() -> Vec<u8> {
    offline(v1_frame::FrameType::ConnectionResponse, |v1| {
        #[allow(deprecated)]
        let frame = ConnectionResponseFrame {
            status: Some(0),
            response: Some(connection_response_frame::ResponseStatus::Accept as i32),
            os_info: Some(OsInfo { r#type: Some(os_info::OsType::Linux as i32) }),
            ..Default::default()
        };
        v1.connection_response = Some(frame);
    })
}

pub fn keep_alive(ack: bool) -> Vec<u8> {
    offline(v1_frame::FrameType::KeepAlive, |v1| v1.keep_alive = Some(KeepAliveFrame { ack: Some(ack), ..Default::default() }))
}

pub fn disconnection() -> Vec<u8> {
    offline(v1_frame::FrameType::Disconnection, |v1| v1.disconnection = Some(DisconnectionFrame::default()))
}

/// One chunk of a payload. Android sends bytes payloads as the whole body, then an
/// empty chunk flagged LAST_CHUNK; file payloads end the same way.
pub fn payload_chunk(
    id: i64,
    kind: payload_transfer_frame::payload_header::PayloadType,
    total: i64,
    offset: i64,
    body: Vec<u8>,
    last: bool,
) -> Vec<u8> {
    offline(v1_frame::FrameType::PayloadTransfer, |v1| {
        v1.payload_transfer = Some(PayloadTransferFrame {
            packet_type: Some(payload_transfer_frame::PacketType::Data as i32),
            payload_header: Some(payload_transfer_frame::PayloadHeader {
                id: Some(id),
                r#type: Some(kind as i32),
                total_size: Some(total),
                is_sensitive: Some(false),
                ..Default::default()
            }),
            payload_chunk: Some(payload_transfer_frame::PayloadChunk {
                flags: Some(last as i32),
                offset: Some(offset),
                body: Some(body),
                ..Default::default()
            }),
            ..Default::default()
        })
    })
}

/// A whole bytes payload as the two chunks Android expects.
pub fn bytes_payload(data: Vec<u8>) -> [Vec<u8>; 2] {
    use payload_transfer_frame::payload_header::PayloadType;
    let id = i64::from_le_bytes(random_bytes::<8>());
    let len = data.len() as i64;
    [
        payload_chunk(id, PayloadType::Bytes, len, 0, data, false),
        payload_chunk(id, PayloadType::Bytes, len, len, Vec::new(), true),
    ]
}

/// Reassembles bytes payloads from their chunks.
#[derive(Default)]
pub struct Assembler {
    buffers: std::collections::HashMap<i64, Vec<u8>>,
}

impl Assembler {
    /// Feeds one bytes-payload chunk; returns the payload once its last chunk arrives.
    pub fn feed(&mut self, frame: &PayloadTransferFrame) -> Result<Option<(i64, Vec<u8>)>> {
        let header = frame.payload_header.as_ref().ok_or_else(|| protocol("payload without header"))?;
        let chunk = frame.payload_chunk.as_ref().ok_or_else(|| protocol("payload without chunk"))?;
        let id = header.id.ok_or_else(|| protocol("payload without id"))?;
        if header.total_size.unwrap_or(0) as usize > MAX_FRAME {
            return Err(protocol("bytes payload too large"));
        }
        let buf = self.buffers.entry(id).or_default();
        if chunk.offset.unwrap_or(0) != buf.len() as i64 {
            return Err(protocol("bytes payload chunk out of order"));
        }
        buf.extend(chunk.body.as_deref().unwrap_or_default());
        if buf.len() > MAX_FRAME {
            return Err(protocol("bytes payload too large"));
        }
        if chunk.flags.unwrap_or(0) & 1 == 1 {
            return Ok(self.buffers.remove(&id).map(|b| (id, b)));
        }
        Ok(None)
    }
}

/// The Quick Share frames that travel inside bytes payloads.
pub mod share {
    use prost::Message;

    use crate::crypto::random_bytes;
    use crate::proto::share::{
        connection_response_frame, file_metadata, frame, paired_key_result_frame, v1_frame, ConnectionResponseFrame,
        FileMetadata, Frame, IntroductionFrame, PairedKeyEncryptionFrame, PairedKeyResultFrame, V1Frame,
    };

    pub use connection_response_frame::Status;

    fn share(kind: v1_frame::FrameType, fill: impl FnOnce(&mut V1Frame)) -> Vec<u8> {
        let mut v1 = V1Frame { r#type: Some(kind as i32), ..Default::default() };
        fill(&mut v1);
        Frame { version: Some(frame::Version::V1 as i32), v1: Some(v1) }.encode_to_vec()
    }

    /// Meaningless without Google's servers; random bytes satisfy Android.
    pub fn paired_key_encryption() -> Vec<u8> {
        share(v1_frame::FrameType::PairedKeyEncryption, |v1| {
            v1.paired_key_encryption = Some(PairedKeyEncryptionFrame {
                secret_id_hash: Some(random_bytes::<6>().to_vec()),
                signed_data: Some(random_bytes::<72>().to_vec()),
                ..Default::default()
            })
        })
    }

    pub fn paired_key_result() -> Vec<u8> {
        share(v1_frame::FrameType::PairedKeyResult, |v1| {
            v1.paired_key_result = Some(PairedKeyResultFrame {
                status: Some(paired_key_result_frame::Status::Unable as i32),
                ..Default::default()
            })
        })
    }

    pub fn response(status: Status) -> Vec<u8> {
        share(v1_frame::FrameType::Response, |v1| {
            v1.connection_response = Some(ConnectionResponseFrame { status: Some(status as i32), ..Default::default() })
        })
    }

    pub fn cancel() -> Vec<u8> {
        share(v1_frame::FrameType::Cancel, |_| {})
    }

    /// (payload id, name, size, mime type) of each file about to be sent.
    pub fn introduction(files: &[(i64, String, i64, String)]) -> Vec<u8> {
        share(v1_frame::FrameType::Introduction, |v1| {
            v1.introduction = Some(IntroductionFrame {
                file_metadata: files
                    .iter()
                    .map(|(id, name, size, mime)| FileMetadata {
                        name: Some(name.clone()),
                        r#type: Some(file_metadata::Type::Unknown as i32),
                        payload_id: Some(*id),
                        size: Some(*size),
                        mime_type: Some(mime.clone()),
                        id: Some(i64::from_le_bytes(random_bytes::<8>())),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_round_trips() {
        let me = Endpoint { name: "hachikuji".into(), kind: DeviceKind::Laptop };
        let encoded = me.encode();
        assert_eq!(encoded[0], 6);
        assert_eq!(Endpoint::decode(&encoded).unwrap(), me);
        assert!(Endpoint::decode(&encoded[..10]).is_err());
    }

    #[test]
    fn mdns_name_is_ten_bytes() {
        let name = mdns_name("ab12");
        let raw = URL_SAFE_NO_PAD.decode(&name).unwrap();
        assert_eq!(raw, [0x23, b'a', b'b', b'1', b'2', 0xFC, 0x9F, 0x5E, 0, 0]);
        assert!(!name.contains('='));
    }

    #[tokio::test]
    async fn frames_are_length_prefixed() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hola").await.unwrap();
        assert_eq!(buf, [0, 0, 0, 4, b'h', b'o', b'l', b'a']);
        assert_eq!(read_frame(&mut &buf[..]).await.unwrap(), b"hola");
        let huge = (MAX_FRAME as u32 + 1).to_be_bytes();
        assert!(read_frame(&mut &huge[..]).await.is_err());
    }
}
