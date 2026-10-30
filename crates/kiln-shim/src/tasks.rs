//! The shim's tasks (design §4, §7.5): their lifecycle, exit statuses and events, over
//! a [`Backend`] that runs their processes. One shim serves one pod: its sandbox task
//! (the pause process) and its containers, of which at most one may be created or
//! running at a time, so init containers run in sequence and each boots its own VM.
//! A running container also has execs (kiln-shim exec and volumes design §5.5): extra
//! processes in its VM, each with its own lifecycle and exit.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::SystemTime;

use crate::error::{Error, Result};

/// The status containerd sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Created,
    Running,
    Stopped,
}

/// How a task's process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    pub code: u32,
    pub at: SystemTime,
}

/// The exit status of a task whose worker went away without reporting the guest's exit
/// (kiln-shim spec §5).
pub const EXIT_LOST: u32 = 255;
pub const SIGKILL: i32 = 9;

/// The FIFOs containerd made for the task's stdio (empty when not used).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stdio {
    pub stdin: String,
    pub stdout: String,
    pub stderr: String,
    pub terminal: bool,
}

/// What a task runs, as the backend needs it.
#[derive(Debug, Clone)]
pub enum Launch {
    /// The pod's sandbox: a process that holds the pod netns (`None`: the caller's).
    Pause { netns: Option<PathBuf> },
    /// A container's VM.
    Vm(Box<crate::service::VmLaunch>),
}

/// A task to create.
#[derive(Debug, Clone)]
pub struct CreateTask {
    pub id: String,
    pub bundle: String,
    /// The pod (the CRI sandbox id), when there is one.
    pub group: Option<String>,
    pub stdio: Stdio,
    pub launch: Launch,
}

impl CreateTask {
    fn sandbox(&self) -> bool {
        matches!(self.launch, Launch::Pause { .. })
    }
}

/// The most execs a container has at once (created or running).
pub const MAX_EXECS: usize = kiln_proto::MAX_EXECS;

/// An exec to create in a container's VM.
#[derive(Debug, Clone)]
pub struct ExecLaunch {
    /// The host's id for it inside the VM (S5), chosen by [`Tasks`].
    pub id: u32,
    pub process: kiln_proto::Process,
    pub stdio: Stdio,
}

/// Runs tasks' processes.
pub trait Backend: Send + Sync + 'static {
    /// Creates the process (prepared, not started).
    fn create(&self, task: &CreateTask) -> Result<Arc<dyn Process>>;
}

/// One task's process.
pub trait Process: Send + Sync {
    fn pid(&self) -> u32;
    fn start(&self) -> Result<()>;
    fn signal(&self, sig: i32) -> Result<()>;
    fn resize(&self, rows: u16, cols: u16) -> Result<()>;
    fn close_stdin(&self) -> Result<()>;
    /// Blocks until the process ends; its exit code, or [`EXIT_LOST`].
    fn wait(&self) -> u32;
    /// Creates an exec in the process's VM (prepared, not started).
    fn exec(&self, _exec: &ExecLaunch) -> Result<Arc<dyn ExecProcess>> {
        Err(Error::Refused("this task runs no VM to exec in".into()))
    }
}

/// One exec's process.
pub trait ExecProcess: Send + Sync {
    fn start(&self) -> Result<()>;
    fn signal(&self, sig: i32) -> Result<()>;
    fn resize(&self, rows: u16, cols: u16) -> Result<()>;
    fn close_stdin(&self) -> Result<()>;
    /// Blocks until it ends (or, never started, is killed or its VM ends): its exit code.
    fn wait(&self) -> u32;
}

/// What containerd is told, in this order per task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Create {
        id: String,
        bundle: String,
        pid: u32,
        stdio: Stdio,
    },
    Start {
        id: String,
        pid: u32,
    },
    Exit {
        id: String,
        pid: u32,
        exit: Exit,
    },
    Delete {
        id: String,
        pid: u32,
        exit: Exit,
    },
    ExecAdded {
        id: String,
        exec_id: String,
    },
    ExecStarted {
        id: String,
        exec_id: String,
        pid: u32,
    },
    ExecExit {
        id: String,
        exec_id: String,
        pid: u32,
        exit: Exit,
    },
}

/// A task as `State` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskState {
    pub id: String,
    pub bundle: String,
    pub pid: u32,
    pub status: Status,
    pub stdio: Stdio,
    pub exit: Option<Exit>,
}

struct Entry {
    bundle: String,
    group: Option<String>,
    sandbox: bool,
    stdio: Stdio,
    process: Arc<dyn Process>,
    status: Status,
    /// A `Start` is in progress: the exit waits for it, so `Start` is published first.
    starting: bool,
    exit: Option<Exit>,
    execs: BTreeMap<String, ExecEntry>,
    /// The last exec id handed out in this container's VM.
    last_exec: u32,
}

struct ExecEntry {
    stdio: Stdio,
    process: Arc<dyn ExecProcess>,
    status: Status,
    starting: bool,
    exit: Option<Exit>,
}

type Publish = Box<dyn Fn(Event) + Send + Sync>;

struct Shared {
    tasks: Mutex<BTreeMap<String, Entry>>,
    changed: Condvar,
    publish: Publish,
}

/// The shim's tasks.
pub struct Tasks {
    backend: Arc<dyn Backend>,
    shared: Arc<Shared>,
}

fn not_found(id: &str) -> Error {
    Error::NotFound(format!("task {id} not found"))
}

fn exec_not_found(exec_id: &str) -> Error {
    Error::NotFound(format!("exec {exec_id} not found"))
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Entry>> {
        self.tasks.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Tasks {
    pub fn new(backend: Arc<dyn Backend>, publish: impl Fn(Event) + Send + Sync + 'static) -> Self {
        Self {
            backend,
            shared: Arc::new(Shared {
                tasks: Mutex::new(BTreeMap::new()),
                changed: Condvar::new(),
                publish: Box::new(publish),
            }),
        }
    }

    /// Creates the task's process; its pid.
    pub fn create(&self, task: CreateTask) -> Result<u32> {
        {
            let tasks = self.shared.lock();
            if tasks.contains_key(&task.id) {
                return Err(Error::Refused(format!("task {} already exists", task.id)));
            }
            if !task.sandbox()
                && tasks.values().any(|e| {
                    !e.sandbox && e.group == task.group && matches!(e.status, Status::Created | Status::Running)
                })
            {
                return Err(Error::Refused(
                    "kiln runs one container per pod at a time (kiln-shim spec D2: sidecars and multi-container \
                     pods are not supported)"
                        .into(),
                ));
            }
        }
        // The backend may take seconds (a VM worker's preparation): outside the lock.
        let process = self.backend.create(&task)?;
        let pid = process.pid();
        let mut tasks = self.shared.lock();
        if tasks.contains_key(&task.id) {
            let _ = process.signal(SIGKILL);
            return Err(Error::Refused(format!("task {} already exists", task.id)));
        }
        tasks.insert(
            task.id.clone(),
            Entry {
                bundle: task.bundle.clone(),
                group: task.group.clone(),
                sandbox: task.sandbox(),
                stdio: task.stdio.clone(),
                process: process.clone(),
                status: Status::Created,
                starting: false,
                exit: None,
                execs: BTreeMap::new(),
                last_exec: 0,
            },
        );
        (self.shared.publish)(Event::Create {
            id: task.id.clone(),
            bundle: task.bundle,
            pid,
            stdio: task.stdio,
        });
        drop(tasks);
        let shared = self.shared.clone();
        let id = task.id;
        std::thread::Builder::new()
            .name(format!("wait-{}", id.chars().take(8).collect::<String>()))
            .spawn(move || {
                let code = process.wait();
                let mut tasks = shared.lock();
                while tasks.get(&id).is_some_and(|e| e.starting) {
                    tasks = shared.changed.wait(tasks).unwrap_or_else(|e| e.into_inner());
                }
                if let Some(e) = tasks.get_mut(&id) {
                    let exit = Exit {
                        code,
                        at: SystemTime::now(),
                    };
                    e.status = Status::Stopped;
                    e.exit = Some(exit);
                    (shared.publish)(Event::Exit {
                        id: id.clone(),
                        pid,
                        exit,
                    });
                }
                shared.changed.notify_all();
            })?;
        Ok(pid)
    }

    fn process(&self, id: &str) -> Result<Arc<dyn Process>> {
        Ok(self.shared.lock().get(id).ok_or_else(|| not_found(id))?.process.clone())
    }

    /// Starts a created task; its pid.
    pub fn start(&self, id: &str) -> Result<u32> {
        let process = {
            let mut tasks = self.shared.lock();
            let e = tasks.get_mut(id).ok_or_else(|| not_found(id))?;
            if e.status != Status::Created || e.starting {
                return Err(Error::Refused(format!("task {id} is not in the created state")));
            }
            e.starting = true;
            e.process.clone()
        };
        let started = process.start();
        let mut tasks = self.shared.lock();
        let e = tasks.get_mut(id).ok_or_else(|| not_found(id))?;
        e.starting = false;
        let pid = process.pid();
        if started.is_ok() && e.status == Status::Created {
            e.status = Status::Running;
            (self.shared.publish)(Event::Start { id: id.into(), pid });
        }
        self.shared.changed.notify_all();
        started.map(|()| pid)
    }

    /// Signals the task's process; a stopped task needs nothing.
    pub fn kill(&self, id: &str, sig: i32) -> Result<()> {
        let process = {
            let tasks = self.shared.lock();
            let e = tasks.get(id).ok_or_else(|| not_found(id))?;
            if e.status == Status::Stopped {
                return Ok(());
            }
            e.process.clone()
        };
        process.signal(sig)
    }

    /// Blocks until the task has stopped.
    pub fn wait(&self, id: &str) -> Result<Exit> {
        let mut tasks = self.shared.lock();
        loop {
            match tasks.get(id) {
                None => return Err(not_found(id)),
                Some(Entry { exit: Some(exit), .. }) => return Ok(*exit),
                Some(_) => tasks = self.shared.changed.wait(tasks).unwrap_or_else(|e| e.into_inner()),
            }
        }
    }

    pub fn state(&self, id: &str) -> Result<TaskState> {
        let tasks = self.shared.lock();
        let e = tasks.get(id).ok_or_else(|| not_found(id))?;
        Ok(TaskState {
            id: id.into(),
            bundle: e.bundle.clone(),
            pid: e.process.pid(),
            status: e.status,
            stdio: e.stdio.clone(),
            exit: e.exit,
        })
    }

    /// Forgets a stopped task; a created one is killed first. Its pid and exit.
    pub fn delete(&self, id: &str) -> Result<(u32, Exit)> {
        let status = self.state(id)?.status;
        match status {
            Status::Running => return Err(Error::Refused(format!("task {id} is running; kill it first"))),
            Status::Created => {
                self.kill(id, SIGKILL)?;
                self.wait(id)?;
            }
            Status::Stopped => {}
        }
        let mut tasks = self.shared.lock();
        let e = tasks.remove(id).ok_or_else(|| not_found(id))?;
        let (pid, exit) = (e.process.pid(), e.exit.expect("a stopped task has its exit"));
        (self.shared.publish)(Event::Delete {
            id: id.into(),
            pid,
            exit,
        });
        self.shared.changed.notify_all();
        Ok((pid, exit))
    }

    pub fn pids(&self, id: &str) -> Result<Vec<u32>> {
        Ok(vec![self.state(id)?.pid])
    }

    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> Result<()> {
        self.process(id)?.resize(rows, cols)
    }

    pub fn close_stdin(&self, id: &str) -> Result<()> {
        self.process(id)?.close_stdin()
    }

    /// Creates exec `exec_id` in running task `id` from `process` and `stdio`.
    pub fn exec(&self, id: &str, exec_id: &str, process: kiln_proto::Process, stdio: Stdio) -> Result<()> {
        let (task, launch) = {
            let mut tasks = self.shared.lock();
            let e = tasks.get_mut(id).ok_or_else(|| not_found(id))?;
            if e.status != Status::Running {
                return Err(Error::Refused(format!("task {id} is not running")));
            }
            if e.execs.contains_key(exec_id) {
                return Err(Error::Refused(format!("exec {exec_id} already exists")));
            }
            if e.execs.values().filter(|x| x.status != Status::Stopped).count() >= MAX_EXECS {
                return Err(Error::Refused(format!(
                    "kiln runs at most {MAX_EXECS} execs per container at once"
                )));
            }
            e.last_exec += 1;
            let launch = ExecLaunch {
                id: e.last_exec,
                process,
                stdio: stdio.clone(),
            };
            (e.process.clone(), launch)
        };
        // Opening its FIFOs may block until containerd's side is there: outside the lock.
        let process = task.exec(&launch)?;
        let mut tasks = self.shared.lock();
        let Some(e) = tasks.get_mut(id) else {
            let _ = process.signal(SIGKILL);
            return Err(not_found(id));
        };
        if e.execs.contains_key(exec_id) {
            let _ = process.signal(SIGKILL);
            return Err(Error::Refused(format!("exec {exec_id} already exists")));
        }
        e.execs.insert(
            exec_id.into(),
            ExecEntry {
                stdio,
                process: process.clone(),
                status: Status::Created,
                starting: false,
                exit: None,
            },
        );
        let pid = e.process.pid();
        (self.shared.publish)(Event::ExecAdded {
            id: id.into(),
            exec_id: exec_id.into(),
        });
        drop(tasks);
        let shared = self.shared.clone();
        let (id, exec_id) = (id.to_string(), exec_id.to_string());
        std::thread::Builder::new()
            .name(format!("exec-{}", exec_id.chars().take(8).collect::<String>()))
            .spawn(move || {
                let code = process.wait();
                let mut tasks = shared.lock();
                while tasks
                    .get(&id)
                    .and_then(|e| e.execs.get(&exec_id))
                    .is_some_and(|x| x.starting)
                {
                    tasks = shared.changed.wait(tasks).unwrap_or_else(|e| e.into_inner());
                }
                if let Some(x) = tasks.get_mut(&id).and_then(|e| e.execs.get_mut(&exec_id)) {
                    let exit = Exit {
                        code,
                        at: SystemTime::now(),
                    };
                    // One that never ran ends without an exit event, as runc's.
                    if x.status == Status::Running {
                        (shared.publish)(Event::ExecExit {
                            id: id.clone(),
                            exec_id: exec_id.clone(),
                            pid,
                            exit,
                        });
                    }
                    x.status = Status::Stopped;
                    x.exit = Some(exit);
                }
                shared.changed.notify_all();
            })?;
        Ok(())
    }

    fn exec_process(&self, id: &str, exec_id: &str) -> Result<Arc<dyn ExecProcess>> {
        let tasks = self.shared.lock();
        let e = tasks.get(id).ok_or_else(|| not_found(id))?;
        Ok(e.execs
            .get(exec_id)
            .ok_or_else(|| exec_not_found(exec_id))?
            .process
            .clone())
    }

    /// Starts a created exec; the pid it reports (its VM's).
    pub fn start_exec(&self, id: &str, exec_id: &str) -> Result<u32> {
        let (process, pid) = {
            let mut tasks = self.shared.lock();
            let e = tasks.get_mut(id).ok_or_else(|| not_found(id))?;
            let pid = e.process.pid();
            let x = e.execs.get_mut(exec_id).ok_or_else(|| exec_not_found(exec_id))?;
            if x.status != Status::Created || x.starting {
                return Err(Error::Refused(format!("exec {exec_id} is not in the created state")));
            }
            x.starting = true;
            (x.process.clone(), pid)
        };
        let started = process.start();
        let mut tasks = self.shared.lock();
        if let Some(x) = tasks.get_mut(id).and_then(|e| e.execs.get_mut(exec_id)) {
            x.starting = false;
            if started.is_ok() && x.status == Status::Created {
                x.status = Status::Running;
                (self.shared.publish)(Event::ExecStarted {
                    id: id.into(),
                    exec_id: exec_id.into(),
                    pid,
                });
            }
        }
        self.shared.changed.notify_all();
        started.map(|()| pid)
    }

    /// Signals an exec; a stopped one needs nothing.
    pub fn kill_exec(&self, id: &str, exec_id: &str, sig: i32) -> Result<()> {
        if self.exec_state(id, exec_id)?.status == Status::Stopped {
            return Ok(());
        }
        self.exec_process(id, exec_id)?.signal(sig)
    }

    /// Blocks until the exec has stopped.
    pub fn wait_exec(&self, id: &str, exec_id: &str) -> Result<Exit> {
        let mut tasks = self.shared.lock();
        loop {
            let e = tasks.get(id).ok_or_else(|| not_found(id))?;
            match e.execs.get(exec_id) {
                None => return Err(exec_not_found(exec_id)),
                Some(ExecEntry { exit: Some(exit), .. }) => return Ok(*exit),
                Some(_) => tasks = self.shared.changed.wait(tasks).unwrap_or_else(|e| e.into_inner()),
            }
        }
    }

    pub fn exec_state(&self, id: &str, exec_id: &str) -> Result<TaskState> {
        let tasks = self.shared.lock();
        let e = tasks.get(id).ok_or_else(|| not_found(id))?;
        let x = e.execs.get(exec_id).ok_or_else(|| exec_not_found(exec_id))?;
        Ok(TaskState {
            id: exec_id.into(),
            bundle: e.bundle.clone(),
            pid: e.process.pid(),
            status: x.status,
            stdio: x.stdio.clone(),
            exit: x.exit,
        })
    }

    /// Forgets a stopped exec (a created one is killed first); its pid and exit. No event,
    /// as runc's shim publishes none for an exec.
    pub fn delete_exec(&self, id: &str, exec_id: &str) -> Result<(u32, Exit)> {
        match self.exec_state(id, exec_id)?.status {
            Status::Running => {
                return Err(Error::Refused(format!("exec {exec_id} is running; kill it first")));
            }
            Status::Created => {
                self.kill_exec(id, exec_id, SIGKILL)?;
                self.wait_exec(id, exec_id)?;
            }
            Status::Stopped => {}
        }
        let mut tasks = self.shared.lock();
        let e = tasks.get_mut(id).ok_or_else(|| not_found(id))?;
        let pid = e.process.pid();
        let x = e.execs.remove(exec_id).ok_or_else(|| exec_not_found(exec_id))?;
        self.shared.changed.notify_all();
        Ok((pid, x.exit.expect("a stopped exec has its exit")))
    }

    pub fn resize_exec(&self, id: &str, exec_id: &str, rows: u16, cols: u16) -> Result<()> {
        self.exec_process(id, exec_id)?.resize(rows, cols)
    }

    pub fn close_stdin_exec(&self, id: &str, exec_id: &str) -> Result<()> {
        self.exec_process(id, exec_id)?.close_stdin()
    }

    /// No task is left: the shim may exit.
    pub fn is_empty(&self) -> bool {
        self.shared.lock().is_empty()
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::sync::mpsc::{Receiver, Sender, channel};

    use super::*;

    /// A process that ends when told (`end`), or on SIGKILL with 137; other signals are
    /// recorded.
    pub struct FakeProcess {
        pub pid: u32,
        tx: Mutex<Sender<u32>>,
        rx: Mutex<Receiver<u32>>,
        pub signals: Mutex<Vec<i32>>,
        pub fail_start: bool,
        pub execs: Mutex<BTreeMap<u32, Arc<FakeExec>>>,
    }

    /// An exec that ends when told, or on SIGKILL (137, or 137 unstarted too).
    pub struct FakeExec {
        tx: Mutex<Sender<u32>>,
        rx: Mutex<Receiver<u32>>,
        pub signals: Mutex<Vec<i32>>,
        pub fail_start: bool,
        pub launch: ExecLaunch,
    }

    impl FakeExec {
        pub fn end(&self, code: u32) {
            let _ = self.tx.lock().unwrap().send(code);
        }
    }

    impl ExecProcess for FakeExec {
        fn start(&self) -> Result<()> {
            if self.fail_start {
                self.end(EXIT_LOST);
                return Err(Error::Internal("exec nope: not found".into()));
            }
            Ok(())
        }
        fn signal(&self, sig: i32) -> Result<()> {
            self.signals.lock().unwrap().push(sig);
            if sig == SIGKILL {
                self.end(137);
            }
            Ok(())
        }
        fn resize(&self, _rows: u16, _cols: u16) -> Result<()> {
            Ok(())
        }
        fn close_stdin(&self) -> Result<()> {
            Ok(())
        }
        fn wait(&self) -> u32 {
            self.rx.lock().unwrap().recv().unwrap_or(EXIT_LOST)
        }
    }

    impl FakeProcess {
        pub fn exec_with(&self, id: u32) -> Arc<FakeExec> {
            self.execs.lock().unwrap()[&id].clone()
        }

        pub fn end(&self, code: u32) {
            let _ = self.tx.lock().unwrap().send(code);
        }

        /// Ends the wait without a code, as a worker that went away: replacing the only
        /// sender drops it, and the receiver sees the channel close.
        pub fn vanish(&self) {
            *self.tx.lock().unwrap() = channel().0;
        }
    }

    impl Process for FakeProcess {
        fn pid(&self) -> u32 {
            self.pid
        }
        fn start(&self) -> Result<()> {
            if self.fail_start {
                return Err(Error::Internal("boot failed".into()));
            }
            Ok(())
        }
        fn signal(&self, sig: i32) -> Result<()> {
            self.signals.lock().unwrap().push(sig);
            if sig == SIGKILL {
                self.end(137);
            }
            Ok(())
        }
        fn resize(&self, _rows: u16, _cols: u16) -> Result<()> {
            Ok(())
        }
        fn close_stdin(&self) -> Result<()> {
            Ok(())
        }
        fn wait(&self) -> u32 {
            self.rx.lock().unwrap().recv().unwrap_or(EXIT_LOST)
        }
        fn exec(&self, exec: &ExecLaunch) -> Result<Arc<dyn ExecProcess>> {
            let (tx, rx) = channel();
            let x = Arc::new(FakeExec {
                tx: Mutex::new(tx),
                rx: Mutex::new(rx),
                signals: Mutex::new(Vec::new()),
                fail_start: exec.process.argv().first() == Some(&"nope"),
                launch: exec.clone(),
            });
            self.execs.lock().unwrap().insert(exec.id, x.clone());
            Ok(x)
        }
    }

    #[derive(Default)]
    pub struct FakeBackend {
        pub processes: Mutex<BTreeMap<String, Arc<FakeProcess>>>,
        pub fail_start: Mutex<bool>,
    }

    impl FakeBackend {
        pub fn get(&self, id: &str) -> Arc<FakeProcess> {
            self.processes.lock().unwrap()[id].clone()
        }
    }

    impl Backend for FakeBackend {
        fn create(&self, task: &CreateTask) -> Result<Arc<dyn Process>> {
            let (tx, rx) = channel();
            let mut ps = self.processes.lock().unwrap();
            let p = Arc::new(FakeProcess {
                pid: 1000 + ps.len() as u32,
                tx: Mutex::new(tx),
                rx: Mutex::new(rx),
                signals: Mutex::new(Vec::new()),
                fail_start: *self.fail_start.lock().unwrap(),
                execs: Mutex::new(BTreeMap::new()),
            });
            ps.insert(task.id.clone(), p.clone());
            Ok(p)
        }
    }

    pub fn container(id: &str, group: Option<&str>) -> CreateTask {
        CreateTask {
            id: id.into(),
            bundle: format!("/b/{id}"),
            group: group.map(String::from),
            stdio: Stdio::default(),
            launch: Launch::Pause { netns: None },
        }
        .into_container()
    }

    pub fn sandbox(id: &str, group: &str) -> CreateTask {
        CreateTask {
            id: id.into(),
            bundle: format!("/b/{id}"),
            group: Some(group.into()),
            stdio: Stdio::default(),
            launch: Launch::Pause { netns: None },
        }
    }

    impl CreateTask {
        fn into_container(mut self) -> Self {
            self.launch = Launch::Vm(Box::default());
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::fake::*;
    use super::*;

    fn tasks() -> (Tasks, Arc<FakeBackend>, Arc<Mutex<Vec<Event>>>) {
        let backend = Arc::new(FakeBackend::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let log = events.clone();
        let t = Tasks::new(backend.clone(), move |e| log.lock().unwrap().push(e));
        (t, backend, events)
    }

    fn kinds(events: &Mutex<Vec<Event>>) -> Vec<&'static str> {
        events
            .lock()
            .unwrap()
            .iter()
            .map(|e| match e {
                Event::Create { .. } => "create",
                Event::Start { .. } => "start",
                Event::Exit { .. } => "exit",
                Event::Delete { .. } => "delete",
                Event::ExecAdded { .. } => "exec-added",
                Event::ExecStarted { .. } => "exec-started",
                Event::ExecExit { .. } => "exec-exit",
            })
            .collect()
    }

    fn argv(a: &[&str]) -> kiln_proto::Process {
        kiln_proto::Process {
            entrypoint: a.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn exec_lifecycle_and_events() {
        let (t, backend, events) = tasks();
        let pid = t.create(container("c", None)).unwrap();
        assert!(
            matches!(
                t.exec("c", "e1", argv(&["sh"]), Stdio::default()),
                Err(Error::Refused(_))
            ),
            "not running yet"
        );
        t.start("c").unwrap();
        t.exec("c", "e1", argv(&["sh"]), Stdio::default()).unwrap();
        t.exec("c", "e2", argv(&["true"]), Stdio::default()).unwrap();
        assert!(
            t.exec("c", "e1", argv(&["sh"]), Stdio::default()).is_err(),
            "same exec id"
        );
        let p = backend.get("c");
        assert_eq!((p.exec_with(1).launch.id, p.exec_with(2).launch.id), (1, 2), "host ids");
        assert_eq!(t.exec_state("c", "e1").unwrap().status, Status::Created);
        assert_eq!(t.start_exec("c", "e1").unwrap(), pid);
        assert_eq!(t.exec_state("c", "e1").unwrap().status, Status::Running);
        assert!(t.start_exec("c", "e1").is_err());
        t.kill_exec("c", "e1", 15).unwrap();
        assert_eq!(p.exec_with(1).signals.lock().unwrap().as_slice(), [15]);
        assert!(matches!(t.delete_exec("c", "e1"), Err(Error::Refused(_))), "running");
        p.exec_with(1).end(7);
        assert_eq!(t.wait_exec("c", "e1").unwrap().code, 7);
        let (p_, exit) = t.delete_exec("c", "e1").unwrap();
        assert_eq!((p_, exit.code), (pid, 7));
        assert!(matches!(t.exec_state("c", "e1"), Err(Error::NotFound(_))));
        // A created exec deleted is killed, with no exit event.
        assert_eq!(t.delete_exec("c", "e2").unwrap().1.code, 137);
        assert_eq!(
            kinds(&events),
            [
                "create",
                "start",
                "exec-added",
                "exec-added",
                "exec-started",
                "exec-exit"
            ]
        );
        assert!(t.state("c").is_ok());
    }

    #[test]
    fn a_failed_exec_start_and_the_limit() {
        let (t, _backend, events) = tasks();
        t.create(container("c", None)).unwrap();
        t.start("c").unwrap();
        t.exec("c", "bad", argv(&["nope"]), Stdio::default()).unwrap();
        assert!(t.start_exec("c", "bad").is_err());
        assert_eq!(t.wait_exec("c", "bad").unwrap().code, EXIT_LOST);
        assert!(!kinds(&events).contains(&"exec-exit"), "it never ran");
        for n in 0..MAX_EXECS {
            t.exec("c", &format!("e{n}"), argv(&["sh"]), Stdio::default()).unwrap();
        }
        let err = t.exec("c", "over", argv(&["sh"]), Stdio::default()).unwrap_err();
        assert!(err.to_string().contains("at most"), "{err}");
        assert!(matches!(t.start_exec("c", "nope"), Err(Error::NotFound(_))));
    }

    #[test]
    fn lifecycle_and_event_order() {
        let (t, backend, events) = tasks();
        let pid = t.create(container("c1", Some("p"))).unwrap();
        assert_eq!(t.state("c1").unwrap().status, Status::Created);
        assert_eq!(t.start("c1").unwrap(), pid);
        assert_eq!(t.state("c1").unwrap().status, Status::Running);
        assert!(t.start("c1").is_err(), "started twice");
        backend.get("c1").end(3);
        assert_eq!(t.wait("c1").unwrap().code, 3);
        let s = t.state("c1").unwrap();
        assert_eq!((s.status, s.exit.unwrap().code, s.pid), (Status::Stopped, 3, pid));
        assert_eq!(t.pids("c1").unwrap(), [pid]);
        assert_eq!(t.delete("c1").unwrap().1.code, 3);
        assert_eq!(kinds(&events), ["create", "start", "exit", "delete"]);
        assert!(t.is_empty());
    }

    #[test]
    fn one_container_at_a_time() {
        let (t, backend, _) = tasks();
        t.create(sandbox("s", "p")).unwrap();
        t.create(container("init", Some("p"))).unwrap();
        let err = t.create(container("app", Some("p"))).unwrap_err();
        assert!(
            matches!(&err, Error::Refused(m) if m.contains("one container per pod at a time")),
            "{err}"
        );
        // Another pod's container is not affected.
        t.create(container("other", Some("q"))).unwrap();
        t.start("init").unwrap();
        backend.get("init").end(0);
        t.wait("init").unwrap();
        t.create(container("app", Some("p"))).unwrap();
        assert!(t.create(container("app", Some("p"))).is_err(), "same id twice");
    }

    #[test]
    fn kill_before_start() {
        let (t, backend, events) = tasks();
        t.create(container("c", None)).unwrap();
        t.kill("c", SIGKILL).unwrap();
        assert_eq!(t.wait("c").unwrap().code, 137);
        assert_eq!(t.state("c").unwrap().status, Status::Stopped);
        assert_eq!(backend.get("c").signals.lock().unwrap().as_slice(), [SIGKILL]);
        t.kill("c", 15).unwrap();
        assert_eq!(
            backend.get("c").signals.lock().unwrap().len(),
            1,
            "a stopped task is not signalled"
        );
        assert_eq!(kinds(&events), ["create", "exit"]);
    }

    #[test]
    fn delete_running_is_refused() {
        let (t, backend, _) = tasks();
        t.create(container("c", None)).unwrap();
        t.start("c").unwrap();
        assert!(matches!(t.delete("c"), Err(Error::Refused(_))));
        assert!(!t.is_empty());
        t.kill("c", 15).unwrap();
        assert_eq!(backend.get("c").signals.lock().unwrap().as_slice(), [15]);
        backend.get("c").end(143);
        t.wait("c").unwrap();
        t.delete("c").unwrap();
        assert!(t.is_empty());
    }

    #[test]
    fn deleting_a_created_task_kills_it() {
        let (t, _, events) = tasks();
        t.create(container("c", None)).unwrap();
        assert_eq!(t.delete("c").unwrap().1.code, 137);
        assert_eq!(kinds(&events), ["create", "exit", "delete"]);
    }

    #[test]
    fn worker_gone_is_255() {
        let (t, backend, _) = tasks();
        t.create(container("c", None)).unwrap();
        t.start("c").unwrap();
        backend.get("c").vanish();
        assert_eq!(t.wait("c").unwrap().code, EXIT_LOST);
    }

    #[test]
    fn a_failed_start_leaves_the_task_created() {
        let (t, backend, events) = tasks();
        *backend.fail_start.lock().unwrap() = true;
        t.create(container("c", None)).unwrap();
        assert!(t.start("c").is_err());
        assert_eq!(t.state("c").unwrap().status, Status::Created);
        assert_eq!(kinds(&events), ["create"]);
    }

    #[test]
    fn unknown_id_is_not_found() {
        let (t, _, _) = tasks();
        for r in [
            t.start("x").map(|_| ()),
            t.kill("x", 9),
            t.wait("x").map(|_| ()),
            t.delete("x").map(|_| ()),
        ] {
            assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
        }
    }

    #[test]
    fn an_exit_during_start_is_published_after_start() {
        let (t, backend, events) = tasks();
        t.create(container("c", None)).unwrap();
        // The process ends while Start runs; Exit still follows Start.
        backend.get("c").end(0);
        std::thread::sleep(Duration::from_millis(20));
        let _ = t.start("c");
        t.wait("c").unwrap();
        let k = kinds(&events);
        assert_eq!(k.first(), Some(&"create"));
        assert_eq!(k.last(), Some(&"exit"));
    }
}
