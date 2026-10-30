//! The Linux backend (design §8): the pod sandbox's `pause` process, the pod's network
//! attachment, and the VM workers.

pub mod attach;
mod backend;
pub mod fdpass;
pub mod pause;
pub mod plan;
pub mod worker;

pub use backend::LinuxBackend;

/// Where the shim keeps its state (root, 0700).
pub const STATE: &str = "/run/kiln-shim";
/// The environment variable naming a worker's channel fd.
pub const FD_ENV: &str = "KILN_SHIM_FD";
/// The environment variable naming a worker's fd channel (exec FIFOs).
pub const FDS_ENV: &str = "KILN_SHIM_FDS";

/// `cleanup <dir>`: removes a pod disk's directory. The shim runs it as the directory's
/// owner (the kiln user), never as root.
pub fn cleanup(dir: Option<&std::path::Path>) -> ! {
    let code = match dir {
        Some(d) if !rustix::process::geteuid().is_root() => match std::fs::remove_dir_all(d) {
            Ok(()) => 0,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => {
                eprintln!("kiln-shim cleanup: {}: {e}", d.display());
                1
            }
        },
        _ => 2,
    };
    std::process::exit(code)
}
