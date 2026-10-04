//! Stage 6: start the main process (spec §9.6): user and groups from the image,
//! environment, stdio on pipes relayed to vsock or on a pty, working directory,
//! exec. Init keeps no copy of the child's ends of its stdio, so the relays see
//! EOF once every process holding them has exited.

use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use kiln_proto::{Config, HOST_CID, WindowSize, port};
use rustix::process::{Gid, Pid, Uid};
use rustix::pty::OpenptFlags;
use rustix::termios::Winsize;
use signal_hook::consts::{SIGCHLD, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use super::sys::Vsock;
use super::{Event, relay};
use crate::error::{Context, Failure, Result};
use crate::passwd::{Identity, resolve};

/// The running main process.
pub struct Started {
    pub pid: Pid,
    /// The relays carrying its output; joined to drain it.
    pub outputs: Vec<JoinHandle<()>>,
    /// The pty master in `tty` mode, for `WindowSize`.
    pub tty: Option<OwnedFd>,
}

pub fn start(config: &Config, events: Sender<Event>) -> Result<Started> {
    let identity = resolve(
        config.process.user.as_deref(),
        &read_optional("/etc/passwd")?,
        &read_optional("/etc/group")?,
    )?;
    let env = crate::env::build(
        &config.process.env,
        &identity.home,
        &config.hostname,
        config.tty.is_some(),
    );
    let workdir = config.process.working_dir.as_deref().unwrap_or("/");
    std::fs::create_dir_all(workdir).context(format!("create the working directory {workdir}"))?;
    watch_signals(events)?;

    let argv = config.process.argv();
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(env.iter().filter_map(|e| e.split_once('=')))
        .current_dir(workdir);
    let (outputs, tty) = match config.tty {
        Some(size) => {
            let (master, outputs) = pty_stdio(&mut cmd, size)?;
            (outputs, Some(master))
        }
        None => (pipe_stdio(&mut cmd, config.interactive)?, None),
    };
    drop_privileges(&mut cmd, &identity, config.tty.is_some());
    let child = cmd.spawn().map_err(|e| Failure {
        errno: e.raw_os_error(),
        message: format!("exec {}: {e}", argv[0]),
        exec: true,
    })?;
    // Dropping the command closes init's copies of the child's stdio.
    drop(cmd);
    Ok(Started {
        pid: Pid::from_raw(child.id() as i32).expect("a child's pid is positive"),
        outputs,
        tty,
    })
}

/// A file's contents, or nothing if it does not exist.
fn read_optional(path: &str) -> Result<String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(Failure::os(format!("read {path}"), &e)),
    }
}

/// SIGCHLD, and SIGINT (Ctrl-Alt-Del) or SIGTERM as `Shutdown`, go to the supervisor.
fn watch_signals(events: Sender<Event>) -> Result<()> {
    let mut signals = Signals::new([SIGCHLD, SIGINT, SIGTERM]).context("install signal handlers")?;
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            for sig in signals.forever() {
                if events.send(Event::Signal(sig)).is_err() {
                    return;
                }
            }
        })
        .context("spawn the signal thread")?;
    Ok(())
}

fn connect(port: u32) -> Result<Vsock> {
    Vsock::connect(HOST_CID, port).context(format!("connect to the host on vsock port {port}"))
}

fn pipe() -> Result<(File, File)> {
    let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).context("pipe")?;
    Ok((File::from(r), File::from(w)))
}

/// Pipes relayed to ports 1025–1027. Without `interactive`, stdin is `/dev/null`.
fn pipe_stdio(cmd: &mut Command, interactive: bool) -> Result<Vec<JoinHandle<()>>> {
    if interactive {
        let (r, w) = pipe()?;
        relay::spawn("stdin", connect(port::STDIN)?, w, drop)?;
        cmd.stdin(r);
    } else {
        cmd.stdin(Stdio::null());
    }
    let mut outputs = Vec::new();
    for (name, port) in [("stdout", port::STDOUT), ("stderr", port::STDERR)] {
        let (r, w) = pipe()?;
        outputs.push(relay::spawn(name, r, connect(port)?, |v: Vsock| v.shutdown_write())?);
        if port == port::STDOUT {
            cmd.stdout(w);
        } else {
            cmd.stderr(w);
        }
    }
    Ok(outputs)
}

/// A pty from the guest's devpts, relayed to port 1028; its slave is the child's stdio.
fn pty_stdio(cmd: &mut Command, size: WindowSize) -> Result<(OwnedFd, Vec<JoinHandle<()>>)> {
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    let master = rustix::pty::openpt(flags).context("open /dev/ptmx")?;
    rustix::pty::grantpt(&master).context("grantpt")?;
    rustix::pty::unlockpt(&master).context("unlockpt")?;
    set_window_size(&master, size)?;
    let slave = rustix::pty::ioctl_tiocgptpeer(&master, flags).context("open the pty slave")?;
    let vsock = connect(port::TTY)?;
    let dup = |fd: &OwnedFd| rustix::io::fcntl_dupfd_cloexec(fd, 0).context("dup the pty");
    relay::spawn(
        "tty-in",
        vsock.try_clone().context("dup the tty socket")?,
        File::from(dup(&master)?),
        drop,
    )?;
    let output = relay::spawn("tty-out", File::from(dup(&master)?), vsock, |v: Vsock| {
        v.shutdown_write()
    })?;
    cmd.stdin(File::from(dup(&slave)?))
        .stdout(File::from(dup(&slave)?))
        .stderr(File::from(slave));
    Ok((master, vec![output]))
}

pub fn set_window_size(master: &OwnedFd, size: WindowSize) -> Result<()> {
    let ws = Winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    rustix::termios::tcsetwinsize(master, ws).context("set the window size")
}

/// In the child, after std has set up stdio and the working directory: a new
/// session (with the pty as controlling terminal in `tty` mode), then groups,
/// gid and uid.
fn drop_privileges(cmd: &mut Command, id: &Identity, tty: bool) {
    let groups: Vec<Gid> = id.groups.iter().map(|&g| Gid::from_raw(g)).collect();
    let (uid, gid) = (Uid::from_raw(id.uid), Gid::from_raw(id.gid));
    let hook = move || -> std::io::Result<()> {
        rustix::process::setsid()?;
        if tty {
            rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
        }
        // The child has one thread, so the per-thread calls apply to the process.
        rustix::thread::set_thread_groups(&groups)?;
        rustix::thread::set_thread_res_gid(gid, gid, gid)?;
        rustix::thread::set_thread_res_uid(uid, uid, uid)?;
        Ok(())
    };
    // SAFETY: the hook runs in the forked child before exec. It only makes raw
    // system calls through rustix on values moved in beforehand: no allocation,
    // locks or other state shared with the parent's threads.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(hook);
    }
}
