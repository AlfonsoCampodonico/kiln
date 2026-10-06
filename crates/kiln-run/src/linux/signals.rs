//! kiln's own signals, forwarded to the session (spec §9.7): the first SIGINT or
//! SIGTERM asks the guest to stop, a second SIGINT kills the VM; SIGHUP, SIGQUIT,
//! SIGUSR1 and SIGUSR2 are forwarded to the main process, as Docker's sig-proxy
//! does; SIGWINCH resizes the guest's terminal in `-t` mode.

use std::thread::JoinHandle;

use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2, SIGWINCH};
use signal_hook::iterator::Signals;

use super::session::Handle;

/// Forwards signals while alive. Dropping it ends kiln's handling of them, but the
/// signals' dispositions are not restored to their defaults: kiln is about to exit.
pub struct Forwarder {
    handle: signal_hook::iterator::Handle,
    thread: Option<JoinHandle<()>>,
}

pub fn forward_signals(session: Handle, tty: bool) -> std::io::Result<Forwarder> {
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT, SIGUSR1, SIGUSR2, SIGWINCH])?;
    let handle = signals.handle();
    let thread = std::thread::Builder::new().name("signals".into()).spawn(move || {
        for sig in signals.forever() {
            match sig {
                SIGINT => session.interrupt(),
                SIGTERM => session.terminate(),
                SIGWINCH => {
                    if tty && let Some(size) = super::tty::window_size() {
                        session.window_size(size);
                    }
                }
                other => session.signal(other),
            }
        }
    })?;
    Ok(Forwarder {
        handle,
        thread: Some(thread),
    })
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
