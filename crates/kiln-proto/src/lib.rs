//! The control protocol between `kiln-init` and the host (spec §9.5, threat T9).
//!
//! The guest connects to the host (CID [`HOST_CID`]) over vsock. Port
//! [`port::CONTROL`] carries frames: a `u32` little-endian length, then a `u8`
//! message type, then a JSON object. The length counts the type byte and the
//! payload and is at most [`MAX_FRAME`]. Every message is validated when it is
//! encoded and when it is decoded; an unknown type, an oversize frame, a payload
//! that is not exactly one JSON object of the expected shape (unknown fields
//! included), or a value out of range is an error, and either side ends the VM.
//! The other ports carry raw stdio bytes, where EOF is a half-close.
#![forbid(unsafe_code)]

mod config;
mod error;
mod exec;
mod exit;
mod frame;
mod message;
pub mod sanitize;
pub mod signal;
mod volume;

pub use config::{Config, ExitMethod, Ipv6Network, MAX_LAYERS, MIN_SCRATCH_BYTES, Network, Process, Scratch};
pub use error::{ProtoError, Result};
pub use exec::{
    EXEC_HEADER, ExecExited, ExecFailed, ExecSignal, ExecStart, ExecStarted, ExecStream, ExecWindowSize, MAX_EXECS,
};
pub use exit::{EXIT_CANNOT_INVOKE, EXIT_INFRA, EXIT_NOT_FOUND};
pub use frame::{read_message, write_message};
pub use message::{
    Exited, GuestMessage, Hello, HostMessage, InitFailed, MAX_MESSAGE_BYTES, Message, STAGES, Shutdown, Signal, Stage,
    WindowSize, stage_name,
};
pub use volume::{
    FileEntry, FileUpdate, MAX_VOLUME_BYTES, MAX_VOLUME_FILES, MAX_VOLUMES, Volume, VolumeFile, VolumeKind,
    check_generation, check_relative_path, file_updates,
};

/// The protocol version a guest announces in [`Hello`].
pub const PROTOCOL_VERSION: u32 = 1;
/// The host's vsock CID; the guest connects to it.
pub const HOST_CID: u32 = 2;
/// The guest's vsock CID (spec §9.1).
pub const GUEST_CID: u32 = 3;
/// The largest frame: the type byte and payload, without the length field.
pub const MAX_FRAME: u32 = 64 * 1024;

/// vsock ports, all guest-initiated (spec §9.5).
pub mod port {
    /// Framed control messages, both directions.
    pub const CONTROL: u32 = 1024;
    /// The main process's stdin (host to guest); only connected when `interactive`.
    pub const STDIN: u32 = 1025;
    /// The main process's stdout (guest to host).
    pub const STDOUT: u32 = 1026;
    /// The main process's stderr (guest to host).
    pub const STDERR: u32 = 1027;
    /// The terminal in `tty` mode, both directions; replaces the three stdio ports.
    pub const TTY: u32 = 1028;
    /// Exec stdio: one connection per stream, each opened by a [`crate::ExecStream`] header.
    pub const EXEC: u32 = 1029;
}
