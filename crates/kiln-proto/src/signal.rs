//! Linux signal numbers, which are the same on aarch64 and x86_64 for 1..=31.
//! The protocol carries numbers; image configs carry names (`STOPSIGNAL`).
//! Real-time signals (`SIGRTMIN+n`, 32–64) are not supported: their numbers depend
//! on the guest's libc, and kiln-init forwards only the named signals.

use crate::error::{ProtoError, Result};

/// The default `stopSignal` (SIGTERM), as in Docker.
pub const DEFAULT_STOP: i32 = 15;
/// SIGKILL, for display and tests.
pub const KILL: i32 = 9;

const NAMES: [&str; 31] = [
    "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "BUS", "FPE", "KILL", "USR1", "SEGV", "USR2", "PIPE", "ALRM", "TERM",
    "STKFLT", "CHLD", "CONT", "STOP", "TSTP", "TTIN", "TTOU", "URG", "XCPU", "XFSZ", "VTALRM", "PROF", "WINCH", "IO",
    "PWR", "SYS",
];

/// The name of signal `sig` without the `SIG` prefix, if it is a supported signal.
pub fn name(sig: i32) -> Option<&'static str> {
    usize::try_from(sig)
        .ok()?
        .checked_sub(1)
        .and_then(|i| NAMES.get(i))
        .copied()
}

/// Parses `SIGTERM`, `TERM`, `term` or `15`, as Docker does for `STOPSIGNAL`.
pub fn parse(s: &str) -> Option<i32> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i32>() {
        return name(n).map(|_| n);
    }
    let upper = s.to_ascii_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(&upper);
    let bare = match bare {
        "IOT" => "ABRT",
        "POLL" => "IO",
        "CLD" => "CHLD",
        other => other,
    };
    NAMES.iter().position(|n| *n == bare).map(|i| i as i32 + 1)
}

/// Parses an image's `STOPSIGNAL` for `Config::stop_signal`, like [`parse`], with an
/// error that says why a value is refused, real-time signals included.
pub fn parse_stop_signal(s: &str) -> Result<i32> {
    parse(s).ok_or_else(|| {
        let reason = if is_realtime(s) {
            format!("{s:?} is a real-time signal; kiln supports only signals 1-31 (SIGHUP to SIGSYS)")
        } else {
            format!("{s:?} is not a signal name or number")
        };
        ProtoError::invalid("STOPSIGNAL", reason)
    })
}

/// `SIGRTMIN`, `RTMIN+3`, `SIGRTMAX-1`, or a number from 32 to 64.
fn is_realtime(s: &str) -> bool {
    let upper = s.trim().to_ascii_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(&upper);
    bare.starts_with("RTMIN") || bare.starts_with("RTMAX") || bare.parse::<i32>().is_ok_and(|n| (32..=64).contains(&n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_numbers_and_aliases() {
        assert_eq!(parse("SIGTERM"), Some(15));
        assert_eq!(parse("quit"), Some(3));
        assert_eq!(parse(" 10 "), Some(10));
        assert_eq!(parse("SIGIOT"), Some(6));
        assert_eq!(parse("SIGWINCH"), Some(28));
        assert_eq!(parse("SIGSYS"), Some(31));
        for bad in ["", "SIG", "0", "32", "-1", "SIGRTMIN+3", "TERMX"] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn stop_signals_explain_refusals() {
        assert_eq!(parse_stop_signal("SIGQUIT").unwrap(), 3);
        for rt in ["SIGRTMIN+3", "rtmin", "SIGRTMAX-1", "34", "64"] {
            let err = parse_stop_signal(rt).unwrap_err().to_string();
            assert!(err.contains("real-time signal") && err.contains("1-31"), "{rt}: {err}");
        }
        for bad in ["SIGFOO", "65", "0", ""] {
            let err = parse_stop_signal(bad).unwrap_err().to_string();
            assert!(err.contains("not a signal name or number"), "{bad}: {err}");
        }
    }

    #[test]
    fn names_cover_one_to_thirty_one() {
        assert_eq!(name(1), Some("HUP"));
        assert_eq!(name(31), Some("SYS"));
        assert_eq!(name(0), None);
        assert_eq!(name(32), None);
        for sig in 1..=31 {
            assert_eq!(parse(name(sig).unwrap()), Some(sig));
        }
    }
}
