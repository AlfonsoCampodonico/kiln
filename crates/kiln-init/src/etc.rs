//! `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf` (spec §9.6 stage 5).

use std::net::Ipv4Addr;

use kiln_proto::Network;

pub fn hostname(name: &str) -> String {
    format!("{name}\n")
}

/// Docker's layout. Without a network the hostname resolves to 127.0.1.1 (on `lo`).
pub fn hosts(name: &str, network: Option<&Network>) -> String {
    let addr = network.map_or(Ipv4Addr::new(127, 0, 1, 1), |n| n.address);
    format!(
        "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\nfe00::\tip6-localnet\n\
         ff00::\tip6-mcastprefix\nff02::1\tip6-allnodes\nff02::2\tip6-allrouters\n{addr}\t{name}\n"
    )
}

/// The guest's resolvers. Without any, `nameserver 127.0.0.1` (as Docker's
/// `--network none` still lists one): lookups fail at once with "connection
/// refused" instead of tools refusing to start without a resolver.
pub fn resolv_conf(network: Option<&Network>) -> String {
    match network {
        Some(n) if !n.dns.is_empty() => n.dns.iter().map(|d| format!("nameserver {d}\n")).collect(),
        Some(_) => "# kiln: no DNS servers configured\nnameserver 127.0.0.1\n".to_string(),
        None => "# kiln: this VM has no network\nnameserver 127.0.0.1\n".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_guest_address_and_resolvers() {
        let net = Network {
            address: Ipv4Addr::new(172, 30, 0, 2),
            prefix_len: 30,
            gateway: Ipv4Addr::new(172, 30, 0, 1),
            dns: vec![Ipv4Addr::new(172, 30, 0, 1)],
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
        let no_dns = Network { dns: vec![], ..net };
        assert_eq!(
            resolv_conf(Some(&no_dns)),
            "# kiln: no DNS servers configured\nnameserver 127.0.0.1\n"
        );
        assert_eq!(hostname("box"), "box\n");
    }
}
