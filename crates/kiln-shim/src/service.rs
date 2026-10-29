//! The shim v2 Task service (design §7.5) over [`Tasks`], and the [`Shim`] that
//! containerd starts: one per pod, grouped by the CRI sandbox id, as runc's shim does.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use containerd_shim::protos::events::task::{
    TaskCreate, TaskDelete, TaskExecAdded, TaskExecStarted, TaskExit, TaskIO, TaskStart,
};
use containerd_shim::protos::protobuf::well_known_types::timestamp::Timestamp;
use containerd_shim::protos::protobuf::{MessageDyn, MessageField};
use containerd_shim::protos::topics::{
    TASK_CREATE_EVENT_TOPIC, TASK_DELETE_EVENT_TOPIC, TASK_EXEC_ADDED_EVENT_TOPIC, TASK_EXEC_STARTED_EVENT_TOPIC,
    TASK_EXIT_EVENT_TOPIC, TASK_START_EVENT_TOPIC,
};
use containerd_shim::protos::ttrpc::{self, Code};
use containerd_shim::protos::types::introspection::{RuntimeInfo, RuntimeVersion};
use containerd_shim::publisher::RemotePublisher;
use containerd_shim::{Config, DeleteResponse, ExitSignal, Flags, StartOpts, TtrpcContext, TtrpcResult, api};

use crate::error::{Error, Result};
use crate::netconf::Resolv;
use crate::oci::{Spec, TaskKind};
use crate::rootfs::RootfsMount;
use crate::settings::{RuntimeOptions, Settings};
use containerd_shim::protos::types::task::ProcessInfo;

use crate::tasks::{Backend, CreateTask, Event, Launch, Status, Stdio, Tasks};
use crate::translate::Resources;
use crate::{RUNTIME_ID, cgroup, mounts, netconf, oci, rootfs, translate};

/// The RuntimeInfo annotation that has containerd hand erofs snapshots over unmounted
/// (containerd 2.2-2.4; design §5.1).
pub const ALLOW_MOUNTS: &str = "containerd.io/runtime-allow-mounts";
pub const ALLOWED_MOUNTS: &str = "mkdir/*,format/*,erofs";

/// Everything a container's VM worker needs, decided by the shim from the request.
#[derive(Debug, Clone, Default)]
pub struct VmLaunch {
    pub namespace: String,
    pub task_id: String,
    pub settings: Settings,
    /// The erofs layer files, lowest first.
    pub layers: Vec<PathBuf>,
    pub process: kiln_proto::Process,
    pub hostname: String,
    pub resources: Resources,
    pub tty: bool,
    pub interactive: bool,
    /// The pod netns the VM joins (and is attached to, design §6.1).
    pub netns: Option<PathBuf>,
    /// The container's cgroup v2 directory.
    pub cgroup: Option<PathBuf>,
    pub resolv: Resolv,
    /// Lines for the container's log before it starts (volumes not delivered).
    pub warnings: Vec<String>,
}

impl Default for Resources {
    fn default() -> Self {
        Resources {
            cpus: 1,
            memory_mib: 1024,
        }
    }
}

/// The `-info` answer.
pub fn runtime_info() -> RuntimeInfo {
    let mut info = RuntimeInfo::new();
    info.name = RUNTIME_ID.into();
    let mut v = RuntimeVersion::new();
    v.version = env!("CARGO_PKG_VERSION").into();
    info.version = MessageField::some(v);
    info.annotations.insert(ALLOW_MOUNTS.into(), ALLOWED_MOUNTS.into());
    info
}

/// The name of a task's directories (the worker's and its scratch disk's): its
/// namespace and id, shortened. The VMM's sockets live inside, and a Unix socket path is
/// at most 108 bytes, so a CRI id (64 hex digits) is cut to its first 24 characters,
/// which still never collide in practice.
pub fn task_dir_name(namespace: &str, id: &str) -> String {
    let clean = |s: &str, n: usize| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .take(n)
            .collect()
    };
    format!("{}-{}", clean(namespace, 16), clean(id, 24))
}

/// The shim process a task belongs to: its pod, or (no CRI) itself.
pub fn group_id(spec: &Spec, id: &str) -> String {
    spec.group().unwrap_or(id).to_string()
}

fn status(e: Error) -> ttrpc::Error {
    let code = match &e {
        Error::Config(_) | Error::Invalid(_) => Code::INVALID_ARGUMENT,
        Error::Refused(_) => Code::FAILED_PRECONDITION,
        Error::NotFound(_) => Code::NOT_FOUND,
        Error::Unsupported(_) => Code::UNIMPLEMENTED,
        Error::Io(_) | Error::Internal(_) => Code::INTERNAL,
    };
    ttrpc::Error::RpcStatus(ttrpc::get_status(code, e.to_string()))
}

fn timestamp(t: SystemTime) -> Timestamp {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let mut ts = Timestamp::new();
    ts.seconds = d.as_secs() as i64;
    ts.nanos = d.subsec_nanos() as i32;
    ts
}

/// A request's exec id: `None` names the task itself.
fn exec_id(exec_id: &str) -> Option<&str> {
    (!exec_id.is_empty()).then_some(exec_id)
}

/// The runtime options CRI sends (`runtimeoptions.v1.Options`), when the request has them.
fn runtime_options(req: &api::CreateTaskRequest) -> Result<Option<RuntimeOptions>> {
    match req.options.as_ref() {
        Some(any) if any.type_url.ends_with(RuntimeOptions::TYPE_URL) => RuntimeOptions::decode(&any.value).map(Some),
        _ => Ok(None),
    }
}

/// The largest resolv.conf read.
const MAX_RESOLV_CONF: u64 = 64 << 10;

/// The pod's resolv.conf, read as root, so only the one CRI wrote for the task's own
/// sandbox (`…/sandboxes/<sandbox id>/resolv.conf`): a regular file, not through a
/// symlink, and bounded. Anything else at `/etc/resolv.conf` (a pod's hostPath volume
/// there) is not read: it would hand the guest lines of an arbitrary host file, or hang
/// on a FIFO.
pub fn read_resolv_conf(path: &Path, sandbox_id: Option<&str>) -> Option<String> {
    use rustix::fs::{Mode, OFlags};
    use std::io::Read;
    let sandbox_id = sandbox_id?;
    let mut parts = path.components().rev();
    let (file, id, dir) = (parts.next()?, parts.next()?, parts.next()?);
    if file.as_os_str() != "resolv.conf" || id.as_os_str() != sandbox_id || dir.as_os_str() != "sandboxes" {
        return None;
    }
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let f = std::fs::File::from(rustix::fs::open(path, flags, Mode::empty()).ok()?);
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    f.take(MAX_RESOLV_CONF).read_to_string(&mut text).ok()?;
    Some(text)
}

/// What a create request runs (design §7.1, §7.2, §5.2). `pod_hostname` is the
/// hostname of the task's pod sandbox; `load` finds the settings.
pub fn launch(
    req: &api::CreateTaskRequest,
    spec: &Spec,
    namespace: &str,
    pod_hostname: Option<&str>,
    load: &dyn Fn(Option<&RuntimeOptions>) -> Result<Settings>,
) -> Result<Launch> {
    if spec.kind() == TaskKind::Sandbox {
        if spec.host_network() {
            return Err(Error::invalid(
                "kiln pods cannot use the host's network (hostNetwork: true): a VM has its own",
            ));
        }
        return Ok(Launch::Pause {
            netns: spec.netns().map(Path::to_path_buf),
        });
    }
    let settings = load(runtime_options(req)?.as_ref())?;
    let checked = mounts::check(spec)?;
    let snapshot: Vec<RootfsMount> = req
        .rootfs
        .iter()
        .map(|m| RootfsMount {
            kind: m.type_.clone(),
            source: m.source.clone(),
            options: m.options.clone(),
        })
        .collect();
    let layers = rootfs::layers(&snapshot)?;
    let resolv = checked
        .resolv_conf
        .as_deref()
        .and_then(|p| read_resolv_conf(p, spec.group()))
        .map(|t| netconf::resolv(&t))
        .unwrap_or_default();
    Ok(Launch::Vm(Box::new(VmLaunch {
        namespace: namespace.into(),
        task_id: req.id.clone(),
        layers,
        process: translate::guest_process(spec)?,
        hostname: translate::hostname(spec, pod_hostname, &req.id),
        resources: translate::resources(spec, &settings),
        tty: req.terminal,
        interactive: !req.stdin.is_empty(),
        netns: spec.netns().map(Path::to_path_buf),
        cgroup: spec
            .cgroups_path()
            .and_then(|p| cgroup::resolve(p, Path::new(cgroup::ROOT))),
        resolv,
        warnings: mounts::warning(&checked.not_delivered).into_iter().collect(),
        settings,
    })))
}

/// How the service finds a task's settings.
type LoadSettings = Arc<dyn Fn(Option<&RuntimeOptions>) -> Result<Settings> + Send + Sync>;

/// The Task service.
#[derive(Clone)]
pub struct Service {
    tasks: Arc<Tasks>,
    namespace: String,
    exit: Arc<ExitSignal>,
    load: LoadSettings,
    /// Each pod's hostname, from its sandbox's spec, for its containers.
    hostnames: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
}

impl Service {
    pub fn new(
        backend: Arc<dyn Backend>,
        namespace: &str,
        exit: Arc<ExitSignal>,
        publish: impl Fn(Event) + Send + Sync + 'static,
    ) -> Self {
        Self {
            tasks: Arc::new(Tasks::new(backend, publish)),
            namespace: namespace.into(),
            exit,
            load: Arc::new(Settings::load),
            hostnames: Arc::default(),
        }
    }

    /// The same service with another way to find the settings (tests).
    pub fn with_settings(
        mut self,
        load: impl Fn(Option<&RuntimeOptions>) -> Result<Settings> + Send + Sync + 'static,
    ) -> Self {
        self.load = Arc::new(load);
        self
    }
}

impl containerd_shim::Task for Service {
    fn create(&self, _ctx: &TtrpcContext, req: api::CreateTaskRequest) -> TtrpcResult<api::CreateTaskResponse> {
        let spec = oci::read_spec(Path::new(&req.bundle)).map_err(status)?;
        let pod_hostname = {
            let mut names = self.hostnames.lock().unwrap_or_else(|e| e.into_inner());
            match (spec.kind(), spec.group(), spec.hostname.as_deref()) {
                (TaskKind::Sandbox, Some(group), Some(h)) => {
                    names.insert(group.into(), h.into());
                    None
                }
                (_, Some(group), _) => names.get(group).cloned(),
                _ => None,
            }
        };
        let launch = launch(&req, &spec, &self.namespace, pod_hostname.as_deref(), &*self.load).map_err(status)?;
        let pid = self
            .tasks
            .create(CreateTask {
                id: req.id.clone(),
                bundle: req.bundle.clone(),
                group: spec.group().map(String::from),
                stdio: Stdio {
                    stdin: req.stdin.clone(),
                    stdout: req.stdout.clone(),
                    stderr: req.stderr.clone(),
                    terminal: req.terminal,
                },
                launch,
            })
            .map_err(status)?;
        let mut resp = api::CreateTaskResponse::new();
        resp.pid = pid;
        Ok(resp)
    }

    fn start(&self, _ctx: &TtrpcContext, req: api::StartRequest) -> TtrpcResult<api::StartResponse> {
        let mut resp = api::StartResponse::new();
        resp.pid = match exec_id(&req.exec_id) {
            None => self.tasks.start(&req.id),
            Some(x) => self.tasks.start_exec(&req.id, x),
        }
        .map_err(status)?;
        Ok(resp)
    }

    fn kill(&self, _ctx: &TtrpcContext, req: api::KillRequest) -> TtrpcResult<api::Empty> {
        let exec = exec_id(&req.exec_id);
        match req.signal {
            // As kill(2): signal 0 only checks that the task is there.
            0 => {
                match exec {
                    None => self.tasks.state(&req.id),
                    Some(x) => self.tasks.exec_state(&req.id, x),
                }
                .map_err(status)?;
            }
            // kiln-proto forwards 1-31; anything else would be a protocol violation, which
            // kills the VM.
            1..=31 => match exec {
                None => self.tasks.kill(&req.id, req.signal as i32),
                Some(x) => self.tasks.kill_exec(&req.id, x, req.signal as i32),
            }
            .map_err(status)?,
            sig => return Err(status(Error::invalid(format!("signal {sig} (kiln forwards 1-31)")))),
        }
        Ok(api::Empty::new())
    }

    fn wait(&self, _ctx: &TtrpcContext, req: api::WaitRequest) -> TtrpcResult<api::WaitResponse> {
        let exit = match exec_id(&req.exec_id) {
            None => self.tasks.wait(&req.id),
            Some(x) => self.tasks.wait_exec(&req.id, x),
        }
        .map_err(status)?;
        let mut resp = api::WaitResponse::new();
        resp.exit_status = exit.code;
        resp.exited_at = MessageField::some(timestamp(exit.at));
        Ok(resp)
    }

    fn state(&self, _ctx: &TtrpcContext, req: api::StateRequest) -> TtrpcResult<api::StateResponse> {
        let s = match exec_id(&req.exec_id) {
            None => self.tasks.state(&req.id),
            Some(x) => self.tasks.exec_state(&req.id, x),
        }
        .map_err(status)?;
        let mut resp = api::StateResponse::new();
        resp.id = s.id;
        resp.bundle = s.bundle;
        resp.pid = s.pid;
        resp.status = match s.status {
            Status::Created => api::Status::CREATED,
            Status::Running => api::Status::RUNNING,
            Status::Stopped => api::Status::STOPPED,
        }
        .into();
        resp.stdin = s.stdio.stdin;
        resp.stdout = s.stdio.stdout;
        resp.stderr = s.stdio.stderr;
        resp.terminal = s.stdio.terminal;
        if let Some(e) = s.exit {
            resp.exit_status = e.code;
            resp.exited_at = MessageField::some(timestamp(e.at));
        }
        Ok(resp)
    }

    fn delete(&self, _ctx: &TtrpcContext, req: api::DeleteRequest) -> TtrpcResult<api::DeleteResponse> {
        let (pid, exit) = match exec_id(&req.exec_id) {
            None => self.tasks.delete(&req.id),
            Some(x) => self.tasks.delete_exec(&req.id, x),
        }
        .map_err(status)?;
        let mut resp = api::DeleteResponse::new();
        resp.pid = pid;
        resp.exit_status = exit.code;
        resp.exited_at = MessageField::some(timestamp(exit.at));
        Ok(resp)
    }

    fn pids(&self, _ctx: &TtrpcContext, req: api::PidsRequest) -> TtrpcResult<api::PidsResponse> {
        let mut resp = api::PidsResponse::new();
        for pid in self.tasks.pids(&req.id).map_err(status)? {
            let mut p = ProcessInfo::new();
            p.pid = pid;
            resp.processes.push(p);
        }
        Ok(resp)
    }

    fn exec(&self, _ctx: &TtrpcContext, req: api::ExecProcessRequest) -> TtrpcResult<api::Empty> {
        if req.exec_id.is_empty() {
            return Err(status(Error::invalid("an exec needs an exec id")));
        }
        let spec = req
            .spec
            .as_ref()
            .ok_or_else(|| status(Error::invalid("the exec has no process spec")))?;
        let (process, terminal) = translate::exec_process(&spec.value).map_err(status)?;
        let stdio = Stdio {
            stdin: req.stdin.clone(),
            stdout: req.stdout.clone(),
            stderr: req.stderr.clone(),
            terminal: terminal || req.terminal,
        };
        self.tasks.exec(&req.id, &req.exec_id, process, stdio).map_err(status)?;
        Ok(api::Empty::new())
    }

    fn resize_pty(&self, _ctx: &TtrpcContext, req: api::ResizePtyRequest) -> TtrpcResult<api::Empty> {
        let clamp = |n: u32| u16::try_from(n).unwrap_or(u16::MAX);
        let (rows, cols) = (clamp(req.height), clamp(req.width));
        match exec_id(&req.exec_id) {
            None => self.tasks.resize(&req.id, rows, cols),
            Some(x) => self.tasks.resize_exec(&req.id, x, rows, cols),
        }
        .map_err(status)?;
        Ok(api::Empty::new())
    }

    fn close_io(&self, _ctx: &TtrpcContext, req: api::CloseIORequest) -> TtrpcResult<api::Empty> {
        if req.stdin {
            match exec_id(&req.exec_id) {
                None => self.tasks.close_stdin(&req.id),
                Some(x) => self.tasks.close_stdin_exec(&req.id, x),
            }
            .map_err(status)?;
        }
        Ok(api::Empty::new())
    }

    fn connect(&self, _ctx: &TtrpcContext, req: api::ConnectRequest) -> TtrpcResult<api::ConnectResponse> {
        let mut resp = api::ConnectResponse::new();
        resp.shim_pid = std::process::id();
        resp.task_pid = self.tasks.state(&req.id).map(|s| s.pid).unwrap_or(0);
        resp.version = env!("CARGO_PKG_VERSION").into();
        Ok(resp)
    }

    fn shutdown(&self, _ctx: &TtrpcContext, _req: api::ShutdownRequest) -> TtrpcResult<api::Empty> {
        if self.tasks.is_empty() {
            self.exit.signal();
        }
        Ok(api::Empty::new())
    }
}

/// Sends an event to containerd; a failure is logged (containerd also polls state).
fn publish(publisher: &RemotePublisher, namespace: &str, event: Event) {
    let (topic, msg): (&str, Box<dyn MessageDyn>) = match event {
        Event::Create { id, bundle, pid, stdio } => {
            let mut io = TaskIO::new();
            io.stdin = stdio.stdin;
            io.stdout = stdio.stdout;
            io.stderr = stdio.stderr;
            io.terminal = stdio.terminal;
            let mut e = TaskCreate::new();
            e.container_id = id;
            e.bundle = bundle;
            e.io = MessageField::some(io);
            e.pid = pid;
            (TASK_CREATE_EVENT_TOPIC, Box::new(e))
        }
        Event::Start { id, pid } => {
            let mut e = TaskStart::new();
            e.container_id = id;
            e.pid = pid;
            (TASK_START_EVENT_TOPIC, Box::new(e))
        }
        Event::Exit { id, pid, exit } => {
            let mut e = TaskExit::new();
            e.container_id = id.clone();
            e.id = id;
            e.pid = pid;
            e.exit_status = exit.code;
            e.exited_at = MessageField::some(timestamp(exit.at));
            (TASK_EXIT_EVENT_TOPIC, Box::new(e))
        }
        Event::Delete { id, pid, exit } => {
            let mut e = TaskDelete::new();
            e.container_id = id.clone();
            e.id = id;
            e.pid = pid;
            e.exit_status = exit.code;
            e.exited_at = MessageField::some(timestamp(exit.at));
            (TASK_DELETE_EVENT_TOPIC, Box::new(e))
        }
        Event::ExecAdded { id, exec_id } => {
            let mut e = TaskExecAdded::new();
            e.container_id = id;
            e.exec_id = exec_id;
            (TASK_EXEC_ADDED_EVENT_TOPIC, Box::new(e))
        }
        Event::ExecStarted { id, exec_id, pid } => {
            let mut e = TaskExecStarted::new();
            e.container_id = id;
            e.exec_id = exec_id;
            e.pid = pid;
            (TASK_EXEC_STARTED_EVENT_TOPIC, Box::new(e))
        }
        Event::ExecExit { id, exec_id, pid, exit } => {
            let mut e = TaskExit::new();
            e.container_id = id;
            e.id = exec_id;
            e.pid = pid;
            e.exit_status = exit.code;
            e.exited_at = MessageField::some(timestamp(exit.at));
            (TASK_EXIT_EVENT_TOPIC, Box::new(e))
        }
    };
    if let Err(e) = publisher.publish(containerd_shim::Context::default(), topic, namespace, msg) {
        log::error!("publishing {topic}: {e}");
    }
}

/// The backend this platform runs tasks with.
fn backend() -> Arc<dyn Backend> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(crate::linux::LinuxBackend::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(Unsupported)
    }
}

/// Where VMs cannot run: every task is refused.
#[cfg(not(target_os = "linux"))]
struct Unsupported;

#[cfg(not(target_os = "linux"))]
impl Backend for Unsupported {
    fn create(&self, _task: &CreateTask) -> Result<Arc<dyn crate::tasks::Process>> {
        Err(Error::Unsupported("kiln-shim runs VMs on Linux with KVM only".into()))
    }
}

/// The shim containerd starts.
pub struct KilnShim {
    namespace: String,
    id: String,
    exit: Arc<ExitSignal>,
}

/// The crate's own reaper would take the exit statuses of the shim's children (the
/// pause and VM worker processes), which the shim waits for itself.
pub fn shim_config() -> Config {
    Config {
        no_reaper: true,
        no_sub_reaper: true,
        ..Config::default()
    }
}

impl containerd_shim::Shim for KilnShim {
    type T = Service;

    fn new(_runtime_id: &str, args: &Flags, _config: &mut Config) -> Self {
        Self {
            namespace: args.namespace.clone(),
            id: args.id.clone(),
            exit: Arc::new(ExitSignal::default()),
        }
    }

    fn start_shim(&mut self, opts: StartOpts) -> containerd_shim::Result<String> {
        // containerd starts the shim in the task's bundle.
        let group = match oci::read_spec(Path::new(".")) {
            Ok(spec) => group_id(&spec, &opts.id),
            Err(_) => opts.id.clone(),
        };
        let (_pid, address) = containerd_shim::spawn(opts, &group, Vec::new())?;
        containerd_shim::util::write_address(&address)?;
        Ok(address)
    }

    fn delete_shim(&mut self) -> containerd_shim::Result<DeleteResponse> {
        // The shim is gone (`delete` runs when it died): its children died with it
        // (PR_SET_PDEATHSIG), and their mounts with their mount namespaces.
        let mut resp = DeleteResponse::new();
        resp.exit_status = 128 + 9;
        resp.exited_at = MessageField::some(timestamp(SystemTime::now()));
        log::info!("delete after the shim of {} ended", self.id);
        Ok(resp)
    }

    fn wait(&mut self) {
        self.exit.wait();
    }

    fn create_task_service(&self, publisher: RemotePublisher) -> Self::T {
        let namespace = self.namespace.clone();
        Service::new(backend(), &self.namespace, self.exit.clone(), move |e| {
            publish(&publisher, &namespace, e)
        })
    }
}

#[cfg(test)]
mod tests {
    use containerd_shim::Task;
    use containerd_shim::protos::types::mount::Mount;

    use super::*;
    use crate::tasks::fake::FakeBackend;

    fn ctx() -> TtrpcContext {
        TtrpcContext {
            fd: -1,
            cancel_rx: crossbeam::channel::unbounded().1,
            mh: Default::default(),
            res_tx: std::sync::mpsc::channel().0,
            metadata: Default::default(),
            timeout_nano: 0,
        }
    }

    fn settings() -> Settings {
        Settings::parse("user = \"kiln\"\nstore = \"/s\"\nkernel = \"/k\"\ninit = \"/i\"\n").unwrap()
    }

    fn service() -> (Service, Arc<FakeBackend>) {
        let backend = Arc::new(FakeBackend::default());
        let s = Service::new(backend.clone(), "k8s.io", Arc::new(ExitSignal::default()), |_| {})
            .with_settings(|_| Ok(settings()));
        (s, backend)
    }

    fn bundle(spec: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), spec).unwrap();
        dir
    }

    fn mount(kind: &str, source: &str) -> Mount {
        let mut m = Mount::new();
        m.type_ = kind.into();
        m.source = source.into();
        m
    }

    fn create_req(id: &str, dir: &Path, rootfs: Vec<Mount>) -> api::CreateTaskRequest {
        let mut r = api::CreateTaskRequest::new();
        r.id = id.into();
        r.bundle = dir.display().to_string();
        r.rootfs = rootfs;
        r
    }

    const CONTAINER: &str = r#"{"process":{"args":["echo","hi"]},
        "annotations":{"io.kubernetes.cri.container-type":"container","io.kubernetes.cri.sandbox-id":"s1"}}"#;

    fn code(e: ttrpc::Error) -> Code {
        match e {
            ttrpc::Error::RpcStatus(s) => s.code(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn service_maps_errors_and_exec() {
        let (s, backend) = service();
        let err = s.exec(&ctx(), api::ExecProcessRequest::new()).unwrap_err();
        assert_eq!(code(err), Code::INVALID_ARGUMENT, "no exec id");
        let mut st = api::StateRequest::new();
        st.id = "nope".into();
        assert_eq!(code(s.state(&ctx(), st).unwrap_err()), Code::NOT_FOUND);

        let dir = bundle(CONTAINER);
        let overlay = vec![mount("overlay", "overlay")];
        let err = s.create(&ctx(), create_req("c1", dir.path(), overlay)).unwrap_err();
        match err {
            ttrpc::Error::RpcStatus(st) => {
                assert_eq!(st.code(), Code::INVALID_ARGUMENT);
                assert!(st.message.contains("snapshotter"), "{}", st.message);
            }
            other => panic!("{other:?}"),
        }

        let erofs = vec![mount("erofs", "/snap/1/layer.erofs")];
        let pid = s.create(&ctx(), create_req("c1", dir.path(), erofs)).unwrap().pid;
        let mut start = api::StartRequest::new();
        start.id = "c1".into();
        assert_eq!(s.start(&ctx(), start).unwrap().pid, pid);
        backend.get("c1").end(3);
        let mut w = api::WaitRequest::new();
        w.id = "c1".into();
        let resp = s.wait(&ctx(), w).unwrap();
        assert_eq!(resp.exit_status, 3);
        assert!(resp.exited_at.seconds > 0);
        let mut st = api::StateRequest::new();
        st.id = "c1".into();
        assert_eq!(s.state(&ctx(), st).unwrap().status, api::Status::STOPPED.into());
    }

    #[test]
    fn exec_through_the_service() {
        let (s, backend) = service();
        let dir = bundle(CONTAINER);
        s.create(&ctx(), create_req("c1", dir.path(), vec![mount("erofs", "/l")]))
            .unwrap();
        let mut start = api::StartRequest::new();
        start.id = "c1".into();
        let pid = s.start(&ctx(), start).unwrap().pid;
        let mut req = api::ExecProcessRequest::new();
        req.id = "c1".into();
        req.exec_id = "probe".into();
        let mut any = containerd_shim::protos::protobuf::well_known_types::any::Any::new();
        any.type_url = "types.containerd.io/opencontainers/runtime-spec/1/Process".into();
        any.value = br#"{"args":["cat","/ready"],"user":{"uid":0,"gid":0},"cwd":"/"}"#.to_vec();
        req.spec = MessageField::some(any);
        s.exec(&ctx(), req.clone()).unwrap();
        assert_eq!(
            code(s.exec(&ctx(), req).unwrap_err()),
            Code::FAILED_PRECONDITION,
            "twice"
        );
        let mut start = api::StartRequest::new();
        start.id = "c1".into();
        start.exec_id = "probe".into();
        assert_eq!(s.start(&ctx(), start).unwrap().pid, pid);
        let x = backend.get("c1").exec_with(1);
        assert_eq!(x.launch.process.entrypoint, ["cat", "/ready"]);
        let mut st = api::StateRequest::new();
        st.id = "c1".into();
        st.exec_id = "probe".into();
        assert_eq!(s.state(&ctx(), st.clone()).unwrap().status, api::Status::RUNNING.into());
        x.end(1);
        let mut w = api::WaitRequest::new();
        w.id = "c1".into();
        w.exec_id = "probe".into();
        assert_eq!(s.wait(&ctx(), w).unwrap().exit_status, 1);
        let mut d = api::DeleteRequest::new();
        d.id = "c1".into();
        d.exec_id = "probe".into();
        assert_eq!(s.delete(&ctx(), d).unwrap().exit_status, 1);
        assert_eq!(code(s.state(&ctx(), st).unwrap_err()), Code::NOT_FOUND);
        // The task itself is still there.
        let mut st = api::StateRequest::new();
        st.id = "c1".into();
        assert_eq!(s.state(&ctx(), st).unwrap().status, api::Status::RUNNING.into());
    }

    #[test]
    fn kill_signal_zero_checks_and_out_of_range_is_refused() {
        let (s, backend) = service();
        let dir = bundle(CONTAINER);
        s.create(&ctx(), create_req("c1", dir.path(), vec![mount("erofs", "/l")]))
            .unwrap();
        let kill = |sig: u32, id: &str| {
            let mut k = api::KillRequest::new();
            k.id = id.into();
            k.signal = sig;
            s.kill(&ctx(), k)
        };
        kill(0, "c1").unwrap();
        assert_eq!(code(kill(0, "nope").unwrap_err()), Code::NOT_FOUND);
        assert_eq!(code(kill(64, "c1").unwrap_err()), Code::INVALID_ARGUMENT);
        assert!(
            backend.get("c1").signals.lock().unwrap().is_empty(),
            "nothing reached the VM"
        );
        kill(15, "c1").unwrap();
        assert_eq!(backend.get("c1").signals.lock().unwrap().as_slice(), [15]);
    }

    #[test]
    fn a_sandbox_with_the_hosts_network_is_refused() {
        let (s, _) = service();
        let dir = bundle(
            r#"{"annotations":{"io.kubernetes.cri.container-type":"sandbox","io.kubernetes.cri.sandbox-id":"s1"},
                "linux":{"namespaces":[{"type":"pid"}]}}"#,
        );
        let err = s.create(&ctx(), create_req("s1", dir.path(), Vec::new())).unwrap_err();
        assert_eq!(code(err), Code::INVALID_ARGUMENT);
    }

    #[test]
    fn launch_translates_the_spec() {
        let dir = bundle(CONTAINER);
        let spec = oci::read_spec(dir.path()).unwrap();
        let mut req = create_req(
            "c1",
            dir.path(),
            vec![
                mount("erofs", "/snap/2/layer.erofs"),
                mount("erofs", "/snap/1/layer.erofs"),
            ],
        );
        req.terminal = true;
        req.stdin = "/fifo/in".into();
        let Launch::Vm(v) = launch(&req, &spec, "k8s.io", Some("p2"), &|_| Ok(settings())).unwrap() else {
            panic!("a container is a VM");
        };
        assert_eq!(v.hostname, "p2", "the pod's hostname");
        assert_eq!(
            v.layers,
            [
                PathBuf::from("/snap/1/layer.erofs"),
                PathBuf::from("/snap/2/layer.erofs")
            ]
        );
        assert_eq!(v.process.entrypoint, ["echo", "hi"]);
        assert!(v.tty && v.interactive);
        assert_eq!((v.namespace.as_str(), v.task_id.as_str()), ("k8s.io", "c1"));
    }

    #[test]
    fn only_the_sandboxs_own_resolv_conf_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("sandboxes/s1");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(own.join("resolv.conf"), "search a.svc\n").unwrap();
        assert_eq!(
            read_resolv_conf(&own.join("resolv.conf"), Some("s1")).as_deref(),
            Some("search a.svc\n")
        );
        // Another pod's, a file of another name, no sandbox: not read.
        assert_eq!(read_resolv_conf(&own.join("resolv.conf"), Some("s2")), None);
        assert_eq!(read_resolv_conf(&own.join("resolv.conf"), None), None);
        std::fs::write(own.join("shadow"), "search secret\n").unwrap();
        assert_eq!(read_resolv_conf(&own.join("shadow"), Some("s1")), None);
        // A directory in its place.
        let d = dir.path().join("sandboxes/s3/resolv.conf");
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(read_resolv_conf(&d, Some("s3")), None);
    }

    #[test]
    fn runtime_info_allows_erofs() {
        let info = runtime_info();
        assert_eq!(info.name, "io.containerd.kiln.v1");
        assert_eq!(
            info.annotations.get(ALLOW_MOUNTS).map(String::as_str),
            Some("mkdir/*,format/*,erofs")
        );
    }

    #[test]
    fn task_directories_keep_socket_paths_short() {
        let id = "47d1f8c701f6a1f59e392bfdea6666ec8cc8c0aa416d52947579cb7778e152e4";
        let name = task_dir_name("k8s.io", id);
        assert_eq!(name, "k8s.io-47d1f8c701f6a1f59e392bfd");
        let sock = format!("/run/kiln-shim/vm/{name}/run/sock/firecracker.sock");
        assert!(sock.len() < 100, "{sock}");
        assert_eq!(task_dir_name("a/b-c", "../x"), "a_b_c-.._x");
    }

    #[test]
    fn group_by_sandbox_id() {
        let spec: Spec = serde_json::from_str(CONTAINER).unwrap();
        assert_eq!(group_id(&spec, "c1"), "s1");
        assert_eq!(group_id(&Spec::default(), "c1"), "c1");
    }

    #[test]
    fn shutdown_waits_for_the_last_task() {
        let backend = Arc::new(FakeBackend::default());
        let exit = Arc::new(ExitSignal::default());
        let s = Service::new(backend, "k8s.io", exit.clone(), |_| {}).with_settings(|_| Ok(settings()));
        let dir = bundle(CONTAINER);
        s.create(&ctx(), create_req("c1", dir.path(), vec![mount("erofs", "/l")]))
            .unwrap();
        s.shutdown(&ctx(), api::ShutdownRequest::new()).unwrap();
        let mut d = api::DeleteRequest::new();
        d.id = "c1".into();
        s.delete(&ctx(), d).unwrap();
        s.shutdown(&ctx(), api::ShutdownRequest::new()).unwrap();
        // The signal was given: waiting returns at once.
        exit.wait();
    }
}
