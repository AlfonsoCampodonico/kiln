//! The shim's channel to a VM worker (design §7.4): JSON objects, one per line, on a
//! Unix socketpair that is the worker's fd 3. The first line to the worker is its plan;
//! after that, each line is a message of at most [`MAX_LINE`] bytes.

use std::io::{self, BufRead, BufReader, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The longest message line.
pub const MAX_LINE: usize = 64 << 10;
/// The longest plan line: the guest's `Config` (one 64 KiB frame), the paths around it,
/// and the volumes' files (at most 32 MiB, base64). The shim's `files` lines to a worker
/// may be as long; the worker trusts its shim. Lines from a worker stay at [`MAX_LINE`].
pub const MAX_PLAN: usize = 48 << 20;
/// The worker's end of the channel.
pub const WORKER_FD: i32 = 3;

/// From the shim to a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum ToWorker {
    /// Boot the VM (task `Start`).
    Start {},
    /// Forward a signal (1-31, not SIGKILL) to the guest's main process.
    Signal(i32),
    /// Kill the VM.
    Kill {},
    Resize {
        rows: u16,
        cols: u16,
    },
    /// Stop relaying stdin: the guest sees EOF.
    CloseStdin {},
    /// Start exec `id`; its FIFOs came just before, on the fd channel.
    Exec {
        id: u32,
        process: kiln_proto::Process,
        tty: bool,
    },
    ExecSignal {
        id: u32,
        signal: i32,
    },
    ExecResize {
        id: u32,
        rows: u16,
        cols: u16,
    },
    ExecCloseStdin {
        id: u32,
    },
    /// A files volume's new generation (kubelet updated it).
    Files {
        volume: u32,
        files: Vec<kiln_proto::VolumeFile>,
    },
}

/// From a worker to the shim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum FromWorker {
    /// Preparation is done (task `Create` returns).
    Ready {},
    /// The VM is booting (task `Start` returns).
    Started {
        #[serde(rename = "vmmPid")]
        vmm_pid: u32,
    },
    /// The session ended with this exit code (kiln §9.5).
    Exited {
        code: i32,
    },
    /// Preparation or boot failed; `Create` or `Start` returns the message.
    Failed {
        message: String,
    },
    ExecStarted {
        id: u32,
    },
    /// The exec could not start (`Start` of the exec returns the message).
    ExecFailed {
        id: u32,
        message: String,
    },
    /// The exec's exit code (`128 + signal`; 137 when its VM ended first).
    ExecExited {
        id: u32,
        code: i32,
    },
}

/// Writes `m` as one line.
pub fn send<T: Serialize>(w: &mut impl Write, m: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(m).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

/// Reads lines as messages.
pub struct Lines<R> {
    r: BufReader<R>,
}

impl<R: Read> Lines<R> {
    pub fn new(r: R) -> Self {
        Self { r: BufReader::new(r) }
    }

    /// The next message; `None` at EOF.
    pub fn read<T: DeserializeOwned>(&mut self) -> io::Result<Option<T>> {
        self.read_within(MAX_LINE)
    }

    /// The next message of at most `max` bytes; `None` at EOF.
    pub fn read_within<T: DeserializeOwned>(&mut self, max: usize) -> io::Result<Option<T>> {
        let mut buf = Vec::new();
        let n = (&mut self.r).take(max as u64 + 1).read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        if buf.pop() != Some(b'\n') {
            let why = if buf.len() >= max {
                "a line over the size limit"
            } else {
                "a partial line before EOF"
            };
            return Err(io::Error::new(io::ErrorKind::InvalidData, why));
        }
        serde_json::from_slice(&buf)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trip_and_cap() {
        let to = [
            (ToWorker::Start {}, r#"{"start":{}}"#),
            (ToWorker::Signal(15), r#"{"signal":15}"#),
            (ToWorker::Kill {}, r#"{"kill":{}}"#),
            (
                ToWorker::Resize { rows: 40, cols: 120 },
                r#"{"resize":{"rows":40,"cols":120}}"#,
            ),
            (ToWorker::CloseStdin {}, r#"{"closeStdin":{}}"#),
            (
                ToWorker::ExecSignal { id: 2, signal: 15 },
                r#"{"execSignal":{"id":2,"signal":15}}"#,
            ),
            (ToWorker::ExecCloseStdin { id: 2 }, r#"{"execCloseStdin":{"id":2}}"#),
            (
                ToWorker::Files {
                    volume: 1,
                    files: vec![kiln_proto::VolumeFile {
                        path: "a".into(),
                        mode: 0o644,
                        uid: 0,
                        gid: 0,
                        data: b"hi".to_vec(),
                    }],
                },
                r#"{"files":{"volume":1,"files":[{"path":"a","mode":420,"uid":0,"gid":0,"data":"aGk="}]}}"#,
            ),
        ];
        let from = [
            (FromWorker::Ready {}, r#"{"ready":{}}"#),
            (FromWorker::Started { vmm_pid: 42 }, r#"{"started":{"vmmPid":42}}"#),
            (FromWorker::Exited { code: 3 }, r#"{"exited":{"code":3}}"#),
            (
                FromWorker::Failed { message: "no".into() },
                r#"{"failed":{"message":"no"}}"#,
            ),
            (
                FromWorker::ExecExited { id: 3, code: 137 },
                r#"{"execExited":{"id":3,"code":137}}"#,
            ),
        ];
        let mut stream = Vec::new();
        for (m, json) in &to {
            assert_eq!(serde_json::to_string(m).unwrap(), *json);
            send(&mut stream, m).unwrap();
        }
        let mut lines = Lines::new(stream.as_slice());
        for (m, _) in &to {
            assert_eq!(lines.read::<ToWorker>().unwrap().as_ref(), Some(m));
        }
        assert_eq!(lines.read::<ToWorker>().unwrap(), None);
        for (m, json) in &from {
            assert_eq!(serde_json::to_string(m).unwrap(), *json);
        }
        let long = format!("{{\"failed\":{{\"message\":\"{}\"}}}}\n", "x".repeat(70 << 10));
        assert!(Lines::new(long.as_bytes()).read::<FromWorker>().is_err());
        assert!(
            Lines::new(long.as_bytes())
                .read_within::<FromWorker>(MAX_PLAN)
                .unwrap()
                .is_some()
        );
        assert!(
            Lines::new(&b"{\"ready\":{}}"[..]).read::<FromWorker>().is_err(),
            "no newline"
        );
        assert!(Lines::new(&b"{\"what\":1}\n"[..]).read::<FromWorker>().is_err());
    }
}
