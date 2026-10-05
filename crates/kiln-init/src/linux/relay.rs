//! Byte relays between the main process's stdio and vsock (spec §9.5): binary-safe,
//! and EOF on one side becomes EOF on the other.

use std::io::{ErrorKind, Read, Write};
use std::thread::JoinHandle;

use crate::error::{Context, Result};

/// Copies `from` to `to` until EOF or an error on either side, then hands `to`
/// to `finish` (which half-closes a socket or closes a pipe by dropping it).
/// A pty master reports EIO once no process holds its slave: that is its EOF.
pub fn spawn<R, W, F>(name: &str, mut from: R, mut to: W, finish: F) -> Result<JoinHandle<()>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    F: FnOnce(W) + Send + 'static,
{
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match from.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            finish(to);
        })
        .context(format!("spawn the {name} relay"))
}
