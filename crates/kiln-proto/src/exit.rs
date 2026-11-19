//! Host-side exit codes (spec §9.5), numbered as Docker numbers them. The guest
//! asserts them (spec §3).

use crate::message::{Exited, InitFailed, stage_name};
use crate::sanitize::clean_line;

/// `kiln` or infrastructure failed, including a boot timeout or a VM that
/// ended without `Exited`.
pub const EXIT_INFRA: i32 = 125;
/// The entrypoint was found but could not be invoked (permission denied, a directory).
pub const EXIT_CANNOT_INVOKE: i32 = 126;
/// The entrypoint was not found.
pub const EXIT_NOT_FOUND: i32 = 127;

// Linux errno numbers (the same on every Linux architecture); kiln-proto stays libc-free.
const ENOENT: i32 = 2;
const EACCES: i32 = 13;
const EISDIR: i32 = 21;
/// The stage that starts the main process.
const PROCESS_STAGE: u8 = 6;

impl Exited {
    /// The app's exit code, or `128 + signal`.
    pub fn exit_code(&self) -> i32 {
        if self.signaled { 128 + self.code } else { self.code }
    }

    /// When a service ended the run: one sanitised line for the user (T8),
    /// `service <name> exited with <code>` or `... was killed by SIG<name>`.
    pub fn describe_service(&self) -> Option<String> {
        let name = clean_line(self.service.as_deref()?);
        Some(if self.signaled {
            let sig = crate::signal::name(self.code).map_or_else(|| self.code.to_string(), |n| format!("SIG{n}"));
            format!("service {name} was killed by {sig}, which ended the run")
        } else {
            format!("service {name} exited with {}, which ended the run", self.code)
        })
    }
}

impl InitFailed {
    /// As Docker reports them: 127 when exec(2) of the entrypoint found nothing
    /// (ENOENT), 126 when it was found but not invokable (EACCES, EISDIR), 125 for
    /// everything else, including other stage-6 failures (an unknown user, setgroups,
    /// ENOEXEC, ENOTDIR, ...) and every earlier stage.
    pub fn exit_code(&self) -> i32 {
        match (self.stage, self.errno) {
            (PROCESS_STAGE, Some(ENOENT)) => EXIT_NOT_FOUND,
            (PROCESS_STAGE, Some(EACCES | EISDIR)) => EXIT_CANNOT_INVOKE,
            _ => EXIT_INFRA,
        }
    }

    /// One sanitised line for the user (T8): `<stage name>: <message>`.
    pub fn describe(&self) -> String {
        format!(
            "guest init failed at {}: {}",
            stage_name(self.stage),
            clean_line(&self.message)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_docker() {
        assert_eq!(Exited::main(false, 3).exit_code(), 3);
        assert_eq!(Exited::main(true, 9).exit_code(), 137);
        let svc = Exited {
            service: Some("php-fpm".into()),
            ..Exited::main(false, 70)
        };
        assert_eq!(svc.exit_code(), 70);
        assert_eq!(
            svc.describe_service().unwrap(),
            "service php-fpm exited with 70, which ended the run"
        );
        assert_eq!(Exited::main(false, 0).describe_service(), None);
        let killed = Exited {
            service: Some("web".into()),
            ..Exited::main(true, 9)
        };
        assert_eq!(
            killed.describe_service().unwrap(),
            "service web was killed by SIGKILL, which ended the run"
        );
        assert_eq!(InitFailed::new(6, Some(2), "").exit_code(), 127);
        assert_eq!(InitFailed::new(6, Some(13), "").exit_code(), 126);
        assert_eq!(InitFailed::new(6, Some(21), "").exit_code(), 126);
        assert_eq!(InitFailed::new(6, Some(8), "").exit_code(), 125, "ENOEXEC");
        assert_eq!(InitFailed::new(6, Some(20), "").exit_code(), 125, "ENOTDIR");
        assert_eq!(InitFailed::new(6, None, "no such user").exit_code(), 125);
        assert_eq!(InitFailed::new(3, Some(2), "").exit_code(), 125);
    }

    #[test]
    fn describe_sanitises_guest_text() {
        let f = InitFailed::new(6, Some(2), "exec /bin/\x1b[2Jx\u{202e}y\nz: not found");
        assert_eq!(
            f.describe(),
            "guest init failed at process: exec /bin/[2Jxy z: not found"
        );
    }
}
