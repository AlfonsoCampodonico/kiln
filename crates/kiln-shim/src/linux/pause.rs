//! The pod sandbox's process (design §4): it joins the pod netns, so that
//! `/proc/<pid>/ns/net` names it for the pod's containers as a pause container's does,
//! and waits for a signal. It dies with the shim.

use std::fs::File;
use std::os::fd::AsFd;
use std::path::Path;

use rustix::process::{Signal, set_parent_process_death_signal};
use rustix::thread::{LinkNameSpaceType, move_into_link_name_space};

pub fn main(netns: Option<&Path>) -> ! {
    let _ = set_parent_process_death_signal(Some(Signal::KILL));
    if let Some(path) = netns {
        let joined = File::open(path).map_err(|e| e.to_string()).and_then(|f| {
            move_into_link_name_space(f.as_fd(), Some(LinkNameSpaceType::Network)).map_err(|e| e.to_string())
        });
        if let Err(e) = joined {
            eprintln!("kiln-shim pause: joining {}: {e}", path.display());
            std::process::exit(1);
        }
    }
    loop {
        std::thread::park();
    }
}
