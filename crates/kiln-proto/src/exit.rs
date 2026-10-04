//! Host-side exit codes (spec §9.5), numbered as Docker numbers them. The guest
//! asserts them (spec §3).

use crate::message::{Exited, InitFailed, stage_name};
use crate::sanitize::clean_line;

/// `kiln` or infrastructure failed, including a boot timeout or a VM that
/// ended without `Exited`.
pub const EXIT_INFRA: i32 = 125;
/// The entrypoint could not be invoked (not executable, bad user, ...).
pub const EXIT_CANNOT_INVOKE: i32 = 126;
/// The entrypoint was not found.
pub const EXIT_NOT_FOUND: i32 = 127;

const ENOENT: i32 = 2;
/// The stage that starts the main process.
const PROCESS_STAGE: u8 = 6;

impl Exited {
    /// The app's exit code, or `128 + signal`.
    pub fn exit_code(&self) -> i32 {
        if self.signaled { 128 + self.code } else { self.code }
    }
}

impl InitFailed {
    /// 127 when exec(2) of the entrypoint found nothing, 126 for any other
    /// failure to start it, 125 for failures before it.
    pub fn exit_code(&self) -> i32 {
        match (self.stage, self.errno) {
            (PROCESS_STAGE, Some(ENOENT)) => EXIT_NOT_FOUND,
            (PROCESS_STAGE, _) => EXIT_CANNOT_INVOKE,
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
        assert_eq!(
            Exited {
                signaled: false,
                code: 3
            }
            .exit_code(),
            3
        );
        assert_eq!(
            Exited {
                signaled: true,
                code: 9
            }
            .exit_code(),
            137
        );
        assert_eq!(InitFailed::new(6, Some(2), "").exit_code(), 127);
        assert_eq!(InitFailed::new(6, Some(13), "").exit_code(), 126);
        assert_eq!(InitFailed::new(6, None, "no such user").exit_code(), 126);
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
