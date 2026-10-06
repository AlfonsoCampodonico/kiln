//! The Linux runtime: vmkit, the session, the terminal and signals.

mod session;

pub use session::{EXIT_KILLED, Handle, Outcome, Session, SessionOptions, Streams};
