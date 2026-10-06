//! `kiln run` (spec §9.7). On Linux it boots the image through `kiln-run`; elsewhere
//! it explains how to use the Lima VM (spec §10) and exits with 125.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use kiln_run::options::{DEFAULT_CPUS, DEFAULT_MEMORY_MIB, DEFAULT_STOP_TIMEOUT, parse_size};
use kiln_run::{RunOptions, VmmKind};

/// The exit code for errors before the guest runs, as `docker run` uses it.
pub const EXIT_ERROR: u8 = 125;

#[derive(clap::Args, Clone, Debug)]
pub struct RunArgs {
    /// The VMM: firecracker (the default) or cloud-hypervisor.
    #[arg(long, default_value = "firecracker", value_parser = |s: &str| s.parse::<VmmKind>())]
    vmm: VmmKind,
    #[arg(long, default_value_t = DEFAULT_CPUS)]
    cpus: u8,
    /// Guest memory in MiB.
    #[arg(long, default_value_t = DEFAULT_MEMORY_MIB, value_name = "MiB")]
    memory: u32,
    /// Keep stdin open and relay it to the guest.
    #[arg(short, long)]
    interactive: bool,
    /// Run the command on a terminal (with -i, kiln's terminal goes raw;
    /// Ctrl-] q stops the guest, Ctrl-] k kills it).
    #[arg(short, long)]
    tty: bool,
    /// The scratch disk's size (default 4G; a multiple of 4096, at least 64M).
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    disk: Option<u64>,
    /// KEY=VALUE, or KEY to pass kiln's own value (repeatable).
    #[arg(short, long = "env", value_name = "KEY[=VALUE]")]
    env: Vec<String>,
    /// Seconds between asking the main process to stop and killing it.
    #[arg(long, default_value_t = DEFAULT_STOP_TIMEOUT, value_name = "SECS")]
    stop_timeout: u32,
    /// Seconds from starting the VM to the command running.
    #[arg(long, default_value_t = 30, value_name = "SECS")]
    boot_timeout: u64,
    /// Boot this kernel (needs --allow-custom-kernel; kiln pins no kernel yet).
    #[arg(long, value_name = "PATH")]
    kernel: Option<PathBuf>,
    #[arg(long)]
    allow_custom_kernel: bool,
    /// Boot this static kiln-init (needs --allow-custom-init; kiln pins no kiln-init yet).
    #[arg(long, value_name = "PATH")]
    init: Option<PathBuf>,
    #[arg(long)]
    allow_custom_init: bool,
    /// The image: a name or digest in the store.
    image: String,
    /// The command, replacing the image's cmd (its entrypoint stays).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "CMD")]
    cmd: Vec<String>,
}

impl RunArgs {
    pub fn options(&self) -> Result<RunOptions> {
        let o = RunOptions {
            image: self.image.clone(),
            vmm: self.vmm,
            cpus: self.cpus,
            memory_mib: self.memory,
            interactive: self.interactive,
            tty: self.tty,
            disk: self.disk,
            env: self.env.clone(),
            cmd: self.cmd.clone(),
            stop_timeout: self.stop_timeout,
            boot_timeout: Duration::from_secs(self.boot_timeout),
            kernel: self.kernel.clone(),
            allow_custom_kernel: self.allow_custom_kernel,
            init: self.init.clone(),
            allow_custom_init: self.allow_custom_init,
        };
        o.check()?;
        Ok(o)
    }
}

/// Runs the image; returns the exit code.
#[cfg(target_os = "linux")]
pub fn run(open_store: impl FnOnce() -> Result<kiln_store::Store>, args: &RunArgs) -> Result<u8> {
    use kiln_proto::sanitize::clean_line;
    use kiln_run::{EXIT_KILLED, Run, Streams, install_signals, tty};

    let t0 = std::time::Instant::now();
    let opts = args.options()?;
    // Before anything slow: an interrupt during setup aborts it cleanly (137).
    let signals = install_signals(opts.tty)?;
    let store = open_store()?;
    let store = &store;
    let raw_tty = opts.tty && opts.interactive;
    if raw_tty && !tty::stdin_is_terminal() {
        anyhow::bail!("the input device is not a TTY (-t with -i needs a terminal on stdin)");
    }
    let streams = Streams {
        stdin: opts
            .interactive
            .then(|| Box::new(std::io::stdin()) as Box<dyn std::io::Read + Send>),
        stdout: Box::new(std::io::stdout()),
        stderr: Box::new(std::io::stderr()),
    };
    let started = Run::start(
        store,
        &opts,
        streams,
        &mut |line| eprintln!("kiln: {}", clean_line(line)),
        &|| signals.interrupted(),
    );
    let run = match started {
        Ok(run) => run,
        // A user's kill before the guest ran, as an interrupt before Hello is.
        Err(kiln_run::Error::Interrupted) => {
            eprintln!("kiln: interrupted before the guest started");
            return Ok(u8::try_from(EXIT_KILLED).unwrap_or(EXIT_ERROR));
        }
        Err(e) => return Err(e.into()),
    };
    let booting = t0.elapsed();
    signals.attach(run.handle());
    // A failure from here drops the run, and the session kills its VM.
    let raw = if raw_tty { Some(tty::RawMode::enter()?) } else { None };
    let report = run.wait();
    drop(raw);
    drop(signals);
    let o = &report.outcome;
    for w in &o.warnings {
        eprintln!("kiln: warning: {}", clean_line(w));
    }
    if let Some(v) = &o.violation {
        eprintln!("kiln: the VM was killed: {}", clean_line(v));
    } else if let Some(f) = &o.init_failed {
        eprintln!("kiln: {}", f.describe());
    } else if o.exited.is_none() && !o.killed {
        eprintln!(
            "kiln: the VM ended without reporting the command's exit ({:?})",
            o.end.reason
        );
    }
    if !report.console_tail.is_empty() {
        eprintln!("kiln: the end of the guest's console:");
        for l in &report.console_tail {
            eprintln!("  | {l}");
        }
    }
    if std::env::var_os("KILN_TIMINGS").is_some_and(|v| v == "1") {
        let ms = |d: Option<Duration>| d.map_or("-".to_string(), |d| d.as_millis().to_string());
        eprintln!(
            "kiln: timings (ms): setup and VMM create {}; VM start to Hello {}, to Running {}, to the VMM's exit {}, \
             to the output's end {}; total {}",
            booting.as_millis(),
            ms(o.hello_after),
            ms(o.running_after),
            ms(o.ended_after),
            o.drained_after.as_millis(),
            t0.elapsed().as_millis()
        );
    }
    if let Some(dir) = &report.kept {
        eprintln!("kiln: run directory kept: {}", dir.display());
    }
    Ok(u8::try_from(o.exit_code).unwrap_or(EXIT_ERROR))
}

#[cfg(not(target_os = "linux"))]
pub fn run(_open_store: impl FnOnce() -> Result<kiln_store::Store>, args: &RunArgs) -> Result<u8> {
    args.options()?;
    eprintln!("{}", kiln_run::LIMA_INSTRUCTIONS);
    Ok(EXIT_ERROR)
}
