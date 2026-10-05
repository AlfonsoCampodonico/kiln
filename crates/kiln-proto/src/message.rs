//! Control messages (spec §9.5). Each direction has its own enum, so a message
//! sent the wrong way fails to decode like any other violation.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{ProtoError, Result};

/// The number of `kiln-init` stages (spec §9.6).
pub const STAGES: u8 = 7;
/// The longest `InitFailed` message, in bytes. [`InitFailed::new`] truncates to it.
pub const MAX_MESSAGE_BYTES: usize = 4096;
/// The longest payload error reason kept, in characters.
const MAX_REASON_CHARS: usize = 200;

/// The name of `kiln-init` stage `n` (1-based), for diagnostics.
pub fn stage_name(n: u8) -> &'static str {
    match n {
        1 => "early",
        2 => "control",
        3 => "storage",
        4 => "root",
        5 => "identity",
        6 => "process",
        7 => "supervise",
        _ => "unknown",
    }
}

/// The guest's first message; the host answers with `Config`, once per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub protocol: u32,
}

/// `kiln-init` entered stage `n` (diagnostics only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    pub n: u8,
}

/// The main process ended, and its stdout and stderr reached EOF. With
/// `signaled`, `code` is the signal that killed it. Guest-asserted (spec §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exited {
    pub signaled: bool,
    pub code: i32,
}

/// `kiln-init` failed in `stage`. At stage 6, `errno` is set only when exec(2)
/// of the entrypoint failed. `message` is untrusted text: sanitise it before
/// printing ([`InitFailed::describe`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitFailed {
    pub stage: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errno: Option<i32>,
    pub message: String,
}

impl InitFailed {
    /// Builds the message, truncating `message` to [`MAX_MESSAGE_BYTES`] on a
    /// character boundary.
    pub fn new(stage: u8, errno: Option<i32>, message: &str) -> Self {
        let mut end = message.len().min(MAX_MESSAGE_BYTES);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            stage,
            errno,
            message: message[..end].to_string(),
        }
    }
}

/// Send `stopSignal` to the main process, then SIGKILL everything after `grace_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Shutdown {
    pub grace_secs: u32,
}

/// Forward signal `sig` to the main process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signal {
    pub sig: i32,
}

/// The terminal's size (`tty` mode only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowSize {
    pub rows: u16,
    pub cols: u16,
}

/// A payload with no fields (`Running`): exactly `{}`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

/// Messages from the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestMessage {
    Hello(Hello),
    Stage(Stage),
    /// The workload started; ends the boot timeout.
    Running,
    Exited(Exited),
    InitFailed(InitFailed),
}

/// Messages from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMessage {
    Config(Box<Config>),
    Shutdown(Shutdown),
    Signal(Signal),
    WindowSize(WindowSize),
}

const HELLO: u8 = 1;
const CONFIG: u8 = 2;
const STAGE: u8 = 3;
const RUNNING: u8 = 4;
const EXITED: u8 = 5;
const INIT_FAILED: u8 = 6;
const SHUTDOWN: u8 = 7;
const SIGNAL: u8 = 8;
const WINDOW_SIZE: u8 = 9;

fn type_name(ty: u8) -> Option<&'static str> {
    Some(match ty {
        HELLO => "Hello",
        CONFIG => "Config",
        STAGE => "Stage",
        RUNNING => "Running",
        EXITED => "Exited",
        INIT_FAILED => "InitFailed",
        SHUTDOWN => "Shutdown",
        SIGNAL => "Signal",
        WINDOW_SIZE => "WindowSize",
        _ => return None,
    })
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::GuestMessage {}
    impl Sealed for super::HostMessage {}
}

/// A direction's message set: [`GuestMessage`] or [`HostMessage`].
pub trait Message: sealed::Sealed + Sized {
    /// The frame's type byte.
    fn message_type(&self) -> u8;
    /// Checks every value against its allowed range.
    fn validate(&self) -> Result<()>;
    /// The JSON payload.
    fn payload(&self) -> Vec<u8>;
    /// Decodes and validates a frame of this direction.
    fn decode(ty: u8, payload: &[u8]) -> Result<Self>;
}

/// Parses a payload that must be exactly one JSON object.
fn parse<T: DeserializeOwned>(ty: u8, payload: &[u8]) -> Result<T> {
    let name = type_name(ty).unwrap_or("unknown");
    if payload.first() != Some(&b'{') || payload.last() != Some(&b'}') {
        return Err(ProtoError::Payload {
            name,
            reason: "not a single JSON object".into(),
        });
    }
    serde_json::from_slice(payload).map_err(|e| ProtoError::Payload {
        name,
        reason: sanitize(&e.to_string()),
    })
}

/// serde quotes unknown field names raw, and the guest chooses them: escape
/// control characters and cap the length before the text can reach a terminal.
fn sanitize(text: &str) -> String {
    text.escape_debug().take(MAX_REASON_CHARS).collect()
}

fn json<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("protocol messages serialize")
}

fn wrong_direction(ty: u8) -> ProtoError {
    match type_name(ty) {
        Some(name) => ProtoError::WrongDirection { ty, name },
        None => ProtoError::UnknownType(ty),
    }
}

fn check_stage(name: &'static str, n: u8) -> Result<()> {
    if (1..=STAGES).contains(&n) {
        Ok(())
    } else {
        Err(ProtoError::invalid(name, format!("stage {n} is not 1..={STAGES}")))
    }
}

impl Message for GuestMessage {
    fn message_type(&self) -> u8 {
        match self {
            GuestMessage::Hello(_) => HELLO,
            GuestMessage::Stage(_) => STAGE,
            GuestMessage::Running => RUNNING,
            GuestMessage::Exited(_) => EXITED,
            GuestMessage::InitFailed(_) => INIT_FAILED,
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            GuestMessage::Hello(h) if h.protocol == 0 => Err(ProtoError::invalid("Hello", "protocol 0")),
            GuestMessage::Hello(_) | GuestMessage::Running => Ok(()),
            GuestMessage::Stage(s) => check_stage("Stage", s.n),
            GuestMessage::Exited(e) => {
                let ok = if e.signaled {
                    (1..=64).contains(&e.code)
                } else {
                    (0..=255).contains(&e.code)
                };
                if ok {
                    Ok(())
                } else {
                    Err(ProtoError::invalid(
                        "Exited",
                        format!("code {} (signaled: {})", e.code, e.signaled),
                    ))
                }
            }
            GuestMessage::InitFailed(f) => {
                check_stage("InitFailed", f.stage)?;
                if let Some(errno) = f.errno
                    && !(1..=4095).contains(&errno)
                {
                    return Err(ProtoError::invalid("InitFailed", format!("errno {errno}")));
                }
                if f.message.len() > MAX_MESSAGE_BYTES {
                    return Err(ProtoError::invalid(
                        "InitFailed",
                        format!("message of {} bytes", f.message.len()),
                    ));
                }
                Ok(())
            }
        }
    }

    fn payload(&self) -> Vec<u8> {
        match self {
            GuestMessage::Hello(h) => json(h),
            GuestMessage::Stage(s) => json(s),
            GuestMessage::Running => json(&Empty {}),
            GuestMessage::Exited(e) => json(e),
            GuestMessage::InitFailed(f) => json(f),
        }
    }

    fn decode(ty: u8, payload: &[u8]) -> Result<Self> {
        let msg = match ty {
            HELLO => GuestMessage::Hello(parse(ty, payload)?),
            STAGE => GuestMessage::Stage(parse(ty, payload)?),
            RUNNING => {
                parse::<Empty>(ty, payload)?;
                GuestMessage::Running
            }
            EXITED => GuestMessage::Exited(parse(ty, payload)?),
            INIT_FAILED => GuestMessage::InitFailed(parse(ty, payload)?),
            _ => return Err(wrong_direction(ty)),
        };
        msg.validate()?;
        Ok(msg)
    }
}

impl Message for HostMessage {
    fn message_type(&self) -> u8 {
        match self {
            HostMessage::Config(_) => CONFIG,
            HostMessage::Shutdown(_) => SHUTDOWN,
            HostMessage::Signal(_) => SIGNAL,
            HostMessage::WindowSize(_) => WINDOW_SIZE,
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            HostMessage::Config(c) => c.validate(),
            HostMessage::Signal(s) if crate::signal::name(s.sig).is_none() => {
                Err(ProtoError::invalid("Signal", format!("signal {}", s.sig)))
            }
            HostMessage::Signal(_) | HostMessage::Shutdown(_) | HostMessage::WindowSize(_) => Ok(()),
        }
    }

    fn payload(&self) -> Vec<u8> {
        match self {
            HostMessage::Config(c) => json(c),
            HostMessage::Shutdown(s) => json(s),
            HostMessage::Signal(s) => json(s),
            HostMessage::WindowSize(w) => json(w),
        }
    }

    fn decode(ty: u8, payload: &[u8]) -> Result<Self> {
        let msg = match ty {
            CONFIG => HostMessage::Config(Box::new(parse(ty, payload)?)),
            SHUTDOWN => HostMessage::Shutdown(parse(ty, payload)?),
            SIGNAL => HostMessage::Signal(parse(ty, payload)?),
            WINDOW_SIZE => HostMessage::WindowSize(parse(ty, payload)?),
            _ => return Err(wrong_direction(ty)),
        };
        msg.validate()?;
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_are_compact_json_objects() {
        assert_eq!(
            GuestMessage::Hello(Hello { protocol: 1 }).payload(),
            br#"{"protocol":1}"#
        );
        assert_eq!(GuestMessage::Running.payload(), b"{}");
        assert_eq!(
            HostMessage::Shutdown(Shutdown { grace_secs: 10 }).payload(),
            br#"{"graceSecs":10}"#
        );
        let f = GuestMessage::InitFailed(InitFailed::new(3, None, "no disk"));
        assert_eq!(f.payload(), br#"{"stage":3,"message":"no disk"}"#);
    }

    #[test]
    fn each_direction_rejects_the_other() {
        let err = HostMessage::decode(HELLO, br#"{"protocol":1}"#).unwrap_err();
        assert!(
            matches!(err, ProtoError::WrongDirection { ty: 1, name: "Hello" }),
            "{err}"
        );
        let err = GuestMessage::decode(SHUTDOWN, br#"{"graceSecs":1}"#).unwrap_err();
        assert!(matches!(err, ProtoError::WrongDirection { ty: 7, .. }), "{err}");
        assert!(matches!(
            GuestMessage::decode(0, b"{}"),
            Err(ProtoError::UnknownType(0))
        ));
        assert!(matches!(
            HostMessage::decode(200, b"{}"),
            Err(ProtoError::UnknownType(200))
        ));
    }

    #[test]
    fn payloads_must_be_exactly_the_expected_object() {
        for bad in [
            &br#"{"protocol":1,"extra":true}"#[..],
            br#"[1]"#,
            br#" {"protocol":1}"#,
            br#"{"protocol":1} "#,
            br#"{"protocol":1}{}"#,
            br#"{"protocol":"1"}"#,
            br#"{"protocol":1,"protocol":2}"#,
            br#"{}"#,
            b"",
            b"\xff",
        ] {
            let err = GuestMessage::decode(HELLO, bad).unwrap_err();
            assert!(
                matches!(err, ProtoError::Payload { name: "Hello", .. }),
                "{:?}: {err}",
                String::from_utf8_lossy(bad)
            );
        }
        assert!(GuestMessage::decode(RUNNING, br#"{"a":1}"#).is_err());
        assert_eq!(GuestMessage::decode(RUNNING, b"{ }").unwrap(), GuestMessage::Running);
    }

    #[test]
    fn values_are_range_checked_on_decode() {
        assert!(GuestMessage::decode(STAGE, br#"{"n":0}"#).is_err());
        assert!(GuestMessage::decode(STAGE, br#"{"n":8}"#).is_err());
        assert!(GuestMessage::decode(EXITED, br#"{"signaled":false,"code":256}"#).is_err());
        assert!(GuestMessage::decode(EXITED, br#"{"signaled":true,"code":0}"#).is_err());
        assert!(GuestMessage::decode(EXITED, br#"{"signaled":true,"code":9}"#).is_ok());
        assert!(GuestMessage::decode(INIT_FAILED, br#"{"stage":6,"errno":0,"message":""}"#).is_err());
        assert!(GuestMessage::decode(INIT_FAILED, br#"{"stage":6,"errno":null,"message":""}"#).is_ok());
        assert!(HostMessage::decode(SIGNAL, br#"{"sig":0}"#).is_err());
        assert!(HostMessage::decode(SIGNAL, br#"{"sig":32}"#).is_err());
        assert!(GuestMessage::decode(HELLO, br#"{"protocol":0}"#).is_err());
    }

    #[test]
    fn payload_errors_do_not_echo_guest_control_bytes_or_bulk() {
        let err = GuestMessage::decode(HELLO, "{\"\\u001b]0;pwned\\u0007\":1}".as_bytes()).unwrap_err();
        assert!(matches!(err, ProtoError::Payload { .. }), "{err}");
        let shown = err.to_string();
        assert!(shown.bytes().all(|b| b >= 0x20 && b != 0x7f), "{shown:?}");
        assert!(shown.contains("pwned"), "{shown}");
        let huge = format!("{{\"{}\":1}}", "k".repeat(60_000));
        let ProtoError::Payload { reason, .. } = GuestMessage::decode(HELLO, huge.as_bytes()).unwrap_err() else {
            panic!("expected a payload error");
        };
        assert!(
            reason.chars().count() <= MAX_REASON_CHARS,
            "{} chars",
            reason.chars().count()
        );
    }

    #[test]
    fn init_failed_truncates_on_a_char_boundary() {
        let long = "é".repeat(MAX_MESSAGE_BYTES);
        let f = InitFailed::new(2, None, &long);
        assert!(f.message.len() <= MAX_MESSAGE_BYTES && f.message.len() >= MAX_MESSAGE_BYTES - 1);
        assert!(GuestMessage::InitFailed(f).validate().is_ok());
    }
}
