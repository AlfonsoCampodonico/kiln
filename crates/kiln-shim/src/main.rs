//! `containerd-shim-kiln-v1`. containerd runs it as a shim (`start`, `delete`, and the
//! server), and with `-info` and `-v`; the shim re-runs itself as `pause` (a pod's
//! sandbox), `vm` (a container's VM worker) and `cleanup` (a pod disk's removal, as its
//! owner) on Linux.

use std::io::Write;

use containerd_shim::protos::protobuf::Message;
use kiln_shim::RUNTIME_ID;
use kiln_shim::service::{KilnShim, runtime_info, shim_config};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    #[cfg(target_os = "linux")]
    match args.get(1).map(String::as_str) {
        Some("vm") => kiln_shim::linux::worker::main(),
        Some("pause") => kiln_shim::linux::pause::main(args.get(2).map(std::path::Path::new)),
        Some("cleanup") => kiln_shim::linux::cleanup(args.get(2).map(std::path::Path::new)),
        _ => {}
    }
    // containerd's flags are Go-style: one dash or two.
    let flag = |name: &str| {
        args.iter()
            .skip(1)
            .any(|a| a.trim_start_matches('-') == name && a.starts_with('-'))
    };
    if flag("info") {
        let bytes = runtime_info().write_to_bytes().expect("RuntimeInfo encodes");
        std::io::stdout().write_all(&bytes).expect("stdout");
        return;
    }
    if flag("v") || flag("version") {
        println!(
            "containerd-shim-kiln-v1:\n  Version: {}\n  Runtime: {RUNTIME_ID}",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    containerd_shim::run::<KilnShim>(RUNTIME_ID, Some(shim_config()));
}
