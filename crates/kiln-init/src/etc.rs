//! `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf` (spec §9.6 stage 5).

use std::net::Ipv4Addr;

use kiln_proto::Network;

pub fn hostname(name: &str) -> String {
    format!("{name}\n")
}

/// Docker's layout. Without a network the hostname resolves to 127.0.1.1 (on `lo`); with
/// IPv6, to the guest's IPv6 address as well.
pub fn hosts(name: &str, network: Option<&Network>) -> String {
    let addr = network.map_or(Ipv4Addr::new(127, 0, 1, 1), |n| n.address);
    let v6 = network
        .and_then(|n| n.ipv6.as_ref())
        .map_or(String::new(), |v6| format!("{}\t{name}\n", v6.address));
    format!(
        "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\nfe00::\tip6-localnet\n\
         ff00::\tip6-mcastprefix\nff02::1\tip6-allnodes\nff02::2\tip6-allrouters\n{addr}\t{name}\n{v6}"
    )
}

/// The guest's resolvers. Without any, `nameserver 127.0.0.1` (as Docker's
/// `--network none` still lists one): lookups fail at once with "connection
/// refused" instead of tools refusing to start without a resolver.
pub fn resolv_conf(network: Option<&Network>) -> String {
    match network {
        Some(n) if !n.dns.is_empty() => {
            let mut out: String = n.dns.iter().map(|d| format!("nameserver {d}\n")).collect();
            if !n.search.is_empty() {
                out += &format!("search {}\n", n.search.join(" "));
            }
            if !n.options.is_empty() {
                out += &format!("options {}\n", n.options.join(" "));
            }
            out
        }
        Some(_) => "# kiln: no DNS servers configured\nnameserver 127.0.0.1\n".to_string(),
        None => "# kiln: this VM has no network\nnameserver 127.0.0.1\n".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use kiln_proto::Ipv6Network;

    use super::*;

    #[test]
    fn writes_the_guest_address_and_resolvers() {
        let net = Network {
            address: Ipv4Addr::new(172, 30, 0, 2),
            prefix_len: 30,
            gateway: Ipv4Addr::new(172, 30, 0, 1),
            dns: vec![Ipv4Addr::new(172, 30, 0, 1)],
            search: Vec::new(),
            options: Vec::new(),
            ipv6: None,
        };
        assert!(hosts("box", Some(&net)).ends_with("\n172.30.0.2\tbox\n"));
        assert!(hosts("box", None).ends_with("\n127.0.1.1\tbox\n"));
        assert!(hosts("box", None).starts_with("127.0.0.1\tlocalhost\n"));
        assert_eq!(resolv_conf(Some(&net)), "nameserver 172.30.0.1\n");
        assert_eq!(
            resolv_conf(None),
            "# kiln: this VM has no network\nnameserver 127.0.0.1\n"
        );
        let pod = Network {
            dns: vec![Ipv4Addr::new(10, 96, 0, 10)],
            search: vec!["a.svc".into(), "b".into()],
            options: vec!["ndots:5".into()],
            ..net.clone()
        };
        assert_eq!(
            resolv_conf(Some(&pod)),
            "nameserver 10.96.0.10\nsearch a.svc b\noptions ndots:5\n"
        );
        let no_dns = Network {
            dns: vec![],
            ..net.clone()
        };
        assert_eq!(
            resolv_conf(Some(&no_dns)),
            "# kiln: no DNS servers configured\nnameserver 127.0.0.1\n"
        );
        assert_eq!(hostname("box"), "box\n");
        let v6 = Network {
            ipv6: Some(Ipv6Network {
                address: "fd76:6d6b:6974::2".parse().unwrap(),
                prefix_len: 64,
                gateway: "fd76:6d6b:6974::1".parse().unwrap(),
            }),
            ..net
        };
        assert!(
            hosts("box", Some(&v6)).ends_with("\n172.30.0.2\tbox\nfd76:6d6b:6974::2\tbox\n"),
            "{}",
            hosts("box", Some(&v6))
        );
    }
}
