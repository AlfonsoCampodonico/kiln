//! kiln's own signals, forwarded to the session (spec §9.7): the first SIGINT or
//! SIGTERM asks the guest to stop, a second SIGINT kills the VM; SIGHUP, SIGQUIT,
//! SIGUSR1 and SIGUSR2 are forwarded to the main process, as Docker's sig-proxy
//! does; SIGWINCH resizes the guest's terminal in `-t` mode.
//!
//! The handlers are installed before the run starts ([`install_signals`]), so an
//! interrupt during setup is not lost or fatal: SIGINT, SIGTERM or SIGHUP (the
//! terminal or ssh connection went away) before a session is attached is recorded
//! ([`Forwarder::interrupted`]); setup checks it and gives up, and if the session
//! was already started it is delivered on [`Forwarder::attach`] as a stop request.
//! SIGQUIT, SIGUSR1 and SIGUSR2 before then are not forwarded later: no process
//! runs yet that could get them, and delivering a stale signal to the app once it
//! starts would surprise it. They are dropped, and [`Forwarder::dropped`] names
//! them so `kiln run` can say so. SIGWINCH before then needs nothing: the
//! terminal's size is read when the session's `Config` is built.

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
    /// SIGINT, SIGTERM or SIGHUP arrived with no target.
    interrupted: bool,
    /// Signals to forward that arrived with no target, in order (each once).
    dropped: Vec<i32>,
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
                (None, SIGINT | SIGTERM | SIGHUP) => state.interrupted = true,
                (None, SIGWINCH) => {}
                (None, other) => {
                    if !state.dropped.contains(&other) {
                        state.dropped.push(other);
                    }
                }
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
    /// SIGINT, SIGTERM or SIGHUP arrived before a target was attached.
    pub fn interrupted(&self) -> bool {
        lock(&self.state).interrupted
    }

    /// The names of the signals (SIGQUIT, SIGUSR1, SIGUSR2) that arrived before a
    /// target was attached and were dropped; taken, so each is reported once.
    pub fn dropped(&self) -> Vec<&'static str> {
        std::mem::take(&mut lock(&self.state).dropped)
            .into_iter()
            .map(|sig| match sig {
                SIGQUIT => "SIGQUIT",
                SIGUSR1 => "SIGUSR1",
                SIGUSR2 => "SIGUSR2",
                _ => "a signal",
            })
            .collect()
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

    /// Signals go to the whole test process: one test at a time raises them.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn raise(sig: rustix::process::Signal) {
        rustix::process::kill_process(rustix::process::getpid(), sig).unwrap();
    }

    /// An interrupt during setup is recorded, not fatal, and reaches the session
    /// once it is attached; later signals are forwarded.
    #[test]
    fn an_interrupt_before_the_session_is_kept_for_it() {
        let _serial = serial();
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

    /// Before a session: SIGHUP (the terminal went away) stops the setup as SIGTERM
    /// does, and SIGQUIT is dropped and named, never delivered later.
    #[test]
    fn a_hangup_before_the_session_interrupts_and_a_quit_is_dropped() {
        let _serial = serial();
        let f = install_signals(false).unwrap();
        raise(rustix::process::Signal::QUIT);
        raise(rustix::process::Signal::QUIT);
        let until = Instant::now() + Duration::from_secs(10);
        while lock(&f.state).dropped.is_empty() {
            assert!(Instant::now() < until, "SIGQUIT was not recorded");
            std::thread::sleep(Duration::from_millis(5));
        }
        raise(rustix::process::Signal::HUP);
        while !f.interrupted() {
            assert!(Instant::now() < until, "SIGHUP was not recorded");
            std::thread::sleep(Duration::from_millis(5));
        }
        let (tx, rx) = channel();
        f.attach(Record(Mutex::new(tx)));
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "interrupt");
        assert_eq!(f.dropped(), ["SIGQUIT"]);
        assert!(f.dropped().is_empty());
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "SIGQUIT was delivered late"
        );
        raise(rustix::process::Signal::QUIT);
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "signal 3");
    }
}
