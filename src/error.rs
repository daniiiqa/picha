use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("malformed message: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("handshake failed: {0}")]
    Handshake(&'static str),
    #[error("the other side cancelled")]
    Cancelled,
    #[error("rejected")]
    Rejected,
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn protocol(msg: impl Into<String>) -> Error {
    Error::Protocol(msg.into())
}
