//! The host's view of the guest's message sequence (spec §9.5, T9). Every guest
//! message goes through [`Protocol::accept`]; anything out of order is a violation,
//! and the host kills the VM. The accepted sequence is
//!
//! `Hello`, `Stage` 3, 4, 5, 6 (strictly increasing, gaps allowed), `Running`,
//! `Stage` 7, `Exited`
//!
//! with `InitFailed` allowed once at any point after `Hello`, naming a stage no
//! earlier than the one the host has seen (kiln-init records a stage before
//! reporting it; `Running` counts as stage 7, so a guest cannot claim an exec
//! failure, 127 or 126, after its process ran). Nothing may follow `Exited`;
//! whatever follows `InitFailed` is ignored, so the init failure is what is
//! reported. So a guest gets at most nine messages accepted, and the host's
//! memory for a session stays bounded whatever the guest sends.

use kiln_proto::{EXIT_INFRA, Exited, GuestMessage, InitFailed, PROTOCOL_VERSION};

/// Why a message was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation(pub String);

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What [`Protocol::accept`] did with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    /// The first `Hello`: the host answers it with `Config`.
    Hello,
    /// Recorded.
    Recorded,
    /// Arrived after `InitFailed`, and dropped.
    Ignored,
}

/// What the guest has reported so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Protocol {
    hello: bool,
    /// The highest stage reported (2 once `Hello` arrived: it stands for stage 2).
    stage: u8,
    running: bool,
    exited: Option<Exited>,
    failed: Option<InitFailed>,
}

impl Protocol {
    /// Checks `msg` against the sequence and records it.
    pub fn accept(&mut self, msg: &GuestMessage) -> Result<Accepted, Violation> {
        let v = |s: String| Err(Violation(s));
        if self.failed.is_some() {
            // Init has failed and is ending the VM; its report stands.
            return Ok(Accepted::Ignored);
        }
        if self.terminal() {
            return v(format!("{} after the guest's final message", name(msg)));
        }
        match msg {
            GuestMessage::Hello(h) => {
                if self.hello {
                    return v("a second Hello".into());
                }
                if h.protocol != PROTOCOL_VERSION {
                    return v(format!("unsupported guest protocol {}", h.protocol));
                }
                self.hello = true;
                self.stage = 2;
                return Ok(Accepted::Hello);
            }
            _ if !self.hello => return v(format!("{} before Hello", name(msg))),
            GuestMessage::Stage(s) => {
                let n = s.n;
                if !(3..=7).contains(&n) || n <= self.stage {
                    return v(format!("Stage {n} after stage {}", self.stage));
                }
                if (n == 7) != self.running {
                    return v(format!(
                        "Stage {n} {} Running",
                        if self.running { "after" } else { "before" }
                    ));
                }
                self.stage = n;
            }
            GuestMessage::Running => {
                if self.running || self.stage != 6 {
                    return v(format!("Running at stage {}", self.stage));
                }
                self.running = true;
            }
            GuestMessage::Exited(e) => {
                if self.stage != 7 {
                    return v(format!("Exited at stage {}", self.stage));
                }
                self.exited = Some(*e);
            }
            GuestMessage::InitFailed(f) => {
                let seen = if self.running { 7 } else { self.stage };
                if f.stage < seen {
                    return v(format!("InitFailed at stage {} after stage {seen}", f.stage));
                }
                self.failed = Some(f.clone());
            }
        }
        Ok(Accepted::Recorded)
    }

    /// `Exited` or `InitFailed` arrived: the guest has nothing more to say.
    pub fn terminal(&self) -> bool {
        self.exited.is_some() || self.failed.is_some()
    }

    pub fn hello(&self) -> bool {
        self.hello
    }

    pub fn running(&self) -> bool {
        self.running
    }

    pub fn stage(&self) -> u8 {
        self.stage
    }

    pub fn exited(&self) -> Option<Exited> {
        self.exited
    }

    pub fn init_failed(&self) -> Option<&InitFailed> {
        self.failed.as_ref()
    }

    /// The exit code `kiln run` reports when the host saw no violation (spec §9.5):
    /// the app's, init's failure as Docker numbers it, or 125.
    pub fn exit_code(&self) -> i32 {
        match (&self.exited, &self.failed) {
            (Some(e), _) => e.exit_code(),
            (None, Some(f)) => f.exit_code(),
            (None, None) => EXIT_INFRA,
        }
    }
}

fn name(msg: &GuestMessage) -> &'static str {
    match msg {
        GuestMessage::Hello(_) => "Hello",
        GuestMessage::Stage(_) => "Stage",
        GuestMessage::Running => "Running",
        GuestMessage::Exited(_) => "Exited",
        GuestMessage::InitFailed(_) => "InitFailed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_proto::{Hello, Stage};

    fn hello() -> GuestMessage {
        GuestMessage::Hello(Hello { protocol: 1 })
    }

    fn stage(n: u8) -> GuestMessage {
        GuestMessage::Stage(Stage { n })
    }

    fn exited(code: i32) -> GuestMessage {
        GuestMessage::Exited(Exited { signaled: false, code })
    }

    fn failed(stage: u8, errno: Option<i32>) -> GuestMessage {
        GuestMessage::InitFailed(InitFailed::new(stage, errno, "x"))
    }

    fn run(msgs: &[GuestMessage]) -> Result<Protocol, (usize, Violation)> {
        let mut p = Protocol::default();
        for (i, m) in msgs.iter().enumerate() {
            p.accept(m).map_err(|v| (i, v))?;
        }
        Ok(p)
    }

    #[test]
    fn the_normal_sequence() {
        let all = [
            hello(),
            stage(3),
            stage(4),
            stage(5),
            stage(6),
            GuestMessage::Running,
            stage(7),
            exited(3),
        ];
        let p = run(&all).unwrap();
        assert!(p.terminal() && p.running());
        assert_eq!(p.exit_code(), 3);
        let mut q = Protocol::default();
        assert_eq!(q.accept(&hello()), Ok(Accepted::Hello));
        assert_eq!(q.accept(&stage(3)), Ok(Accepted::Recorded));
        // Gaps are allowed (the guest reports stages, they need not all be seen).
        run(&[hello(), stage(6), GuestMessage::Running, stage(7), exited(0)]).unwrap();
    }

    #[test]
    fn init_failures_end_the_sequence_anywhere_after_hello() {
        let all = [
            hello(),
            stage(3),
            stage(4),
            stage(5),
            stage(6),
            GuestMessage::Running,
            stage(7),
        ];
        for prefix in 1..=7 {
            let mut msgs = all[..prefix].to_vec();
            msgs.push(failed(7, None));
            assert_eq!(run(&msgs).unwrap().exit_code(), EXIT_INFRA);
            if prefix <= 5 {
                // Before Running, an exec failure at stage 6 is 127.
                msgs.pop();
                msgs.push(failed(6, Some(2)));
                assert_eq!(run(&msgs).unwrap().exit_code(), 127);
            }
            // Whatever follows InitFailed is ignored: the failure is reported.
            let mut p = run(&msgs).unwrap();
            let code = p.exit_code();
            for late in [stage(7), GuestMessage::Running, exited(0), failed(7, None), hello()] {
                assert_eq!(p.accept(&late), Ok(Accepted::Ignored), "{msgs:?} {late:?}");
            }
            assert_eq!(
                (p.exit_code(), p.init_failed()),
                (code, run(&msgs).unwrap().init_failed())
            );
        }
        assert_eq!(run(&[hello(), failed(3, None)]).unwrap().exit_code(), EXIT_INFRA);
    }

    /// kiln-init records a stage before reporting it, so its failure never names an
    /// earlier stage than the host saw; a guest claiming one is lying (for instance
    /// an exec failure, 127, after its process ran).
    #[test]
    fn init_failures_cannot_name_a_stage_already_passed() {
        let bad: [&[GuestMessage]; 4] = [
            &[hello(), stage(4), failed(3, None)],
            &[hello(), stage(6), GuestMessage::Running, failed(6, Some(2))],
            &[hello(), stage(6), GuestMessage::Running, stage(7), failed(6, Some(2))],
            &[hello(), failed(1, None)],
        ];
        for msgs in bad {
            let (at, why) = run(msgs).unwrap_err();
            assert_eq!(at, msgs.len() - 1, "{msgs:?}: {why}");
            assert!(why.0.starts_with("InitFailed at stage"), "{why}");
        }
        // The stage being entered, before it was reported, is fine.
        assert_eq!(
            run(&[hello(), stage(4), failed(5, None)]).unwrap().exit_code(),
            EXIT_INFRA
        );
        assert_eq!(run(&[hello(), stage(5), failed(6, Some(13))]).unwrap().exit_code(), 126);
    }

    #[test]
    fn out_of_order_messages_are_violations() {
        let bad: [&[GuestMessage]; 13] = [
            &[stage(3)],
            &[GuestMessage::Running],
            &[failed(2, None)],
            &[hello(), hello()],
            &[hello(), stage(3), stage(3)],
            &[hello(), stage(4), stage(3)],
            &[hello(), stage(2)],
            &[hello(), stage(8)],
            &[hello(), stage(5), GuestMessage::Running],
            &[hello(), stage(6), stage(7)],
            &[hello(), stage(6), GuestMessage::Running, GuestMessage::Running],
            &[hello(), stage(6), GuestMessage::Running, exited(0)],
            &[hello(), stage(6), GuestMessage::Running, stage(7), exited(0), exited(0)],
        ];
        for msgs in bad {
            let (at, why) = run(msgs).unwrap_err();
            assert_eq!(at, msgs.len() - 1, "{msgs:?}: {why}");
        }
        let (_, why) = run(&[GuestMessage::Hello(Hello { protocol: 2 })]).unwrap_err();
        assert!(why.0.contains("unsupported guest protocol 2"), "{why}");
    }

    #[test]
    fn a_stage_flood_is_refused_at_the_first_repeat() {
        let mut p = Protocol::default();
        p.accept(&hello()).unwrap();
        p.accept(&stage(3)).unwrap();
        assert!(p.accept(&stage(3)).is_err());
    }

    #[test]
    fn without_a_final_message_the_run_failed() {
        assert_eq!(Protocol::default().exit_code(), EXIT_INFRA);
        let p = run(&[hello(), stage(6), GuestMessage::Running]).unwrap();
        assert_eq!(p.exit_code(), EXIT_INFRA);
    }
}
