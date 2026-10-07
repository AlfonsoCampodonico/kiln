//! The boot tests' view of the production session (`kiln_run::Session`): the
//! guest's output captured in memory, stdin from bytes or a pipe for the terminal.

use std::io::{Cursor, PipeWriter, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kiln_proto::{Config, Exited, HostMessage, InitFailed};
use kiln_run::{SessionOptions, Streams};
use vmkit::{Vm, VmEnd};

/// What the host answers the first `Hello` with.
#[derive(Debug, Clone)]
pub enum Reply {
    Config,
    /// Raw bytes instead of a `Config` frame, to test the guest's side of T9.
    Raw(Vec<u8>),
}

/// A sink the test reads while the session writes it.
#[derive(Clone, Default)]
pub struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Shared {
    pub fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes()).into_owned()
    }
}

/// How the run ended.
#[derive(Debug)]
pub struct Outcome {
    pub run: kiln_run::Outcome,
    pub violation: Option<String>,
    pub end: VmEnd,
    /// stdout, or the terminal in tty mode.
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Outcome {
    pub fn exited(&self) -> Option<Exited> {
        self.run.exited
    }

    pub fn init_failed(&self) -> Option<&InitFailed> {
        self.run.init_failed.as_ref()
    }

    pub fn stages(&self) -> Vec<u8> {
        self.run.stages.clone()
    }

    pub fn running(&self) -> bool {
        self.run.running
    }

    pub fn exit_code(&self) -> i32 {
        self.run.exit_code
    }

    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    pub fn tty(&self) -> String {
        self.stdout()
    }
}

/// One running VM.
pub struct Session {
    inner: kiln_run::Session,
    out: Shared,
    err: Shared,
    tty_in: Option<PipeWriter>,
}

impl Session {
    pub fn start(vm: Box<dyn Vm>, config: Config, reply: Reply, stdin: Vec<u8>, opts: SessionOptions) -> Self {
        let (out, err) = (Shared::default(), Shared::default());
        let mut tty_in = None;
        let input: Option<Box<dyn Read + Send>> = match (config.interactive, config.tty.is_some()) {
            (true, true) => {
                let (r, w) = std::io::pipe().unwrap();
                tty_in = Some(w);
                Some(Box::new(r))
            }
            (true, false) => Some(Box::new(Cursor::new(stdin))),
            (false, _) => None,
        };
        let streams = Streams {
            stdin: input,
            stdout: Box::new(out.clone()),
            stderr: Box::new(err.clone()),
        };
        let opts = SessionOptions {
            reply: match reply {
                Reply::Config => None,
                Reply::Raw(b) => Some(b),
            },
            ..opts
        };
        let inner = kiln_run::Session::start(vm, config, streams, opts).expect("start the session");
        Self {
            inner,
            out,
            err,
            tty_in,
        }
    }

    pub fn handle(&self) -> kiln_run::Handle {
        self.inner.handle()
    }

    /// Waits for `Running`; false if the VM ended or failed first.
    pub fn wait_running(&mut self, timeout: Duration) -> bool {
        self.inner.pump(timeout, |s| s.running())
    }

    /// Waits until stdout (or the terminal) contains `needle`.
    pub fn wait_output(&mut self, needle: &str, timeout: Duration) -> bool {
        let out = self.out.clone();
        self.inner.pump(timeout, |_| out.text().contains(needle))
    }

    pub fn send(&self, msg: &HostMessage) -> Result<(), String> {
        self.inner.handle().send(msg.clone());
        Ok(())
    }

    pub fn send_raw(&self, bytes: &[u8]) -> Result<(), String> {
        self.inner.handle().send_raw(bytes.to_vec());
        Ok(())
    }

    pub fn tty_write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.tty_in
            .as_mut()
            .ok_or_else(|| std::io::Error::other("no terminal in this config"))?
            .write_all(bytes)
    }

    /// Runs to the end (killing the VM after `limit`), then collects the output.
    pub fn finish(self, limit: Duration) -> Outcome {
        let Session {
            inner,
            out,
            err,
            tty_in,
        } = self;
        drop(tty_in);
        let run = inner.finish_within(Some(limit));
        Outcome {
            violation: run.violation.clone(),
            end: run.end,
            run,
            stdout: out.bytes(),
            stderr: err.bytes(),
        }
    }
}
