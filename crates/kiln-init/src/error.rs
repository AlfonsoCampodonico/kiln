//! Init failures: a message for the console and `InitFailed`, and an errno when
//! a system call failed.

use std::fmt;

/// Why a stage failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub errno: Option<i32>,
    pub message: String,
    /// exec(2) of the entrypoint failed: the only stage-6 failure that reports
    /// its errno, from which the host derives 127 (ENOENT) or 126 (EACCES or
    /// EISDIR), as Docker does; every other stage-6 failure is 125 (spec §9.5).
    pub exec: bool,
}

impl Failure {
    /// A failure without an errno.
    pub fn msg(message: impl Into<String>) -> Self {
        Self {
            errno: None,
            message: message.into(),
            exec: false,
        }
    }

    /// `what` failed with an OS error.
    pub fn os(what: impl fmt::Display, err: &std::io::Error) -> Self {
        Self {
            errno: err.raw_os_error(),
            message: format!("{what}: {err}"),
            exec: false,
        }
    }

    /// The errno `InitFailed` carries at `stage`.
    pub fn reported_errno(&self, stage: u8) -> Option<i32> {
        if stage == 6 && !self.exec { None } else { self.errno }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

pub type Result<T> = std::result::Result<T, Failure>;

/// Adds what was being done to an OS error.
pub trait Context<T> {
    fn context(self, what: impl fmt::Display) -> Result<T>;
}

impl<T> Context<T> for std::io::Result<T> {
    fn context(self, what: impl fmt::Display) -> Result<T> {
        self.map_err(|e| Failure::os(what, &e))
    }
}

#[cfg(target_os = "linux")]
impl<T> Context<T> for rustix::io::Result<T> {
    fn context(self, what: impl fmt::Display) -> Result<T> {
        self.map_err(|e| Failure::os(what, &e.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_six_reports_errno_only_for_exec() {
        let io = std::io::Error::from_raw_os_error(2);
        let mut f = Failure::os("open /etc/passwd", &io);
        assert_eq!(f.errno, Some(2));
        assert!(f.message.starts_with("open /etc/passwd: "));
        assert_eq!(f.reported_errno(3), Some(2));
        assert_eq!(f.reported_errno(6), None);
        f.exec = true;
        assert_eq!(f.reported_errno(6), Some(2));
    }
}
