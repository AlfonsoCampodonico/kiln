//! Stage 6: start the services, then the main process (spec §9.6, §9.8): user and
//! groups from the image, environment, stdio on pipes relayed to vsock or on a
//! pty, working directory, exec. Each service is started in `Config` order and
//! must be ready before the next one starts; the main process starts last. The
//! main process's stdio is set up once and shared: the services get its stdout
//! and stderr (with `-t`, the pty as plain output, never their controlling
//! terminal) and `/dev/null` as stdin. Init keeps no copy of the processes' ends
//! of their stdio, so the relays see EOF once every process holding them has
//! exited. Execs start the same way, each with stdio of its own on port 1029
//! (kiln-shim exec and volumes design §3.2).

use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use std::io::Write;

use kiln_proto::{Config, ExecStart, ExecStream, HOST_CID, Process, Ready, WindowSize, port};
use rustix::fs::Mode;
use rustix::pipe::PipeFlags;
use rustix::process::{Gid, Pid, Uid, WaitOptions, WaitStatus};
use rustix::pty::OpenptFlags;
use rustix::termios::Winsize;
use signal_hook::consts::{SIGCHLD, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use super::sys::Vsock;
use super::{Event, host_ok, relay};
use crate::error::{Context, Failure, Result};
use crate::passwd::{Identity, resolve};

/// One process init supervises: a service, or the main process.
#[derive(Debug, Clone)]
pub struct Proc {
    /// The service's name; `None` for the main process.
    pub name: Option<String>,
    pub pid: Pid,
    pub stop_signal: i32,
}

/// The running services and main process.
pub struct Started {
    /// The services in start order, then the main process (always last).
    pub procs: Vec<Proc>,
    /// The relays carrying their output; joined to drain it.
    pub outputs: Vec<JoinHandle<()>>,
    /// The pty master in `tty` mode, for `WindowSize`.
    pub tty: Option<OwnedFd>,
}

/// A running exec.
pub struct Spawned {
    pub pid: Pid,
    /// The relays carrying its output; joined to drain it.
    pub outputs: Vec<JoinHandle<()>>,
    /// The pty master in `tty` mode, for `ExecWindowSize`.
    pub tty: Option<OwnedFd>,
}

/// A process's stdio once set up: the child's ends, the relays carrying them to the
/// host, and the pty master in `tty` mode.
struct Ends {
    /// The stdin pipe's read end with `interactive`, or the pty slave; else `/dev/null`.
    stdin: Option<File>,
    /// The stdout and stderr pipes' write ends, or the pty slave twice.
    stdout: File,
    stderr: File,
    outputs: Vec<JoinHandle<()>>,
    master: Option<OwnedFd>,
}

/// How long a readiness check waits between attempts, at most.
const READY_POLL: Duration = Duration::from_millis(50);

/// Starts the services, each ready before the next, then the main process.
/// `config_at` is when `Config` arrived: the services' readiness deadline
/// (`Config.readyTimeoutMs`) counts from it.
pub fn start(config: &Config, config_at: Instant, events: Sender<Event>) -> Result<Started> {
    let passwd = read_optional("/etc/passwd")?;
    let group = read_optional("/etc/group")?;
    let identity = resolve(config.process.user.as_deref(), &passwd, &group)?;
    let service_ids = config
        .services
        .iter()
        .map(|s| {
            resolve(s.process.user.as_deref(), &passwd, &group)
                .map_err(|f| Failure::msg(format!("service {}: {f}", s.name)))
        })
        .collect::<Result<Vec<_>>>()?;
    watch_signals(events)?;
    let workdir = create_workdir(&config.process, "")?;

    let main = |s: ExecStream| {
        connect(match s {
            ExecStream::Stdin => port::STDIN,
            ExecStream::Stdout => port::STDOUT,
            ExecStream::Stderr => port::STDERR,
            ExecStream::Tty => port::TTY,
        })
    };
    let ends = stdio(config.tty, config.interactive, &identity, &main)?;
    // A service running as another user must be able to reopen its stdout and
    // stderr (`/dev/stdout`) as the main process can.
    if config.tty.is_none() && service_ids.iter().any(|id| id.uid != identity.uid) {
        for f in [&ends.stdout, &ends.stderr] {
            rustix::fs::fchmod(f, Mode::from_raw_mode(0o622)).context("chmod the stdio")?;
        }
    }

    // Under `-t` the services write to the pty through a write-only descriptor of
    // their own: the read-write slave (terminal input, the main process's) is not theirs.
    let service_tty = match &ends.master {
        Some(master) => Some(File::from(service_output(master)?)),
        None => None,
    };
    let (service_out, service_err) = match &service_tty {
        Some(tty) => (tty, tty),
        None => (&ends.stdout, &ends.stderr),
    };
    // Under `-t` a service is not a session leader, so it can never take the pty as its
    // controlling terminal (the main process's, below).
    let service_session = if config.tty.is_some() {
        Session::Group
    } else {
        Session::Own
    };
    let deadline = config
        .ready_timeout_ms
        .map(|ms| config_at + Duration::from_millis(u64::from(ms)));
    let mut procs = Vec::new();
    for (svc, id) in config.services.iter().zip(&service_ids) {
        let what = format!("service {}: ", svc.name);
        let env = crate::env::build(&svc.process.env, &id.home, &config.hostname, false);
        let svc_workdir = create_workdir(&svc.process, &what)?;
        let stdout = service_out.try_clone().context(format!("{what}dup its stdout"))?;
        let stderr = service_err.try_clone().context(format!("{what}dup its stderr"))?;
        let child = (Stdio::null(), stdout.into(), stderr.into());
        let pid = launch(&svc.process, &env, id, svc_workdir, child, service_session, &what)?;
        procs.push(Proc {
            name: Some(svc.name.clone()),
            pid,
            stop_signal: svc.stop_signal,
        });
        wait_ready(&svc.name, &svc.ready, deadline, &procs)?;
    }
    // Init keeps no copy of the pty slave, or the terminal's output would never end.
    drop(service_tty);

    let env = crate::env::build(
        &config.process.env,
        &identity.home,
        &config.hostname,
        config.tty.is_some(),
    );
    let Ends {
        stdin,
        stdout,
        stderr,
        outputs,
        master,
    } = ends;
    let child = (
        stdin.map_or_else(Stdio::null, Stdio::from),
        stdout.into(),
        stderr.into(),
    );
    let pid = launch(
        &config.process,
        &env,
        &identity,
        workdir,
        child,
        terminal(config.tty.is_some()),
        "",
    )?;
    procs.push(Proc {
        name: None,
        pid,
        stop_signal: config.stop_signal,
    });
    Ok(Started {
        procs,
        outputs,
        tty: master,
    })
}

/// Starts exec `e`: each stream is a connection to port 1029 opened by its header.
pub fn exec(config: &Config, e: &ExecStart) -> Result<Spawned> {
    let id = e.id;
    let open = |s: ExecStream| {
        let mut v = connect(port::EXEC)?;
        v.write_all(&s.header(id)).context("send an exec stream's header")?;
        Ok(v)
    };
    let process = &e.process;
    let identity = resolve(
        process.user.as_deref(),
        &read_optional("/etc/passwd")?,
        &read_optional("/etc/group")?,
    )?;
    let env = crate::env::build(&process.env, &identity.home, &config.hostname, e.tty.is_some());
    let workdir = create_workdir(process, "")?;
    let Ends {
        stdin,
        stdout,
        stderr,
        outputs,
        master,
    } = stdio(e.tty, e.stdin, &identity, &open)?;
    let child = (
        stdin.map_or_else(Stdio::null, Stdio::from),
        stdout.into(),
        stderr.into(),
    );
    let pid = launch(process, &env, &identity, workdir, child, terminal(e.tty.is_some()), "")?;
    Ok(Spawned {
        pid,
        outputs,
        tty: master,
    })
}

/// Creates `process`'s working directory; returns it. `what` prefixes a failure.
fn create_workdir<'a>(process: &'a Process, what: &str) -> Result<&'a str> {
    let workdir = process.working_dir.as_deref().unwrap_or("/");
    std::fs::create_dir_all(workdir).context(format!("{what}create the working directory {workdir}"))?;
    Ok(workdir)
}

/// Sets up stdio for a process on the connections `connect` opens: a pty with
/// `tty`, else pipes (stdin a pipe only with `interactive`).
fn stdio(
    tty: Option<WindowSize>,
    interactive: bool,
    id: &Identity,
    connect: &dyn Fn(ExecStream) -> Result<Vsock>,
) -> Result<Ends> {
    match tty {
        Some(size) => pty_stdio(size, id, connect),
        None => pipe_stdio(interactive, id, connect),
    }
}

/// The pty slave under `-t` opened anew, write-only and never as a controlling
/// terminal: the services' stdout and stderr.
fn service_output(master: &OwnedFd) -> Result<OwnedFd> {
    let flags =
        OpenptFlags::from_bits_retain(rustix::fs::OFlags::WRONLY.bits()) | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    rustix::pty::ioctl_tiocgptpeer(master, flags).context("open the pty slave write-only")
}

/// How a child is placed among sessions and process groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Session {
    /// A session of its own, without a controlling terminal.
    Own,
    /// A session of its own with stdin, the pty slave, as its controlling terminal:
    /// the main process and execs under `-t`.
    Terminal,
    /// A process group of its own in init's session: a service under `-t`. Not being
    /// a session leader, it can never take a controlling terminal.
    Group,
}

/// The session of the main process or an exec, with a terminal or without.
fn terminal(tty: bool) -> Session {
    if tty { Session::Terminal } else { Session::Own }
}

/// Starts `process` with the given stdio ends, as `identity`, in `workdir`, placed
/// as `session` says. `what` prefixes failures (empty for the main process and execs).
fn launch(
    process: &Process,
    env: &[String],
    identity: &Identity,
    workdir: &str,
    (stdin, stdout, stderr): (Stdio, Stdio, Stdio),
    session: Session,
    what: &str,
) -> Result<Pid> {
    let argv = process.argv();
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(env.iter().filter_map(|e| e.split_once('=')))
        .current_dir(workdir)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);
    let (hook_failed, hook_report) =
        rustix::pipe::pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).context("pipe")?;
    drop_privileges(&mut cmd, identity, session, hook_report);
    let child = cmd.spawn().map_err(|e| match hook_step(&hook_failed) {
        // The error came from the setup before exec, not from exec itself.
        Some(step) => Failure::os(format!("{what}start {}: {step}", argv[0]), &e),
        None => Failure {
            errno: e.raw_os_error(),
            message: format!("{what}exec {}: {e}", argv[0]),
            exec: true,
        },
    })?;
    // Dropping the command closes init's copies of the child's stdio.
    drop(cmd);
    Ok(Pid::from_raw(child.id() as i32).expect("a child's pid is positive"))
}

/// Waits until service `name` is ready, failing when the deadline passes, when
/// any process started so far exits, or when the host has failed.
fn wait_ready(name: &str, ready: &Ready, deadline: Option<Instant>, procs: &[Proc]) -> Result<()> {
    let mut pause = Duration::from_millis(5);
    loop {
        for p in procs {
            if let Ok(Some((_, status))) = rustix::process::waitpid(Some(p.pid), WaitOptions::NOHANG) {
                let who = p.name.as_deref().unwrap_or("main");
                let when = if who == name {
                    "before it was ready".to_string()
                } else {
                    format!("while service {name} was starting")
                };
                return Err(Failure::msg(format!("service {who} {} {when}", describe(status))));
            }
        }
        if is_ready(ready, deadline) {
            return Ok(());
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Failure::msg(format!(
                "service {name} was not ready ({ready}) by the deadline the host set (its boot timeout)"
            )));
        }
        host_ok()?;
        std::thread::sleep(bounded(pause, deadline, Instant::now()));
        pause = (pause * 2).min(READY_POLL);
    }
}

/// `wait` cut to what is left until `deadline`, and at least 1 ms.
fn bounded(wait: Duration, deadline: Option<Instant>, now: Instant) -> Duration {
    match deadline {
        Some(d) => wait.min(d.saturating_duration_since(now)).max(Duration::from_millis(1)),
        None => wait,
    }
}

/// Whether `ready` holds now: a connection to the socket or port is accepted
/// (and closed at once), the attempt bounded by `deadline`.
fn is_ready(ready: &Ready, deadline: Option<Instant>) -> bool {
    match ready {
        Ready::None => true,
        Ready::Unix(path) => std::os::unix::net::UnixStream::connect(path).is_ok(),
        Ready::Tcp(port) => {
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], *port));
            let wait = bounded(Duration::from_millis(200), deadline, Instant::now());
            std::net::TcpStream::connect_timeout(&addr, wait).is_ok()
        }
    }
}

/// `exited with 3` or `was killed by signal 9`.
pub fn describe(status: WaitStatus) -> String {
    match (status.exit_status(), status.terminating_signal()) {
        (Some(code), _) => format!("exited with {code}"),
        (None, Some(sig)) => format!("was killed by signal {sig}"),
        (None, None) => "ended".into(),
    }
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

/// Gives the workload's user its stdio, as runc does: a non-root user could
/// otherwise not reopen `/dev/stdout` or `/proc/self/fd/N`.
fn give(fd: impl AsFd, id: &Identity) -> Result<()> {
    rustix::fs::fchown(fd, Some(Uid::from_raw(id.uid)), Some(Gid::from_raw(id.gid))).context("chown the stdio")
}

/// Pipes relayed to ports 1025–1027. Without `interactive`, stdin is `/dev/null`.
fn pipe_stdio(interactive: bool, id: &Identity, connect: &dyn Fn(ExecStream) -> Result<Vsock>) -> Result<Ends> {
    let stdin = if interactive {
        let (r, w) = pipe()?;
        give(&r, id)?;
        relay::spawn("stdin", connect(ExecStream::Stdin)?, w, drop)?;
        Some(r)
    } else {
        None
    };
    let mut outputs = Vec::new();
    let mut ends = Vec::new();
    for (name, stream) in [("stdout", ExecStream::Stdout), ("stderr", ExecStream::Stderr)] {
        let (r, w) = pipe()?;
        give(&w, id)?;
        outputs.push(relay::spawn(name, r, connect(stream)?, |v: Vsock| v.shutdown_write())?);
        ends.push(w);
    }
    let [stdout, stderr] = <[File; 2]>::try_from(ends).expect("two pipes");
    Ok(Ends {
        stdin,
        stdout,
        stderr,
        outputs,
        master: None,
    })
}

/// A pty from the guest's devpts, relayed to port 1028; its slave is the child's stdio.
fn pty_stdio(size: WindowSize, id: &Identity, connect: &dyn Fn(ExecStream) -> Result<Vsock>) -> Result<Ends> {
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    let master = rustix::pty::openpt(flags).context("open /dev/ptmx")?;
    rustix::pty::grantpt(&master).context("grantpt")?;
    rustix::pty::unlockpt(&master).context("unlockpt")?;
    set_window_size(&master, size)?;
    let slave = rustix::pty::ioctl_tiocgptpeer(&master, flags).context("open the pty slave")?;
    give(&slave, id)?;
    let vsock = connect(ExecStream::Tty)?;
    let dup = |fd: &OwnedFd| rustix::io::fcntl_dupfd_cloexec(fd, 0).context("dup the pty");
    relay::spawn(
        "tty-in",
        vsock.try_clone().context("dup the tty socket")?,
        File::from(dup(&master)?),
        drop,
    )?;
    // The terminal's end is the end of its output: its input relay stops too.
    let output = relay::spawn("tty-out", File::from(dup(&master)?), vsock, |v: Vsock| v.shutdown())?;
    Ok(Ends {
        stdin: Some(File::from(dup(&slave)?)),
        stdout: File::from(dup(&slave)?),
        stderr: File::from(slave),
        outputs: vec![output],
        master: Some(master),
    })
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

/// What the child's setup was doing when it failed, as `drop_privileges`'s hook
/// numbers its steps.
const HOOK_STEPS: [&str; 6] = [
    "setsid",
    "make the terminal the controlling terminal",
    "setgroups",
    "setresgid",
    "setresuid",
    "setpgid",
];

/// The step the pre-exec hook reported on `pipe`, if it failed (std reports a hook's
/// error exactly as an exec error, so the hook names its step on a pipe of its own).
fn hook_step(pipe: &OwnedFd) -> Option<&'static str> {
    let mut step = [0u8; 1];
    match rustix::io::read(pipe, &mut step) {
        Ok(1) => HOOK_STEPS.get(usize::from(step[0])).copied(),
        _ => None,
    }
}

/// In the child, after std has set up stdio and the working directory: a new
/// session (with the pty as controlling terminal for [`Session::Terminal`]) or, for
/// [`Session::Group`], a new process group, then groups, gid and uid. A failing step
/// writes its index in [`HOOK_STEPS`] to `report`.
fn drop_privileges(cmd: &mut Command, id: &Identity, session: Session, report: OwnedFd) {
    let groups: Vec<Gid> = id.groups.iter().map(|&g| Gid::from_raw(g)).collect();
    let (uid, gid) = (Uid::from_raw(id.uid), Gid::from_raw(id.gid));
    let hook = move || -> std::io::Result<()> {
        let step = |n: u8, r: rustix::io::Result<()>| {
            r.map_err(|e| {
                let _ = rustix::io::write(&report, &[n]);
                std::io::Error::from(e)
            })
        };
        match session {
            Session::Own => step(0, rustix::process::setsid().map(drop))?,
            Session::Terminal => {
                step(0, rustix::process::setsid().map(drop))?;
                step(1, rustix::process::ioctl_tiocsctty(rustix::stdio::stdin()))?;
            }
            Session::Group => step(5, rustix::process::setpgid(None, None))?,
        }
        // The child has one thread, so the per-thread calls apply to the process.
        step(2, rustix::thread::set_thread_groups(&groups))?;
        step(3, rustix::thread::set_thread_res_gid(gid, gid, gid))?;
        step(4, rustix::thread::set_thread_res_uid(uid, uid, uid))?;
        Ok(())
    };
    // SAFETY: the hook runs in the forked child before exec. It only makes raw
    // system calls through rustix on values moved in beforehand (a failure
    // writes one byte to a pipe and converts an errno, neither allocating): no
    // allocation, locks or other state shared with the parent's threads.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(hook);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_hook_step_is_named_and_exec_errors_are_not() {
        let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).unwrap();
        assert_eq!(hook_step(&r), None, "nothing reported: the error came from exec");
        rustix::io::write(&w, &[2]).unwrap();
        assert_eq!(hook_step(&r), Some("setgroups"));
        rustix::io::write(&w, &[200]).unwrap();
        assert_eq!(hook_step(&r), None);
    }

    /// The real hook, as a non-root user: setsid succeeds in the child and
    /// setgroups fails with EPERM, reported by the name `HOOK_STEPS` gives it, not
    /// as an exec error.
    #[test]
    fn a_refused_setgroups_is_named_by_the_hook() {
        if rustix::process::geteuid().is_root() {
            eprintln!("skipped: as root, setgroups would succeed");
            return;
        }
        let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).unwrap();
        let mut cmd = Command::new("/bin/true");
        let id = Identity {
            uid: 0,
            gid: 0,
            groups: vec![0],
            home: "/".into(),
        };
        drop_privileges(&mut cmd, &id, Session::Own, w);
        let err = cmd.spawn().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(rustix::io::Errno::PERM.raw_os_error()));
        assert_eq!(hook_step(&r), Some("setgroups"));
    }

    fn test_pty() -> OwnedFd {
        let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
        let master = rustix::pty::openpt(flags).unwrap();
        rustix::pty::grantpt(&master).unwrap();
        rustix::pty::unlockpt(&master).unwrap();
        master
    }

    /// Under `-t` the services' output is the pty opened write-only: they cannot read
    /// the terminal's input, which is the main process's.
    #[test]
    fn a_services_terminal_output_is_write_only() {
        let master = test_pty();
        let out = service_output(&master).unwrap();
        let mode = rustix::fs::fcntl_getfl(&out).unwrap() & rustix::fs::OFlags::RWMODE;
        assert_eq!(mode, rustix::fs::OFlags::WRONLY);
        assert_eq!(rustix::io::read(&out, &mut [0u8; 1]), Err(rustix::io::Errno::BADF));
        assert!(
            rustix::io::fcntl_getfd(&out)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        );
        rustix::io::write(&out, b"x").unwrap();
        let mut got = [0u8; 1];
        assert_eq!(rustix::io::read(&master, &mut got), Ok(1));
        assert_eq!(&got, b"x");
    }

    /// The hook as root, the way the services run under `-t`: a service in a process
    /// group of its own (not a session leader) that reopens its terminal output
    /// read-write without `O_NOCTTY` does not take the pty as its controlling
    /// terminal; in a session of its own it would (the control case).
    #[test]
    fn a_service_under_a_terminal_cannot_take_it_as_its_controlling_terminal() {
        if !rustix::process::geteuid().is_root() {
            eprintln!("skipped: the hook's setgroups needs root");
            return;
        }
        let tty_nr = |session: Session| {
            let master = test_pty();
            let (_r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).unwrap();
            let mut cmd = Command::new("/bin/sh");
            // Field 7 of /proc/<pid>/stat is the controlling terminal (0: none).
            cmd.args([
                "-c",
                "exec 3<>/proc/self/fd/2; read -r l < /proc/self/stat; set -- ${l#*) }; echo $5",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(File::from(service_output(&master).unwrap()));
            let id = Identity {
                uid: 0,
                gid: 0,
                groups: vec![0],
                home: "/".into(),
            };
            drop_privileges(&mut cmd, &id, session, w);
            let out = cmd.output().unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        assert_eq!(tty_nr(Session::Group), "0");
        assert_ne!(tty_nr(Session::Own), "0", "the control case took the terminal");
    }

    #[test]
    fn the_readiness_probe_never_waits_past_the_deadline() {
        let now = Instant::now();
        let d = Duration::from_millis(200);
        assert_eq!(bounded(d, None, now), d);
        assert_eq!(bounded(d, Some(now + Duration::from_secs(5)), now), d);
        assert_eq!(
            bounded(d, Some(now + Duration::from_millis(30)), now),
            Duration::from_millis(30)
        );
        // A deadline already passed still allows the shortest wait (connect_timeout refuses zero).
        assert_eq!(bounded(d, Some(now), now), Duration::from_millis(1));
    }
}
