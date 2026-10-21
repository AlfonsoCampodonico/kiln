//! Attaching a pod netns for its VMs (design §6.1), as root: the tap `kiln0` owned by
//! the kiln user, and the tc redirects and deny list between it and the CNI interface.
//! Commands run from a thread that has joined the pod netns, so the `ip` and `tc` it
//! forks run there too.

use std::fs::File;
use std::net::Ipv4Addr;
use std::os::fd::AsFd;
use std::path::Path;
use std::process::{Command, Stdio};

use kiln_run::options::Cidr;
use rustix::thread::{LinkNameSpaceType, move_into_link_name_space};

use crate::TAP;
use crate::error::{Error, Result};
use crate::netconf::{self, PodNet};
use crate::passwd::Account;

/// Runs `program args`; its stdout.
fn run(program: &str, args: &[String]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::Internal(format!("starting {program}: {e}")))?;
    if !out.status.success() {
        return Err(Error::Internal(format!(
            "{program} {}: {} ({})",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim(),
            out.status
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|s| s.to_string()).collect()
}

/// The node's own IPv4 addresses, from the shim's (the host's) namespace.
pub fn node_addresses() -> Result<Vec<Ipv4Addr>> {
    netconf::host_addresses(&run("ip", &words(&["-j", "addr"]))?)
}

/// Sets up the pod netns at `netns` for the guest; the CNI interface it took over, or
/// `None` when the pod has no IPv4 network. A netns that already has the tap was set
/// up before (an earlier container of the pod) and is only read.
pub fn attach(netns: &Path, account: &Account, allow: &[Cidr], node: &[Ipv4Addr]) -> Result<Option<PodNet>> {
    let ns =
        File::open(netns).map_err(|e| Error::Internal(format!("opening the pod netns {}: {e}", netns.display())))?;
    std::thread::scope(|s| {
        s.spawn(|| {
            move_into_link_name_space(ns.as_fd(), Some(LinkNameSpaceType::Network))
                .map_err(|e| Error::Internal(format!("joining the pod netns {}: {e}", netns.display())))?;
            let addr = run("ip", &words(&["-j", "addr"]))?;
            let route = run("ip", &words(&["-j", "-4", "route", "show", "default"]))?;
            let Some(pod) = netconf::pod_net(&addr, &route)? else {
                return Ok(None);
            };
            if run("ip", &words(&["link", "show", TAP])).is_ok() {
                return Ok(Some(pod));
            }
            let setup = || -> Result<()> {
                for c in netconf::tap_commands(&pod, TAP, account.uid, account.gid) {
                    run("ip", &c)?;
                }
                for c in netconf::tc_commands(&pod, TAP, allow, node) {
                    run("tc", &c)?;
                }
                Ok(())
            };
            if let Err(e) = setup() {
                // Undone, so a later container finds no tap and sets it all up again,
                // instead of taking a half-made one for done: the tap goes with its filters,
                // and the redirect on the pod interface is the shim's own (pref 1).
                let _ = run("ip", &words(&["link", "del", TAP]));
                let _ = run(
                    "tc",
                    &words(&["filter", "del", "dev", &pod.ifname, "ingress", "pref", "1"]),
                );
                return Err(e);
            }
            Ok(Some(pod))
        })
        .join()
        .unwrap_or_else(|_| Err(Error::Internal("the netns thread panicked".into())))
    })
}
