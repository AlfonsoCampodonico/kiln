//! A build VM (spec §9.8): boots `Config { mode: build }`, then runs one step at
//! a time and reports where each step's diff landed.
//!
//! Threads, as in a run's session ([`super::session`]): a listener over the
//! control, stdout and stderr ports (the control port is served once; each step
//! connects stdout and stderr anew, and a connection while no step runs, or a
//! second one in a step, is a violation); the control reader, which answers
//! `Hello` with `Config` and passes every guest message through
//! [`BuildProtocol`]; the control writer, the only thread that writes host
//! messages, from a bounded queue with a send timeout; and one reader per step
//! stream. They all feed one bounded channel, which only `run_step`, on the
//! caller's thread, drains: output goes to the caller's sink there.
//!
//! Every wait is bounded: `start` by the boot timeout; `run_step` signals the
//! step's process group with SIGKILL at its timeout and kills the VM
//! [`STEP_GRACE`] later, then waits at most [`KILL_WAIT`] for the VMM;
//! `finish` gives the guest [`END_TIMEOUT`] to end the VM. A protocol violation
//! kills the VM, and the `BuildVm` refuses further steps.
//!
//! kiln never reads the output disk. [`BuildProtocol`] checks a `StepSealed`'s
//! shape, its id and that its range is a possible diff (aligned, starting where
//! the previous one ended, within the disk); a guest whose seal is not is a
//! protocol violation. The caller still checks the zeros after the tar's end
//! and hashes what it reads.

use std::collections::BTreeSet;
use std::io::{ErrorKind, Read};
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kiln_proto::{Config, GuestMessage, HostMessage, Signal, port, read_message, write_message};
use kiln_store::Store;
use rustix::event::{PollFd, PollFlags, poll};
use vmkit::{Disk, EgressGate, EndReason, NetSpec, Vm, VmSpec, VsockSpec};

use super::console::ConsoleRelay;
use super::run::{CMDLINE, backend, exit_method, vmm_identity};
use super::session::bind;
use crate::build::{BuildEvent, BuildProtocol, BuildSpec, Exit, KILL_WAIT, RunStep, STEP_GRACE, StepOutcome, Stream};
use crate::console::{TAIL_LINES, tail};
use crate::error::{Error, Result};
use crate::rundir::{self, RUN_INFO_VERSION, RunDir, RunInfo, ScratchDir, ScratchInfo};

/// How long the guest has to end the VM after `Finish`.
pub const END_TIMEOUT: Duration = Duration::from_secs(30);
const QUEUE: usize = 256;
const WRITE_QUEUE: usize = 16;
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(20);
/// The most bytes one output event carries.
const CHUNK: usize = 64 * 1024;

/// The largest `BuildSpec` a VMM can hold: its devices left for layer and
/// cache disks once the fixed ones (and a NIC with `network`) are counted.
pub fn max_devices(vmm: &dyn vmkit::Vmm, network: bool) -> usize {
    crate::build::device_budget(vmm.capabilities().available_devices(), network)
}

enum Event {
    Guest(GuestMessage),
    Violation(String),
    ControlClosed,
    Stream(u32, UnixStream),
    Output(Stream, Vec<u8>),
    Eof(Stream),
}

/// A running build VM.
pub struct BuildVm {
    spec: BuildSpec,
    vm: Box<dyn Vm>,
    events: Receiver<Event>,
    /// For the output readers.
    tx: SyncSender<Event>,
    out: SyncSender<HostMessage>,
    proto: BuildProtocol,
    stacked: BTreeSet<usize>,
    /// Steps sealed so far: each sealed upper is one more lower.
    sealed: usize,
    /// The VM's egress gate (with `network`): closed from boot, opened for the
    /// steps that ask for the network (vmkit's `Vm::set_egress`).
    gate: EgressGate,
    /// Why the VM is unusable, once it is.
    dead: Option<String>,
    /// The VMM is known to have exited (a `wait_timeout` returned its end).
    /// `dead` alone does not say: a killed VMM can outlive [`KILL_WAIT`].
    vm_ended: bool,
    wake: OwnedFd,
    listener: Option<JoinHandle<()>>,
    console: Option<ConsoleRelay>,
    readers: Vec<JoinHandle<()>>,
    /// Removed by `Drop` once the VMM is known gone, in this order; left for a
    /// later run's orphan reaper when it is not.
    scratch_dir: Option<ScratchDir>,
    run_dir: Option<RunDir>,
}

/// Whether the VMM ended the way a guest ending its own VM looks: the VMM
/// exited with code 0 and no signal, or the guest reset and vmkit stopped it
/// there. vmkit's `Exited` also covers a VMM that failed, hence the code check.
fn ended_by_the_guest(end: &vmkit::VmEnd) -> bool {
    match end.reason {
        EndReason::Exited => end.code == Some(0) && end.signal.is_none(),
        EndReason::ResetStopped => true,
        // Killed, BackstopFailed, and any reason added later: not the guest's doing.
        _ => false,
    }
}

/// `t + d`, never panicking: a duration too large for `Instant` (a caller's
/// `Duration::MAX` timeout) is clamped to a year from `t`, which is effectively
/// never for a step or a boot. Every `Instant + Duration` in this file goes through it.
fn after(t: Instant, d: Duration) -> Instant {
    const NEVER: Duration = Duration::from_secs(365 * 24 * 3600);
    t.checked_add(d).or_else(|| t.checked_add(NEVER)).unwrap_or(t)
}

impl BuildVm {
    /// Creates the run's directories, the init layer and the scratch disk,
    /// boots the VM and waits for the guest to be ready (`Running`).
    pub fn start(store: &Store, spec: BuildSpec) -> Result<Self> {
        let vmm = backend(spec.vmm).discover()?;
        spec.check(vmm.capabilities().available_devices())?;
        let len = std::fs::metadata(&spec.output)
            .map_err(|e| Error::refused(format!("the output disk {}: {e}", spec.output.display())))?
            .len();
        if len != spec.output_size {
            return Err(Error::refused(format!(
                "the output disk {} has {len} bytes, not {}",
                spec.output.display(),
                spec.output_size
            )));
        }
        let bin = std::fs::read(&spec.init)?;
        let init_layer = kiln_image::init_layer(&bin, &store.tmp_dir())?;

        let base = rundir::base_dir()?;
        let mut run_dir = RunDir::create(&base)?;
        let keep = std::env::var_os("KILN_KEEP_RUN_DIR").is_some_and(|v| v == "1");
        if keep {
            run_dir.keep();
        }
        let scratch_base = rundir::scratch_base(store.root())?;
        let scratch_path = scratch_base.join(&run_dir.id);
        let mut info = RunInfo {
            version: RUN_INFO_VERSION,
            id: run_dir.id.clone(),
            boot_id: rundir::boot_id(),
            hostname: rundir::hostname(),
            kiln: rundir::identity(std::process::id()).ok_or_else(|| Error::refused("cannot read /proc/self/stat"))?,
            vmm: None,
            vmm_kind: spec.vmm.name().into(),
            image: "(build)".into(),
            custom_kernel: Some(spec.kernel.display().to_string()),
            custom_init: Some(spec.init.display().to_string()),
            keep,
            scratch_dir: Some(scratch_path.display().to_string()),
            persist: None,
        };
        run_dir.write_info(&info)?;
        let mut scratch_dir = ScratchDir::create(&scratch_base, &ScratchInfo::from(&info))?;
        if keep {
            scratch_dir.keep();
        }
        let init = run_dir.path.join("init.erofs");
        std::fs::write(&init, &init_layer)?;
        let scratch = scratch_dir.path.join("scratch.img");
        kiln_image::scratch::create_scratch(&scratch, spec.scratch_bytes)?;

        let console = ConsoleRelay::start(&run_dir.path)?;
        let mut disks = vec![
            Disk {
                path: init,
                read_only: true,
            },
            Disk {
                path: scratch,
                read_only: false,
            },
            Disk {
                path: spec.output.clone(),
                read_only: false,
            },
        ];
        disks.extend(spec.layers.iter().map(|p| Disk {
            path: p.clone(),
            read_only: true,
        }));
        disks.extend(spec.cache_disks.iter().map(|(p, _)| Disk {
            path: p.clone(),
            read_only: false,
        }));
        let vm_spec = VmSpec {
            kernel: spec.kernel.clone(),
            initramfs: None,
            cmdline: CMDLINE.map(String::from).to_vec(),
            disks,
            vcpus: spec.cpus,
            memory_mib: spec.memory_mib,
            vsock: Some(VsockSpec {
                guest_cid: kiln_proto::GUEST_CID,
            }),
            // IPv4 only, vmkit's restricted egress; each step turns it on or off.
            net: spec.network.then(NetSpec::default),
            tap: None,
            console_log: console.fifo.clone(),
            run_dir: run_dir.path.clone(),
        };
        let config = spec.config(exit_method(vmm.as_ref()));
        config.validate()?;
        let mut vm = vmm.create(&vm_spec)?;
        // Boot closed: no step has asked for the network yet.
        let spec_network = spec.network;
        let output_size = spec.output_size;
        if spec_network {
            vm.set_egress(EgressGate::Closed)?;
        }
        info.vmm = vmm_identity(&run_dir.path);
        run_dir.write_info(&info)?;
        scratch_dir.write_info(&ScratchInfo::from(&info))?;

        let sock = vm
            .vsock_socket()
            .ok_or_else(|| Error::refused("the VM has no vsock device"))?
            .to_path_buf();
        let listeners = [port::CONTROL, port::STDOUT, port::STDERR]
            .iter()
            .map(|&p| Ok((p, bind(&sock, p)?)))
            .collect::<Result<Vec<_>>>()?;
        let (tx, events) = sync_channel(QUEUE);
        let (out, out_rx) = sync_channel(WRITE_QUEUE);
        let (wake_r, wake) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(std::io::Error::from)?;
        let listener = {
            let tx = tx.clone();
            std::thread::Builder::new()
                .name("build-listener".into())
                .spawn(move || listen(listeners, &wake_r, config, &tx, out_rx))?
        };
        if let Err(e) = vm.start() {
            let _ = rustix::io::write(&wake, b"x");
            let _ = listener.join();
            return Err(e.into());
        }
        let mut b = BuildVm {
            spec,
            vm,
            events,
            tx,
            out,
            proto: BuildProtocol::new(output_size),
            stacked: BTreeSet::new(),
            sealed: 0,
            gate: if spec_network {
                EgressGate::Closed
            } else {
                EgressGate::Policy
            },
            dead: None,
            vm_ended: false,
            wake,
            listener: Some(listener),
            console: Some(console),
            readers: Vec::new(),
            scratch_dir: Some(scratch_dir),
            run_dir: Some(run_dir),
        };
        let deadline = after(Instant::now(), b.spec.boot_timeout);
        while !b.proto.running() {
            if Instant::now() >= deadline {
                b.kill(format!(
                    "boot timeout: the guest was not ready {}s after the VM started",
                    b.spec.boot_timeout.as_secs()
                ));
            }
            if let Some(why) = &b.dead {
                return Err(Error::refused(format!(
                    "the build VM failed to boot: {why}{}",
                    b.console_tail()
                )));
            }
            b.turn(deadline, &mut |_, _| {});
        }
        Ok(b)
    }

    /// The output disk, for the caller to read each step's diff from.
    pub fn output_path(&self) -> &Path {
        &self.spec.output
    }

    /// The run directory (console log, VMM logs).
    pub fn run_dir(&self) -> Option<&Path> {
        self.run_dir.as_ref().map(|d| d.path.as_path())
    }

    /// Runs `step`, streaming its output to `sink`. Returns within
    /// `step.timeout + STEP_GRACE + KILL_WAIT`. An error means the VM is gone (a
    /// protocol violation, the guest failed, the VMM ended): no more steps.
    /// A [`Exit::TimedOut`] outcome is `Ok` but also ends the VM (kiln kills
    /// it, as the guest's state after a killed step is not known): the next
    /// `run_step` and `finish` return `Err`.
    pub fn run_step(&mut self, step: RunStep, sink: &mut dyn FnMut(Stream, &[u8])) -> Result<StepOutcome> {
        if let Some(why) = &self.dead {
            return Err(Error::refused(format!("the build VM has ended: {why}")));
        }
        // The deadline counts from here: the egress change is part of the step's time.
        let start = Instant::now();
        // A refusal here (the lowers cap included) sends nothing: the VM stays usable.
        let msg = self.spec.proto_step(&step, &self.stacked, self.sealed)?;
        if self.spec.network {
            let gate = if step.network {
                EgressGate::Policy
            } else {
                EgressGate::Closed
            };
            if gate != self.gate {
                // A failure leaves the gate as it was; the VM is not used again.
                if let Err(e) = self.vm.set_egress(gate) {
                    self.kill(format!("setting step {}'s egress: {e}", step.id));
                    return Err(e.into());
                }
                self.gate = gate;
            }
        }
        self.proto.start_step(step.id).map_err(Error::refused)?;
        self.stacked.extend(step.lowers_added.iter().copied());
        self.send(HostMessage::RunStep(Box::new(msg)));
        let deadline = after(start, step.timeout);
        let kill_at = after(deadline, STEP_GRACE);
        let mut timed_out = false;
        let mut s = StepState::default();
        loop {
            if self.dead.is_none() && !timed_out && Instant::now() >= deadline {
                timed_out = true;
                self.send(HostMessage::Signal(Signal { sig: 9 }));
            }
            if self.dead.is_none() && Instant::now() >= kill_at {
                self.kill(format!(
                    "step {} did not end within {}s of its {}s timeout",
                    step.id,
                    STEP_GRACE.as_secs(),
                    step.timeout.as_secs()
                ));
                return Ok(StepOutcome {
                    exit: Exit::TimedOut,
                    sealed: None,
                });
            }
            if let Some(why) = &self.dead {
                return Err(Error::refused(format!(
                    "step {}: {why}{}",
                    step.id,
                    self.console_tail()
                )));
            }
            if let Some(outcome) = s.done(timed_out) {
                self.proto.end_step();
                if timed_out {
                    // The guest's state after a killed step is not one the caller knows.
                    self.kill(format!("step {} timed out", step.id));
                }
                if outcome.sealed.is_some() {
                    self.sealed += 1;
                }
                return Ok(outcome);
            }
            let until = if timed_out { kill_at } else { deadline };
            self.step_turn(until, &mut s, sink);
        }
    }

    /// `Finish`: the guest syncs and ends the VM; killed after [`END_TIMEOUT`].
    /// `Ok` only when the guest ended the VM cleanly (the VMM exited with code 0
    /// and no signal, or the guest reset it). After a [`Exit::TimedOut`] step
    /// the VM is already dead, so this is `Err`. An error means the VM was
    /// already gone, `Finish` never reached it, the VMM ended some other way
    /// than the guest ending it (killed, or its reset backstop failed), the
    /// guest reported `InitFailed` after `Finish`, or it did not end in time.
    pub fn finish(mut self) -> Result<()> {
        if let Some(why) = &self.dead {
            return Err(Error::refused(format!("the build VM had ended before Finish: {why}")));
        }
        self.send(HostMessage::Finish);
        if let Some(why) = &self.dead {
            return Err(Error::refused(format!(
                "Finish was not delivered, and the build VM was killed: {why}"
            )));
        }
        let deadline = after(Instant::now(), END_TIMEOUT);
        let mut failed = None;
        loop {
            match self.vm.wait_timeout(TICK) {
                Ok(Some(end)) => {
                    self.vm_ended = true;
                    self.dead = Some("finished".into());
                    // What the guest sent just before the end.
                    self.drain_after_finish(&mut failed);
                    if let Some(f) = failed {
                        return Err(Error::refused(format!(
                            "the guest failed after Finish: {}",
                            f.describe()
                        )));
                    }
                    return if ended_by_the_guest(&end) {
                        Ok(())
                    } else {
                        Err(Error::refused(format!(
                            "the build VM ended after Finish, but not by the guest ending it ({:?}, code {:?}, signal {:?})",
                            end.reason, end.code, end.signal
                        )))
                    };
                }
                Ok(None) if Instant::now() < deadline => {}
                Ok(None) => {
                    self.kill("the guest did not end the VM after Finish".into());
                    return Err(Error::refused(format!(
                        "the build VM did not end within {}s of Finish; it was killed",
                        END_TIMEOUT.as_secs()
                    )));
                }
                Err(e) => {
                    self.kill(format!("waiting for the VMM after Finish: {e}"));
                    return Err(Error::refused(format!(
                        "waiting for the build VMM after Finish failed ({e}); it was killed"
                    )));
                }
            }
            self.drain_after_finish(&mut failed);
            if let Some(f) = failed {
                self.kill("the guest reported InitFailed after Finish".into());
                return Err(Error::refused(format!(
                    "the guest failed after Finish: {}",
                    f.describe()
                )));
            }
        }
    }

    /// Messages after `Finish` matter only for an `InitFailed`; the rest are
    /// dropped to keep the channel moving.
    fn drain_after_finish(&mut self, failed: &mut Option<kiln_proto::InitFailed>) {
        while let Ok(e) = self.events.try_recv() {
            if let Event::Guest(msg) = e
                && let Ok(BuildEvent::Failed(f)) = self.proto.accept(&msg)
            {
                failed.get_or_insert(f);
            }
        }
    }

    fn send(&mut self, msg: HostMessage) {
        match self.out.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.kill("the guest is not reading its control channel".into()),
            Err(TrySendError::Disconnected(_)) => self.kill("the control connection is gone".into()),
        }
    }

    /// Kills the VM (once) and records why; waits at most [`KILL_WAIT`] for it.
    fn kill(&mut self, why: String) {
        if self.dead.is_some() {
            return;
        }
        self.dead = Some(why);
        let _ = self.vm.kill();
        self.vm_ended = matches!(self.vm.wait_timeout(KILL_WAIT), Ok(Some(_)));
    }

    fn console_tail(&self) -> String {
        let Some(c) = &self.console else {
            return String::new();
        };
        let lines = tail(&c.log, TAIL_LINES);
        if lines.is_empty() {
            String::new()
        } else {
            format!("\nthe console's end:\n{}", lines.join("\n"))
        }
    }

    /// One wait for an event outside a step (boot).
    fn turn(&mut self, until: Instant, sink: &mut dyn FnMut(Stream, &[u8])) {
        let mut s = StepState {
            active: false,
            ..StepState::default()
        };
        self.step_turn(until, &mut s, sink);
    }

    /// Waits up to a tick (or `until`) for one event and handles it; notices the VM's end.
    fn step_turn(&mut self, until: Instant, s: &mut StepState, sink: &mut dyn FnMut(Stream, &[u8])) {
        if self.dead.is_none() {
            match self.vm.wait_timeout(Duration::ZERO) {
                Ok(Some(end)) => {
                    self.vm_ended = true;
                    self.dead = Some(format!("the VM ended ({:?})", end.reason));
                    return;
                }
                Ok(None) => {}
                Err(e) => {
                    self.kill(format!("waiting for the VMM: {e}"));
                    return;
                }
            }
        }
        let wait = TICK.min(until.saturating_duration_since(Instant::now()));
        let event = match self.events.recv_timeout(wait) {
            Ok(e) => e,
            Err(RecvTimeoutError::Timeout) => return,
            Err(RecvTimeoutError::Disconnected) => {
                self.kill("the listener stopped".into());
                return;
            }
        };
        match event {
            Event::Guest(msg) => match self.proto.accept(&msg) {
                Ok(BuildEvent::Boot) => {}
                Ok(BuildEvent::Exited(e)) => s.exited = Some(e),
                Ok(BuildEvent::Sealed(seal)) => s.sealed = Some(seal),
                Ok(BuildEvent::Failed(f)) => self.kill(f.describe()),
                Err(why) => self.kill(format!("guest protocol violation: {why}")),
            },
            Event::Violation(why) => self.kill(why),
            Event::ControlClosed => self.kill("the guest closed the control connection".into()),
            Event::Stream(p, stream) => {
                let which = if p == port::STDOUT {
                    Stream::Stdout
                } else {
                    Stream::Stderr
                };
                let slot = match which {
                    Stream::Stdout => &mut s.stdout,
                    Stream::Stderr => &mut s.stderr,
                };
                if !s.active || *slot != StreamState::Waiting {
                    self.kill(format!("an unexpected connection on vsock port {p}"));
                    return;
                }
                *slot = StreamState::Open;
                let tx = self.tx.clone();
                match std::thread::Builder::new()
                    .name("build-output".into())
                    .spawn(move || read_stream(stream, which, &tx))
                {
                    Ok(h) => self.readers.push(h),
                    Err(e) => self.kill(format!("spawn an output reader: {e}")),
                }
            }
            Event::Output(which, bytes) => sink(which, &bytes),
            Event::Eof(which) => match which {
                Stream::Stdout => s.stdout = StreamState::Done,
                Stream::Stderr => s.stderr = StreamState::Done,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum StreamState {
    #[default]
    Waiting,
    Open,
    Done,
}

#[derive(Debug)]
struct StepState {
    /// A step is running: its stream connections are expected.
    active: bool,
    stdout: StreamState,
    stderr: StreamState,
    exited: Option<kiln_proto::StepExited>,
    sealed: Option<kiln_proto::StepSealed>,
}

impl Default for StepState {
    fn default() -> Self {
        Self {
            active: true,
            stdout: StreamState::Waiting,
            stderr: StreamState::Waiting,
            exited: None,
            sealed: None,
        }
    }
}

impl StepState {
    /// The step's outcome, once its exit, both streams' EOF and (after exit 0) its seal are in.
    fn done(&self, timed_out: bool) -> Option<StepOutcome> {
        let e = self.exited?;
        if self.stdout != StreamState::Done || self.stderr != StreamState::Done {
            return None;
        }
        let exit = match (timed_out, e.code, e.signal) {
            (true, _, _) => Exit::TimedOut,
            (false, Some(c), _) => Exit::Code(c),
            // Decoding validated exactly one of the two.
            (false, None, sig) => Exit::Signal(sig.unwrap_or_default()),
        };
        if e.code == Some(0) {
            let sealed = self.sealed.clone()?;
            return Some(StepOutcome {
                exit: if timed_out { Exit::TimedOut } else { exit },
                sealed: (!timed_out).then_some(sealed),
            });
        }
        Some(StepOutcome { exit, sealed: None })
    }
}

impl Drop for BuildVm {
    fn drop(&mut self) {
        // The directories go only once the VMM is known gone: `dead` is set
        // before a kill that can outlive KILL_WAIT.
        if !self.vm_ended {
            let _ = self.vm.kill();
            self.vm_ended = matches!(self.vm.wait_timeout(KILL_WAIT), Ok(Some(_)));
        }
        let _ = rustix::io::write(&self.wake, b"x");
        if let Some(l) = self.listener.take() {
            join_draining(l, &self.events);
        }
        // Unblocks readers waiting on a full channel.
        while self.events.try_recv().is_ok() {}
        if let Some(c) = self.console.take() {
            c.finish(Duration::from_secs(2));
        }
        // A VMM that may still run keeps using the disks: leave both directories
        // to a later run's orphan reaper, which finds it by its run.json.
        match (self.vm_ended, self.scratch_dir.take(), self.run_dir.take()) {
            (true, scratch, run) => {
                drop(scratch);
                drop(run);
            }
            (false, scratch, run) => {
                if let Some(d) = scratch {
                    d.leave();
                }
                if let Some(d) = run {
                    d.leave();
                }
            }
        }
    }
}

/// Joins `handle`, draining `events` meanwhile: a thread blocked sending into
/// the full channel cannot see its wake-up until someone makes room.
fn join_draining<T>(handle: JoinHandle<()>, events: &Receiver<T>) {
    while !handle.is_finished() {
        while events.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(1));
    }
    let _ = handle.join();
}

/// Accepts on the control, stdout and stderr ports until woken. The control
/// port is served once; stream connections go to `run_step`, which decides.
fn listen(
    listeners: Vec<(u32, UnixListener)>,
    wake: &OwnedFd,
    config: Config,
    tx: &SyncSender<Event>,
    out: Receiver<HostMessage>,
) {
    let mut out = Some(out);
    let mut control = false;
    loop {
        let mut fds: Vec<PollFd<'_>> = listeners
            .iter()
            .map(|(_, l)| PollFd::new(l, PollFlags::IN))
            .chain([PollFd::new(wake, PollFlags::IN)])
            .collect();
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                let _ = tx.send(Event::Violation(format!("vsock listener: {e}")));
                return;
            }
        }
        let ready: Vec<bool> = fds.iter().map(|f| !f.revents().is_empty()).collect();
        drop(fds);
        if ready.last().copied().unwrap_or(true) {
            return;
        }
        for ((p, l), is_ready) in listeners.iter().zip(ready) {
            if !is_ready {
                continue;
            }
            let stream = match l.accept() {
                Ok((s, _)) => s,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => continue,
                Err(e) => {
                    let _ = tx.send(Event::Violation(format!("accept on vsock port {p}: {e}")));
                    return;
                }
            };
            if stream.set_nonblocking(false).is_err() {
                continue;
            }
            if *p != port::CONTROL {
                if tx.send(Event::Stream(*p, stream)).is_err() {
                    return;
                }
                continue;
            }
            if control {
                let _ = tx.send(Event::Violation("a second connection on the control port".into()));
                continue;
            }
            control = true;
            let Some(out) = out.take() else { continue };
            let (config, ctx) = (config.clone(), tx.clone());
            let spawned = std::thread::Builder::new()
                .name("build-control".into())
                .spawn(move || read_control(stream, &config, &ctx, out));
            if let Err(e) = spawned {
                let _ = tx.send(Event::Violation(format!("spawn the control reader: {e}")));
            }
        }
    }
}

/// Reads guest messages; answers a version 3 `Hello` with `Config` and then
/// starts the writer. The main loop checks the order ([`BuildProtocol`]).
fn read_control(stream: UnixStream, config: &Config, tx: &SyncSender<Event>, out: Receiver<HostMessage>) {
    let violation = |why: String| {
        let _ = tx.send(Event::Violation(why));
    };
    if let Err(e) =
        rustix::net::sockopt::set_socket_timeout(&stream, rustix::net::sockopt::Timeout::Send, Some(SEND_TIMEOUT))
    {
        return violation(format!("control socket: setting its send timeout: {e}"));
    }
    let mut reader = match stream.try_clone() {
        Ok(r) => r,
        Err(e) => return violation(format!("control socket: {e}")),
    };
    let mut writer = Some((stream, out));
    loop {
        let msg = match read_message::<_, GuestMessage>(&mut reader) {
            Ok(Some(m)) => m,
            Ok(None) => {
                let _ = tx.send(Event::ControlClosed);
                return;
            }
            Err(e) => return violation(format!("guest protocol error: {e}")),
        };
        let hello = matches!(msg, GuestMessage::Hello(_));
        let version = if let GuestMessage::Hello(h) = &msg {
            h.protocol
        } else {
            0
        };
        if tx.send(Event::Guest(msg)).is_err() {
            return;
        }
        if hello && let Some((mut w, out)) = writer.take() {
            if let Some(why) = crate::protocol::version_refusal(version, config) {
                return violation(why);
            }
            if let Err(e) = write_message(&mut w, &HostMessage::Config(Box::new(config.clone()))) {
                return violation(format!("sending Config: {e}"));
            }
            let tx = tx.clone();
            let _ = std::thread::Builder::new()
                .name("build-writer".into())
                .spawn(move || write_control(w, &out, &tx));
        }
    }
}

/// Writes queued host messages; a stalled send is a violation.
fn write_control(mut w: UnixStream, out: &Receiver<HostMessage>, tx: &SyncSender<Event>) {
    for msg in out {
        if let Err(e) = write_message(&mut w, &msg) {
            if let kiln_proto::ProtoError::Io(io) = &e
                && matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
            {
                let _ = tx.send(Event::Violation("the guest is not reading its control channel".into()));
            }
            return;
        }
    }
}

/// Streams one step's output to the main loop, then its EOF.
fn read_stream(mut from: UnixStream, which: Stream, tx: &SyncSender<Event>) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let Some(chunk) = buf.get(..n) else { break };
                if tx.send(Event::Output(which, chunk.to_vec())).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = tx.send(Event::Eof(which));
}

#[cfg(test)]
mod tests {
    use vmkit::VmEnd;

    use super::*;

    #[test]
    fn only_a_clean_end_counts_as_the_guest_ending_the_vm() {
        let end = |reason, code, signal| VmEnd { reason, code, signal };
        assert!(ended_by_the_guest(&end(EndReason::Exited, Some(0), None)));
        assert!(ended_by_the_guest(&end(EndReason::ResetStopped, None, None)));
        // vmkit's `Exited` also covers a VMM that failed on its own.
        assert!(!ended_by_the_guest(&end(EndReason::Exited, Some(1), None)));
        assert!(!ended_by_the_guest(&end(EndReason::Exited, None, None)));
        assert!(!ended_by_the_guest(&end(EndReason::Exited, None, Some(11))));
        assert!(!ended_by_the_guest(&end(EndReason::Exited, Some(0), Some(9))));
        assert!(!ended_by_the_guest(&end(EndReason::Killed, None, Some(9))));
        assert!(!ended_by_the_guest(&end(EndReason::BackstopFailed, None, Some(9))));
    }

    #[test]
    fn huge_durations_do_not_overflow_an_instant() {
        let t = Instant::now();
        assert!(after(t, Duration::MAX) > t + Duration::from_secs(3600));
        assert_eq!(after(t, Duration::from_secs(5)), t + Duration::from_secs(5));
    }

    fn exited(code: Option<i32>, signal: Option<i32>) -> kiln_proto::StepExited {
        kiln_proto::StepExited { id: 1, code, signal }
    }

    fn seal() -> kiln_proto::StepSealed {
        kiln_proto::StepSealed {
            id: 1,
            offset: 0,
            len: 4096,
            sha256: "0".repeat(64),
            entries: 1,
        }
    }

    fn state(e: Option<kiln_proto::StepExited>, sealed: bool) -> StepState {
        StepState {
            active: true,
            stdout: StreamState::Done,
            stderr: StreamState::Done,
            exited: e,
            sealed: sealed.then(seal),
        }
    }

    #[test]
    fn a_step_is_done_by_its_exit_streams_and_seal() {
        let out = |exit, sealed: bool| {
            Some(StepOutcome {
                exit,
                sealed: sealed.then(seal),
            })
        };
        // Not timed out.
        assert_eq!(state(None, false).done(false), None, "no exit yet");
        assert_eq!(
            state(Some(exited(Some(0), None)), false).done(false),
            None,
            "exit 0 waits for its seal"
        );
        assert_eq!(
            state(Some(exited(Some(0), None)), true).done(false),
            out(Exit::Code(0), true)
        );
        assert_eq!(
            state(Some(exited(Some(3), None)), false).done(false),
            out(Exit::Code(3), false)
        );
        assert_eq!(
            state(Some(exited(None, Some(9))), false).done(false),
            out(Exit::Signal(9), false)
        );
        // A stream still open holds the outcome back, whatever the exit.
        let mut open = state(Some(exited(Some(1), None)), false);
        open.stderr = StreamState::Open;
        assert_eq!(open.done(false), None);
        open.stderr = StreamState::Done;
        open.stdout = StreamState::Waiting;
        assert_eq!(open.done(false), None);
        // Timed out: the outcome is TimedOut and a seal is dropped.
        assert_eq!(state(None, false).done(true), None);
        assert_eq!(
            state(Some(exited(Some(0), None)), false).done(true),
            None,
            "exit 0 without its seal still waits (the kill deadline ends it)"
        );
        assert_eq!(
            state(Some(exited(Some(0), None)), true).done(true),
            out(Exit::TimedOut, false)
        );
        assert_eq!(
            state(Some(exited(Some(1), None)), false).done(true),
            out(Exit::TimedOut, false)
        );
        assert_eq!(
            state(Some(exited(None, Some(9))), false).done(true),
            out(Exit::TimedOut, false)
        );
    }

    #[test]
    fn join_draining_frees_a_thread_blocked_on_a_full_channel() {
        let (tx, rx) = sync_channel::<u32>(2);
        let h = std::thread::spawn(move || {
            for i in 0..1000 {
                if tx.send(i).is_err() {
                    return;
                }
            }
        });
        join_draining(h, &rx);
    }
}
