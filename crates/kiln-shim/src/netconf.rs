//! The pod's network as kiln sees it (design §6): the CNI interface in the pod netns
//! (from `ip -j addr` and `ip -j route`), the pod's resolv.conf, the commands that put
//! the guest on the pod IP through a tap, and the guest's `Config.network`.
//!
//! Guest traffic is redirected by tc before the pod netns's IP stack sees it, so the
//! fixed deny list (cloud metadata and the node's own addresses, D4) is tc filters on
//! the tap's ingress, not nftables. They are stateless: towards the node only replies
//! pass (TCP with ACK set, ICMP echo replies and unreachables), so the guest still
//! answers kubelet's probes, which come from a node address, but opens nothing there,
//! over any protocol.

use std::net::Ipv4Addr;

use kiln_proto::Network;
use kiln_run::options::Cidr;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The CNI interface the guest takes over: its address, MAC, MTU and gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodNet {
    pub ifname: String,
    pub address: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
    pub mac: String,
    pub mtu: u32,
}

#[derive(Deserialize)]
struct Link {
    ifname: String,
    #[serde(default)]
    link_type: String,
    #[serde(default)]
    address: String,
    #[serde(default)]
    mtu: u32,
    #[serde(default)]
    addr_info: Vec<Addr>,
}

#[derive(Deserialize)]
struct Addr {
    family: String,
    /// IPv4 or IPv6 (for `inet6`), so a string.
    local: Option<String>,
    #[serde(default)]
    prefixlen: u8,
}

impl Addr {
    fn v4(&self) -> Option<Ipv4Addr> {
        (self.family == "inet").then(|| self.local.as_deref()?.parse().ok())?
    }
}

#[derive(Deserialize)]
struct Route {
    dst: String,
    gateway: Option<Ipv4Addr>,
    dev: Option<String>,
}

fn links(json: &str) -> Result<Vec<Link>> {
    serde_json::from_str(json).map_err(|e| Error::Internal(format!("parsing `ip -j addr`: {e}")))
}

fn ipv4(l: &Link) -> Option<(Ipv4Addr, u8)> {
    l.addr_info.iter().find_map(|a| Some((a.v4()?, a.prefixlen)))
}

/// The pod netns's one IPv4 interface, or `None` when it has none (no network).
pub fn pod_net(addr_json: &str, route_json: &str) -> Result<Option<PodNet>> {
    let found: Vec<(Link, (Ipv4Addr, u8))> = links(addr_json)?
        .into_iter()
        .filter(|l| l.link_type != "loopback")
        .filter_map(|l| ipv4(&l).map(|a| (l, a)))
        .collect();
    let (link, (address, prefix)) = match found.len() {
        0 => return Ok(None),
        1 => found.into_iter().next().expect("one"),
        n => {
            return Err(Error::invalid(format!(
                "the pod network has {n} IPv4 interfaces; kiln attaches a VM to exactly one"
            )));
        }
    };
    let routes: Vec<Route> =
        serde_json::from_str(route_json).map_err(|e| Error::Internal(format!("parsing `ip -j route`: {e}")))?;
    let gateway = routes
        .iter()
        .find(|r| r.dst == "default" && r.dev.as_deref().is_none_or(|d| d == link.ifname))
        .and_then(|r| r.gateway)
        .ok_or_else(|| Error::invalid(format!("the pod network has no default route via {}", link.ifname)))?;
    Ok(Some(PodNet {
        ifname: link.ifname,
        address,
        prefix,
        gateway,
        mac: link.address,
        mtu: if link.mtu == 0 { 1500 } else { link.mtu },
    }))
}

/// The node's own IPv4 addresses (from `ip -j addr` in the host's namespace), loopback excepted.
pub fn host_addresses(addr_json: &str) -> Result<Vec<Ipv4Addr>> {
    Ok(links(addr_json)?
        .iter()
        .flat_map(|l| l.addr_info.iter())
        .filter_map(Addr::v4)
        .filter(|a| !a.is_loopback())
        .collect())
}

/// What a resolv.conf says (IPv4 servers only: the guest is IPv4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolv {
    pub nameservers: Vec<Ipv4Addr>,
    pub search: Vec<String>,
    pub options: Vec<String>,
}

pub fn resolv(text: &str) -> Resolv {
    let mut r = Resolv::default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("nameserver") => r
                .nameservers
                .extend(words.next().and_then(|a| a.parse::<Ipv4Addr>().ok())),
            // As the resolver does, the last `search` line wins.
            Some("search") => r.search = words.map(String::from).collect(),
            Some("options") => r.options.extend(words.map(String::from)),
            _ => {}
        }
    }
    r
}

/// The guest's `eth0`: the pod's address and gateway, and the pod's resolvers within
/// what kiln-proto carries (3 servers, 6 search domains).
pub fn guest_network(net: &PodNet, r: &Resolv) -> Network {
    Network {
        address: net.address,
        prefix_len: net.prefix,
        gateway: net.gateway,
        dns: r.nameservers.iter().copied().take(3).collect(),
        search: r.search.iter().take(6).cloned().collect(),
        options: r.options.clone(),
        // The tap carries IPv4 only (its ingress drops IPv6, design §6.1).
        ipv6: None,
    }
}

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

/// `ip` commands for the tap: owned by the kiln user, with the pod interface's MTU.
pub fn tap_commands(net: &PodNet, tap: &str, uid: u32, gid: u32) -> Vec<Vec<String>> {
    let (uid, gid, mtu) = (uid.to_string(), gid.to_string(), net.mtu.to_string());
    vec![
        args(&["tuntap", "add", tap, "mode", "tap", "user", &uid, "group", &gid]),
        args(&["link", "set", tap, "mtu", &mtu, "up"]),
    ]
}

/// `tc` commands, in order: everything the CNI delivers to the pod goes to the tap; from
/// the tap, the `allow` exceptions, the deny list, then only the pod's own IPv4 and ARP to
/// the pod interface, and nothing else (IPv6, spoofed sources). Every redirect of IPv4
/// requires the pod's own source address.
pub fn tc_commands(net: &PodNet, tap: &str, allow: &[Cidr], node: &[Ipv4Addr]) -> Vec<Vec<String>> {
    let pod = net.ifname.as_str();
    let mut out = vec![
        args(&["qdisc", "replace", "dev", pod, "clsact"]),
        args(&[
            "filter", "add", "dev", pod, "ingress", "pref", "1", "protocol", "all", "matchall", "action", "mirred",
            "egress", "redirect", "dev", tap,
        ]),
        args(&["qdisc", "replace", "dev", tap, "clsact"]),
    ];
    let mut pref = 0u32;
    let mut filter = |rest: &[&str]| {
        pref += 1;
        let p = pref.to_string();
        let mut f = args(&["filter", "add", "dev", tap, "ingress", "pref", &p]);
        f.extend(args(rest));
        out.push(f);
    };
    let to_pod = ["action", "mirred", "egress", "redirect", "dev", pod];
    let own = format!("{}/32", net.address);
    let from_pod = ["protocol", "ip", "flower", "src_ip", own.as_str()];
    for c in allow {
        let cidr = format!("{}/{}", c.addr, c.prefix);
        filter(&[&from_pod[..], &["dst_ip", &cidr], &to_pod].concat());
    }
    filter(&["protocol", "ip", "flower", "dst_ip", "169.254.0.0/16", "action", "drop"]);
    for a in node {
        let a = format!("{a}/32");
        // Replies only: to connections the node opened, and ICMP answers.
        for reply in [
            &["ip_proto", "tcp", "tcp_flags", "0x10/0x10"][..],
            &["ip_proto", "icmp", "type", "0"],
            &["ip_proto", "icmp", "type", "3"],
        ] {
            filter(&[&from_pod[..], &["dst_ip", &a], reply, &to_pod].concat());
        }
        filter(&["protocol", "ip", "flower", "dst_ip", &a, "action", "drop"]);
    }
    filter(&[&from_pod[..], &to_pod].concat());
    filter(&[&["protocol", "arp", "flower", "arp_sip", own.as_str()][..], &to_pod].concat());
    filter(&["protocol", "all", "matchall", "action", "drop"]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: &str = r#"[
      {"ifindex":1,"ifname":"lo","flags":["LOOPBACK","UP"],"mtu":65536,"link_type":"loopback","address":"00:00:00:00:00:00",
       "addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8,"scope":"host"},{"family":"inet6","local":"::1","prefixlen":128}]},
      {"ifindex":2,"link_index":7,"ifname":"eth0","flags":["BROADCAST","UP"],"mtu":1450,"link_type":"ether","address":"0a:58:0a:58:00:05",
       "link_netnsid":0,"addr_info":[{"family":"inet","local":"10.88.0.5","prefixlen":16,"broadcast":"10.88.255.255","scope":"global"},
       {"family":"inet6","local":"fe80::858:aff:fe58:5","prefixlen":64,"scope":"link"}]}
    ]"#;
    const ROUTE: &str = r#"[{"dst":"default","gateway":"10.88.0.1","dev":"eth0","flags":[]}]"#;

    fn pod() -> PodNet {
        pod_net(ADDR, ROUTE).unwrap().unwrap()
    }

    #[test]
    fn parses_ip_json() {
        assert_eq!(
            pod(),
            PodNet {
                ifname: "eth0".into(),
                address: Ipv4Addr::new(10, 88, 0, 5),
                prefix: 16,
                gateway: Ipv4Addr::new(10, 88, 0, 1),
                mac: "0a:58:0a:58:00:05".into(),
                mtu: 1450,
            }
        );
        let lo_only = r#"[{"ifname":"lo","link_type":"loopback","addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8}]}]"#;
        assert_eq!(pod_net(lo_only, "[]").unwrap(), None);
        let two = ADDR.replace(
            "\n    ]",
            r#",{"ifname":"net1","link_type":"ether","address":"02:00:00:00:00:01","addr_info":[{"family":"inet","local":"192.168.9.2","prefixlen":24}]}]"#,
        );
        assert!(
            pod_net(&two, ROUTE)
                .unwrap_err()
                .to_string()
                .contains("2 IPv4 interfaces")
        );
        assert!(
            pod_net(ADDR, "[]")
                .unwrap_err()
                .to_string()
                .contains("no default route")
        );
    }

    /// Calico: a /32 pod address and a link-local gateway on-link.
    #[test]
    fn calico_shape() {
        let addr = ADDR.replace(
            r#""local":"10.88.0.5","prefixlen":16"#,
            r#""local":"10.244.1.5","prefixlen":32"#,
        );
        let route = r#"[{"dst":"default","gateway":"169.254.1.1","dev":"eth0","flags":["onlink"]}]"#;
        let n = pod_net(&addr, route).unwrap().unwrap();
        assert_eq!(
            (n.address, n.prefix, n.gateway),
            (Ipv4Addr::new(10, 244, 1, 5), 32, Ipv4Addr::new(169, 254, 1, 1))
        );
        let net = guest_network(&n, &Resolv::default());
        assert!(!net.gateway_in_prefix());
    }

    #[test]
    fn node_addresses() {
        assert_eq!(host_addresses(ADDR).unwrap(), [Ipv4Addr::new(10, 88, 0, 5)]);
    }

    #[test]
    fn resolv_conf() {
        let r = resolv(
            "nameserver 10.96.0.10\nnameserver fd00::10\nsearch ns.svc.cluster.local svc.cluster.local cluster.local\n\
             options ndots:5\n# a comment\n",
        );
        assert_eq!(r.nameservers, [Ipv4Addr::new(10, 96, 0, 10)]);
        assert_eq!(r.search, ["ns.svc.cluster.local", "svc.cluster.local", "cluster.local"]);
        assert_eq!(r.options, ["ndots:5"]);
        let net = guest_network(&pod(), &r);
        assert_eq!(net.dns, [Ipv4Addr::new(10, 96, 0, 10)]);
        assert_eq!(net.search.len(), 3);
        assert_eq!(net.options, ["ndots:5"]);
    }

    #[test]
    fn tap_plan() {
        assert_eq!(
            tap_commands(&pod(), "kiln0", 999, 998),
            [
                args(&["tuntap", "add", "kiln0", "mode", "tap", "user", "999", "group", "998"]),
                args(&["link", "set", "kiln0", "mtu", "1450", "up"]),
            ]
        );
    }

    #[test]
    fn tc_plan() {
        let allow = ["169.254.20.10/32".parse::<Cidr>().unwrap()];
        let node = [Ipv4Addr::new(192, 168, 5, 15)];
        let plan = tc_commands(&pod(), "kiln0", &allow, &node);
        let text: Vec<String> = plan.iter().map(|c| c.join(" ")).collect();
        assert_eq!(
            text,
            [
                "qdisc replace dev eth0 clsact",
                "filter add dev eth0 ingress pref 1 protocol all matchall action mirred egress redirect dev kiln0",
                "qdisc replace dev kiln0 clsact",
                "filter add dev kiln0 ingress pref 1 protocol ip flower src_ip 10.88.0.5/32 dst_ip 169.254.20.10/32 action mirred egress redirect dev eth0",
                "filter add dev kiln0 ingress pref 2 protocol ip flower dst_ip 169.254.0.0/16 action drop",
                "filter add dev kiln0 ingress pref 3 protocol ip flower src_ip 10.88.0.5/32 dst_ip 192.168.5.15/32 ip_proto tcp tcp_flags 0x10/0x10 action mirred egress redirect dev eth0",
                "filter add dev kiln0 ingress pref 4 protocol ip flower src_ip 10.88.0.5/32 dst_ip 192.168.5.15/32 ip_proto icmp type 0 action mirred egress redirect dev eth0",
                "filter add dev kiln0 ingress pref 5 protocol ip flower src_ip 10.88.0.5/32 dst_ip 192.168.5.15/32 ip_proto icmp type 3 action mirred egress redirect dev eth0",
                "filter add dev kiln0 ingress pref 6 protocol ip flower dst_ip 192.168.5.15/32 action drop",
                "filter add dev kiln0 ingress pref 7 protocol ip flower src_ip 10.88.0.5/32 action mirred egress redirect dev eth0",
                "filter add dev kiln0 ingress pref 8 protocol arp flower arp_sip 10.88.0.5/32 action mirred egress redirect dev eth0",
                "filter add dev kiln0 ingress pref 9 protocol all matchall action drop",
            ]
        );
    }
}
