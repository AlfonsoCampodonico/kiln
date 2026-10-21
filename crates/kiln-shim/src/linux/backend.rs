//! The Linux [`Backend`] (design §8): the shim, as root, spawns the pause process for a
//! pod's sandbox, attaches the pod netns once, and spawns a VM worker per container
//! with its stdio FIFOs and its plan.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};

use std::os::fd::OwnedFd;

use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};

use super::plan::WorkerPlan;
use super::{FD_ENV, STATE, attach};
use crate::error::{Error, Result};
use crate::netconf::{self, PodNet};
use crate::passwd::{self, Account};
use crate::service::VmLaunch;
use crate::tasks::{Backend, CreateTask, EXIT_LOST, Launch, Process, SIGKILL};
use crate::wire::{FromWorker, Lines, ToWorker, send};

/// The pod netns attachments this shim made, by the netns's identity (one shim serves
/// one pod, so they live as long as it does).
type Attached = HashMap<(u64, u64), Option<PodNet>>;

type Spawn = (Command, std::sync::mpsc::Sender<std::io::Result<Child>>);

pub struct LinuxBackend {
    attached: Mutex<Attached>,
    /// The spawner thread's queue. A child's `PR_SET_PDEATHSIG` fires when the *thread*
    /// that forked it ends, and ttrpc's handler threads come and go, so every child is
    /// forked by one thread that lives as long as the shim. It also spawns one child at a
    /// time, so a worker's channel fd, which must be inheritable, reaches no other child.
    spawner: Mutex<std::sync::mpsc::Sender<Spawn>>,
}

impl LinuxBackend {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Spawn>();
        std::thread::Builder::new()
            .name("spawner".into())
            .spawn(move || {
                for (mut cmd, reply) in rx {
                    let _ = reply.send(cmd.spawn());
                }
            })
            .expect("the spawner thread");
        Self {
            attached: Mutex::new(HashMap::new()),
            spawner: Mutex::new(tx),
        }
    }

    /// `cmd`, spawned by the spawner thread. `inherit` is made inheritable for this
    /// child only: no other spawn runs until it has been forked.
    fn spawn(&self, cmd: Command, inherit: Option<&UnixStream>) -> Result<Child> {
        let gone = || Error::Internal("the spawner thread is gone".into());
        let spawner = self.spawner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(fd) = inherit {
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty())
                .map_err(|e| Error::Internal(format!("the worker's channel: {e}")))?;
        }
        let (reply, child) = std::sync::mpsc::channel();
        spawner.send((cmd, reply)).map_err(|_| gone())?;
        child.recv().map_err(|_| gone())?.map_err(Error::from)
    }

    fn attach(&self, netns: &Path, v: &VmLaunch, account: &Account) -> Result<Option<PodNet>> {
        let m = fs::metadata(netns).map_err(|e| Error::Internal(format!("the pod netns {}: {e}", netns.display())))?;
        let key = (m.dev(), m.ino());
        let mut attached = self.attached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pod) = attached.get(&key) {
            return Ok(pod.clone());
        }
        let node = attach::node_addresses()?;
        let pod = attach::attach(netns, account, &v.settings.egress_allow, &node)?;
        attached.insert(key, pod.clone());
        Ok(pod)
    }
}

impl Default for LinuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn exe() -> Result<PathBuf> {
    std::env::current_exe().map_err(|e| Error::Internal(format!("finding the shim's binary: {e}")))
}

fn code(status: ExitStatus) -> u32 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(c), _) => c as u32,
        (None, Some(s)) => 128 + s as u32,
        _ => EXIT_LOST,
    }
}

/// A pidfd for a child just spawned (not yet reaped, so the pid is still its own).
fn pidfd(pid: u32) -> Result<OwnedFd> {
    let p = Pid::from_raw(pid as i32).ok_or_else(|| Error::Internal("no pid".into()))?;
    pidfd_open(p, PidfdFlags::empty()).map_err(|e| Error::Internal(format!("pidfd for {pid}: {e}")))
}

/// Signals a child through its pidfd: once it is reaped the pidfd stays its own, and a
/// signal can never reach another process that reused the pid.
fn signal_pidfd(fd: &OwnedFd, sig: i32) -> Result<()> {
    let sig = Signal::from_named_raw(sig).ok_or_else(|| Error::invalid(format!("signal {sig}")))?;
    match pidfd_send_signal(fd, sig) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(e) => Err(Error::Internal(format!("signalling a child: {e}"))),
    }
}

/// A directory (and its missing parents) with `mode`.
fn dir(path: &Path, mode: u32) -> Result<()> {
    fs::DirBuilder::new().recursive(true).mode(mode).create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// A FIFO for writing that never blocks on a missing reader (read+write), or /dev/null.
fn output(path: &str) -> Result<Stdio> {
    if path.is_empty() {
        return Ok(Stdio::null());
    }
    Ok(OpenOptions::new().read(true).write(true).open(path)?.into())
}

/// The pod sandbox's process.
struct Pause {
    pid: u32,
    pidfd: OwnedFd,
    child: Mutex<Child>,
}

impl Process for Pause {
    fn pid(&self) -> u32 {
        self.pid
    }
    fn start(&self) -> Result<()> {
        Ok(())
    }
    fn signal(&self, sig: i32) -> Result<()> {
        signal_pidfd(&self.pidfd, sig)
    }
    fn resize(&self, _rows: u16, _cols: u16) -> Result<()> {
        Ok(())
    }
    fn close_stdin(&self) -> Result<()> {
        Ok(())
    }
    fn wait(&self) -> u32 {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        child.wait().map_or(EXIT_LOST, code)
    }
}

/// What the shim heard from a worker after `ready`.
#[derive(Default)]
struct Heard {
    started: Option<std::result::Result<u32, String>>,
    exited: Option<i32>,
    closed: bool,
}

/// A container's VM worker.
struct Worker {
    pid: u32,
    pidfd: OwnedFd,
    child: Mutex<Child>,
    tx: Mutex<UnixStream>,
    heard: Arc<(Mutex<Heard>, Condvar)>,
    vm_dir: PathBuf,
    /// The container's cgroup leaf the worker made, removed once it is empty.
    cgroup: Option<PathBuf>,
}

impl Worker {
    fn tell(&self, m: &ToWorker) -> Result<()> {
        let mut tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        send(&mut *tx, m).map_err(|e| Error::Internal(format!("the VM worker: {e}")))
    }
}

impl Process for Worker {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn start(&self) -> Result<()> {
        self.tell(&ToWorker::Start {})?;
        let (lock, cv) = &*self.heard;
        let mut h = lock.lock().unwrap_or_else(|e| e.into_inner());
        while h.started.is_none() && h.exited.is_none() && !h.closed {
            h = cv.wait(h).unwrap_or_else(|e| e.into_inner());
        }
        match &h.started {
            Some(Ok(_)) => Ok(()),
            Some(Err(m)) => Err(Error::Internal(m.clone())),
            None => Err(Error::Internal("the VM worker ended before the VM started".into())),
        }
    }

    fn signal(&self, sig: i32) -> Result<()> {
        let m = if sig == SIGKILL {
            ToWorker::Kill {}
        } else {
            ToWorker::Signal(sig)
        };
        if self.tell(&m).is_err() {
            // The channel is gone: end the worker itself (the VMM dies with it).
            return signal_pidfd(&self.pidfd, SIGKILL);
        }
        Ok(())
    }

    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.tell(&ToWorker::Resize { rows, cols })
    }

    fn close_stdin(&self) -> Result<()> {
        self.tell(&ToWorker::CloseStdin {})
    }

    fn wait(&self) -> u32 {
        let status = self.child.lock().unwrap_or_else(|e| e.into_inner()).wait();
        let (lock, cv) = &*self.heard;
        let mut h = lock.lock().unwrap_or_else(|e| e.into_inner());
        while !h.closed {
            h = cv.wait(h).unwrap_or_else(|e| e.into_inner());
        }
        // The worker's mounts went with its mount namespace, and it removed its scratch disk
        // itself (as the kiln user: root deletes nothing in that user's store). Its directory
        // here is under root's own.
        let _ = fs::remove_dir_all(&self.vm_dir);
        if let Some(cg) = &self.cgroup {
            // Empty now that the worker and the VMM are gone; kubelet removes the pod's.
            let _ = fs::remove_dir(cg);
        }
        // Only the worker's own report is the guest's exit: a worker that crashed (or
        // was killed) without it says nothing about the guest.
        let _ = status;
        h.exited.map_or(EXIT_LOST, |c| c as u32)
    }
}

impl LinuxBackend {
    fn pause(&self, netns: Option<&Path>) -> Result<Arc<dyn Process>> {
        let mut cmd = Command::new(exe()?);
        cmd.arg("pause")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(n) = netns {
            cmd.arg(n);
        }
        let child = self.spawn(cmd, None)?;
        Ok(Arc::new(Pause {
            pid: child.id(),
            pidfd: pidfd(child.id())?,
            child: Mutex::new(child),
        }))
    }

    fn vm(&self, task: &CreateTask, v: &VmLaunch) -> Result<Arc<dyn Process>> {
        let read = |p: &str| fs::read_to_string(p).map_err(|e| Error::Config(format!("{p}: {e}")));
        let account = passwd::lookup(&read("/etc/passwd")?, &read("/etc/group")?, &v.settings.user)?;
        let pod = match &v.netns {
            Some(ns) => self.attach(ns, v, &account)?,
            None => None,
        };
        let state = Path::new(STATE);
        // Traversable (not listable) by the kiln user, whose own directory is inside.
        dir(state, 0o711)?;
        dir(&state.join("vm"), 0o711)?;
        let name = crate::service::task_dir_name(&v.namespace, &v.task_id);
        let vm_dir = state.join("vm").join(&name);
        // The worker does root work in it (the layer mount points) before it hands it to the
        // kiln user, so it starts fresh and root's own: a leftover (a crashed shim's) may hold
        // that user's symlinks, and `create` fails rather than reuse whatever raced in.
        // `remove_dir_all` does not follow symlinks.
        let _ = fs::remove_dir_all(&vm_dir);
        fs::DirBuilder::new().mode(0o700).create(&vm_dir)?;
        let plan = WorkerPlan {
            task_id: v.task_id.clone(),
            account: account.clone(),
            kernel: v.settings.kernel.clone(),
            init: v.settings.init.clone(),
            vmm: v.settings.vmm.name().into(),
            cpus: v.resources.cpus,
            memory_mib: v.resources.memory_mib,
            boot_timeout_secs: v.settings.boot_timeout.as_secs(),
            layers: v.layers.clone(),
            process: v.process.clone(),
            hostname: v.hostname.clone(),
            tty: v.tty,
            interactive: v.interactive,
            netns: v.netns.clone(),
            network: pod.as_ref().map(|p| netconf::guest_network(p, &v.resolv)),
            pod,
            cgroup: v.cgroup.clone(),
            vm_dir: vm_dir.clone(),
            scratch_dir: v.settings.store.join("shim-scratch").join(&name),
            scratch_bytes: v.settings.scratch_disk,
            warnings: v.warnings.clone(),
        };
        let stdin = if task.stdio.stdin.is_empty() {
            Stdio::null()
        } else {
            // containerd opened the writer side already; this returns once it is there.
            File::open(&task.stdio.stdin)?.into()
        };
        let (mut ours, theirs) = UnixStream::pair()?;
        let mut cmd = Command::new(exe()?);
        cmd.arg("vm")
            .env(FD_ENV, theirs.as_raw_fd().to_string())
            .env("HOME", &account.home)
            .env("USER", &account.name)
            .env("LOGNAME", &account.name)
            .env("KILN_HOME", &v.settings.store)
            .stdin(stdin)
            .stdout(output(&task.stdio.stdout)?)
            .stderr(output(&task.stdio.stderr)?);
        // Its end is closed in the shim as soon as it is forked, so no later child gets it.
        let spawned = self.spawn(cmd, Some(&theirs));
        drop(theirs);
        let child = spawned?;
        let pid = child.id();
        let worker_pidfd = pidfd(pid)?;
        let mut child = child;
        let failed = |child: &mut Child, msg: String| {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_dir_all(&plan.vm_dir);
            Error::Internal(msg)
        };
        if let Err(e) = send(&mut ours, &plan) {
            return Err(failed(&mut child, format!("sending the VM worker its plan: {e}")));
        }
        let mut lines = Lines::new(ours.try_clone()?);
        match lines.read::<FromWorker>() {
            Ok(Some(FromWorker::Ready {})) => {}
            Ok(Some(FromWorker::Failed { message })) => return Err(failed(&mut child, message)),
            Ok(other) => {
                return Err(failed(
                    &mut child,
                    format!("the VM worker ended before it was ready ({other:?})"),
                ));
            }
            Err(e) => return Err(failed(&mut child, format!("the VM worker: {e}"))),
        }
        let heard = Arc::new((Mutex::new(Heard::default()), Condvar::new()));
        let h = heard.clone();
        std::thread::spawn(move || {
            loop {
                let m = lines.read::<FromWorker>();
                let (lock, cv) = &*h;
                let mut heard = lock.lock().unwrap_or_else(|e| e.into_inner());
                match m {
                    Ok(Some(FromWorker::Started { vmm_pid })) => heard.started = Some(Ok(vmm_pid)),
                    Ok(Some(FromWorker::Failed { message })) => {
                        if heard.started.is_none() {
                            heard.started = Some(Err(message.clone()));
                        }
                        log::error!("the VM worker failed: {message}");
                    }
                    Ok(Some(FromWorker::Exited { code })) => heard.exited = Some(code),
                    Ok(Some(FromWorker::Ready {})) => {}
                    Ok(None) | Err(_) => {
                        heard.closed = true;
                        cv.notify_all();
                        return;
                    }
                }
                cv.notify_all();
            }
        });
        Ok(Arc::new(Worker {
            pid,
            pidfd: worker_pidfd,
            child: Mutex::new(child),
            tx: Mutex::new(ours),
            heard,
            vm_dir: plan.vm_dir,
            cgroup: plan.cgroup,
        }))
    }
}

impl Backend for LinuxBackend {
    fn create(&self, task: &CreateTask) -> Result<Arc<dyn Process>> {
        match &task.launch {
            Launch::Pause { netns } => self.pause(netns.as_deref()),
            Launch::Vm(v) => self.vm(task, v),
        }
    }
}
