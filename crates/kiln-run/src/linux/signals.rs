//! kiln's own signals, forwarded to the session (spec §9.7): the first SIGINT or
//! SIGTERM asks the guest to stop, a second SIGINT kills the VM; SIGHUP, SIGQUIT,
//! SIGUSR1 and SIGUSR2 are forwarded to the main process, as Docker's sig-proxy
//! does; SIGWINCH resizes the guest's terminal in `-t` mode.
//!
//! The handlers are installed before the run starts ([`install_signals`]), so an
//! interrupt during setup is not lost or fatal: SIGINT or SIGTERM before a session
//! is attached is recorded ([`Forwarder::interrupted`]); setup checks it and gives
//! up, and if the session was already started it is delivered on
//! [`Forwarder::attach`]. Other signals before then have nothing to go to.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use kiln_proto::WindowSize;
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2, SIGWINCH};
use signal_hook::iterator::Signals;

use super::session::Handle;

/// What signals are forwarded to: the session's [`Handle`].
pub trait SignalTarget: Send {
    fn interrupt(&self);
    fn terminate(&self);
    fn signal(&self, sig: i32);
    fn window_size(&self, size: WindowSize);
}

impl SignalTarget for Handle {
    fn interrupt(&self) {
        Handle::interrupt(self);
    }

    fn terminate(&self) {
        Handle::terminate(self);
    }

    fn signal(&self, sig: i32) {
        Handle::signal(self, sig);
    }

    fn window_size(&self, size: WindowSize) {
        Handle::window_size(self, size);
    }
}

#[derive(Default)]
struct State {
    target: Option<Box<dyn SignalTarget>>,
    /// SIGINT or SIGTERM arrived with no target.
    interrupted: bool,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Forwards signals while alive. Dropping it ends kiln's handling of them, but the
/// signals' dispositions are not restored to their defaults: kiln is about to exit.
pub struct Forwarder {
    handle: signal_hook::iterator::Handle,
    thread: Option<JoinHandle<()>>,
    state: Arc<Mutex<State>>,
}

/// Installs kiln's handlers; signals are forwarded once a target is attached.
pub fn install_signals(tty: bool) -> std::io::Result<Forwarder> {
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT, SIGUSR1, SIGUSR2, SIGWINCH])?;
    let handle = signals.handle();
    let state = Arc::new(Mutex::new(State::default()));
    let shared = state.clone();
    let thread = std::thread::Builder::new().name("signals".into()).spawn(move || {
        for sig in signals.forever() {
            let mut state = lock(&shared);
            match (&state.target, sig) {
                (None, SIGINT | SIGTERM) => state.interrupted = true,
                (None, _) => {}
                (Some(t), SIGINT) => t.interrupt(),
                (Some(t), SIGTERM) => t.terminate(),
                (Some(t), SIGWINCH) => {
                    if tty && let Some(size) = super::tty::window_size() {
                        t.window_size(size);
                    }
                }
                (Some(t), other) => t.signal(other),
            }
        }
    })?;
    Ok(Forwarder {
        handle,
        thread: Some(thread),
        state,
    })
}

impl Forwarder {
    /// SIGINT or SIGTERM arrived before a target was attached.
    pub fn interrupted(&self) -> bool {
        lock(&self.state).interrupted
    }

    /// Forwards signals to `target` from now on; an interrupt that arrived before
    /// is delivered to it first.
    pub fn attach(&self, target: impl SignalTarget + 'static) {
        let mut state = lock(&self.state);
        if std::mem::take(&mut state.interrupted) {
            target.interrupt();
        }
        state.target = Some(Box::new(target));
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Sender, channel};
    use std::time::{Duration, Instant};

    struct Record(Mutex<Sender<String>>);

    impl SignalTarget for Record {
        fn interrupt(&self) {
            let _ = lock_tx(&self.0).send("interrupt".into());
        }

        fn terminate(&self) {
            let _ = lock_tx(&self.0).send("terminate".into());
        }

        fn signal(&self, sig: i32) {
            let _ = lock_tx(&self.0).send(format!("signal {sig}"));
        }

        fn window_size(&self, _: WindowSize) {}
    }

    fn lock_tx(m: &Mutex<Sender<String>>) -> std::sync::MutexGuard<'_, Sender<String>> {
        m.lock().unwrap()
    }

    fn raise(sig: rustix::process::Signal) {
        rustix::process::kill_process(rustix::process::getpid(), sig).unwrap();
    }

    /// An interrupt during setup is recorded, not fatal, and reaches the session
    /// once it is attached; later signals are forwarded.
    #[test]
    fn an_interrupt_before_the_session_is_kept_for_it() {
        let f = install_signals(false).unwrap();
        assert!(!f.interrupted());
        raise(rustix::process::Signal::INT);
        let until = Instant::now() + Duration::from_secs(10);
        while !f.interrupted() {
            assert!(Instant::now() < until, "SIGINT was not recorded");
            std::thread::sleep(Duration::from_millis(5));
        }
        let (tx, rx) = channel();
        f.attach(Record(Mutex::new(tx)));
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "interrupt");
        assert!(!f.interrupted());
        raise(rustix::process::Signal::USR1);
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "signal 10");
        raise(rustix::process::Signal::TERM);
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "terminate");
    }
}
