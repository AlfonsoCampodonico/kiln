//! `-t` on the host's terminal (spec §9.7): raw mode owned by kiln and restored on
//! every way out, including a panic and SIGTERM (which ends the session normally,
//! so the guard is dropped).

use std::sync::Mutex;

use kiln_proto::WindowSize;
use rustix::termios::{OptionalActions, Termios, isatty, tcgetattr, tcgetwinsize, tcsetattr};

/// The terminal's settings before raw mode, for the panic hook.
static SAVED: Mutex<Option<Termios>> = Mutex::new(None);

/// Raw mode on stdin while alive.
pub struct RawMode(());

impl RawMode {
    /// Puts stdin's terminal in raw mode; fails when stdin is not a terminal.
    pub fn enter() -> std::io::Result<Self> {
        let stdin = rustix::stdio::stdin();
        let saved = tcgetattr(stdin)?;
        let mut raw = saved.clone();
        raw.make_raw();
        *SAVED.lock().unwrap_or_else(|e| e.into_inner()) = Some(saved);
        install_panic_hook();
        tcsetattr(stdin, OptionalActions::Now, &raw)?;
        Ok(Self(()))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    if let Some(t) = SAVED.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, &t);
    }
}

fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let next = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            next(info);
        }));
    });
}

pub fn stdin_is_terminal() -> bool {
    isatty(rustix::stdio::stdin())
}

/// The size of the terminal on stdout (else stdin), if there is one.
pub fn window_size() -> Option<WindowSize> {
    [rustix::stdio::stdout(), rustix::stdio::stdin()]
        .into_iter()
        .filter(|fd| isatty(fd))
        .find_map(|fd| tcgetwinsize(fd).ok())
        .map(|w| WindowSize {
            rows: w.ws_row,
            cols: w.ws_col,
        })
}
