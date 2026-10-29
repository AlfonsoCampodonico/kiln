//! Extra processes in a running guest (kiln-shim exec and volumes design §2.1, §2.3):
//! the exec messages, and the header that opens each exec stdio connection on
//! [`crate::port::EXEC`].

use serde::{Deserialize, Serialize};

use crate::config::Process;
use crate::error::{ProtoError, Result};
use crate::message::{MAX_MESSAGE_BYTES, WindowSize};

/// The most execs a VM runs at once.
pub const MAX_EXECS: usize = 32;
/// The bytes that open an exec stdio connection: the exec id (u32 LE), then the stream.
pub const EXEC_HEADER: usize = 5;

/// Start a process next to the main one. `id` is the host's (S5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecStart {
    pub id: u32,
    pub process: Process,
    /// Run it on a pseudo-terminal of this size: one `Tty` stream instead of pipes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<WindowSize>,
    /// Relay stdin (or the terminal's input); without it stdin is at EOF.
    #[serde(default)]
    pub stdin: bool,
}

/// Forward signal `sig` to exec `id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecSignal {
    pub id: u32,
    pub sig: i32,
}

/// Exec `id`'s terminal was resized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecWindowSize {
    pub id: u32,
    pub rows: u16,
    pub cols: u16,
}

/// Exec `id` runs, and its streams are connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecStarted {
    pub id: u32,
}

/// Exec `id` ended and its output reached EOF; as [`crate::Exited`]. Guest-asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecExited {
    pub id: u32,
    pub signaled: bool,
    pub code: i32,
}

impl ExecExited {
    /// The process's exit code, or `128 + signal`.
    pub fn exit_code(&self) -> i32 {
        if self.signaled { 128 + self.code } else { self.code }
    }
}

/// Exec `id` could not start. `errno` is set when exec(2) failed. `message` is untrusted
/// text: sanitise it before printing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecFailed {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errno: Option<i32>,
    pub message: String,
}

impl ExecFailed {
    /// Builds the message, truncating `message` to [`MAX_MESSAGE_BYTES`] on a character
    /// boundary.
    pub fn new(id: u32, errno: Option<i32>, message: &str) -> Self {
        let mut end = message.len().min(MAX_MESSAGE_BYTES);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            id,
            errno,
            message: message[..end].to_string(),
        }
    }
}

/// Which of an exec's streams a connection carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExecStream {
    /// Host to guest.
    Stdin = 0,
    Stdout = 1,
    Stderr = 2,
    /// Both ways: the terminal.
    Tty = 3,
}

impl ExecStream {
    /// The connection's first [`EXEC_HEADER`] bytes.
    pub fn header(self, id: u32) -> [u8; EXEC_HEADER] {
        let id = id.to_le_bytes();
        [id[0], id[1], id[2], id[3], self as u8]
    }

    /// Parses a header; an id of 0 or an unknown stream is a violation.
    pub fn parse(header: [u8; EXEC_HEADER]) -> Result<(u32, Self)> {
        let id = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let stream = match header[4] {
            0 => Self::Stdin,
            1 => Self::Stdout,
            2 => Self::Stderr,
            3 => Self::Tty,
            s => return Err(ProtoError::invalid("exec stream header", format!("stream {s}"))),
        };
        check_id("exec stream header", id)?;
        Ok((id, stream))
    }
}

pub(crate) fn check_id(name: &'static str, id: u32) -> Result<()> {
    if id == 0 {
        return Err(ProtoError::invalid(name, "exec id 0"));
    }
    Ok(())
}

pub(crate) fn check_code(name: &'static str, signaled: bool, code: i32) -> Result<()> {
    let ok = if signaled {
        (1..=64).contains(&code)
    } else {
        (0..=255).contains(&code)
    };
    if ok {
        Ok(())
    } else {
        Err(ProtoError::invalid(name, format!("code {code} (signaled: {signaled})")))
    }
}

impl ExecStart {
    pub fn validate(&self) -> Result<()> {
        check_id("ExecStart", self.id)?;
        self.process.validate("ExecStart")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GuestMessage, HostMessage, Message};

    #[test]
    fn headers_round_trip_and_reject_bad_values() {
        for s in [
            ExecStream::Stdin,
            ExecStream::Stdout,
            ExecStream::Stderr,
            ExecStream::Tty,
        ] {
            assert_eq!(ExecStream::parse(s.header(0x0102_0304)).unwrap(), (0x0102_0304, s));
        }
        assert_eq!(ExecStream::Stderr.header(1), [1, 0, 0, 0, 2]);
        assert!(ExecStream::parse([1, 0, 0, 0, 4]).is_err());
        assert!(ExecStream::parse([0, 0, 0, 0, 1]).is_err());
    }

    #[test]
    fn exec_messages_round_trip_and_validate() {
        let start = ExecStart {
            id: 7,
            process: Process {
                entrypoint: vec!["sh".into(), "-c".into(), "id".into()],
                env: vec!["A=1".into()],
                user: Some("1000:1000".into()),
                ..Process::default()
            },
            tty: Some(WindowSize { rows: 24, cols: 80 }),
            stdin: true,
        };
        let host = [
            HostMessage::ExecStart(start.clone()),
            HostMessage::ExecSignal(ExecSignal { id: 7, sig: 15 }),
            HostMessage::ExecWindowSize(ExecWindowSize {
                id: 7,
                rows: 50,
                cols: 100,
            }),
        ];
        for m in host {
            assert_eq!(HostMessage::decode(m.message_type(), &m.payload()).unwrap(), m);
        }
        let guest = [
            GuestMessage::ExecStarted(ExecStarted { id: 7 }),
            GuestMessage::ExecExited(ExecExited {
                id: 7,
                signaled: true,
                code: 9,
            }),
            GuestMessage::ExecFailed(ExecFailed::new(7, Some(2), "exec nope: not found")),
        ];
        for m in guest {
            assert_eq!(GuestMessage::decode(m.message_type(), &m.payload()).unwrap(), m);
        }
        let mut bad = start.clone();
        bad.id = 0;
        assert!(HostMessage::ExecStart(bad).validate().is_err());
        let mut bad = start;
        bad.process.entrypoint.clear();
        assert!(HostMessage::ExecStart(bad).validate().is_err());
        assert!(
            HostMessage::ExecSignal(ExecSignal { id: 1, sig: 0 })
                .validate()
                .is_err()
        );
        assert!(
            HostMessage::ExecSignal(ExecSignal { id: 0, sig: 9 })
                .validate()
                .is_err()
        );
        for bad in [&br#"{"id":0}"#[..], br#"{"id":1,"x":1}"#] {
            assert!(GuestMessage::decode(14, bad).is_err());
        }
        assert!(GuestMessage::decode(15, br#"{"id":1,"signaled":false,"code":256}"#).is_err());
        assert!(GuestMessage::decode(16, br#"{"id":1,"errno":0,"message":""}"#).is_err());
        assert_eq!(
            ExecExited {
                id: 1,
                signaled: true,
                code: 9
            }
            .exit_code(),
            137
        );
    }
}
