use thiserror::Error;

/// A protocol error. Every variant but `Io` is a protocol violation: the peer
/// that receives one ends the VM (spec §9.5, T9).
#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds the {max}-byte limit", max = crate::MAX_FRAME)]
    Oversize(u32),
    #[error("empty frame (no message type)")]
    Empty,
    #[error("connection closed inside a frame")]
    Truncated,
    #[error("unknown message type {0}")]
    UnknownType(u8),
    #[error("message type {ty} ({name}) is not valid in this direction")]
    WrongDirection { ty: u8, name: &'static str },
    #[error("invalid {name} payload: {reason}")]
    Payload { name: &'static str, reason: String },
    #[error("invalid {name}: {reason}")]
    Invalid { name: &'static str, reason: String },
}

impl ProtoError {
    pub(crate) fn invalid(name: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            name,
            reason: reason.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, ProtoError>;
