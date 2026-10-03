//! Destination checks for T2: which hosts a registry may send kiln to.
//!
//! Every connection the client makes goes through [`CheckedResolver`], which refuses
//! a host when any of its addresses is loopback, link-local, private, CGNAT,
//! unspecified or reserved, unless the configured registry's own addresses are in
//! that same class. The connection then uses exactly the addresses that were
//! checked, so DNS rebinding cannot slip past. Hosts given as IP literals never
//! reach a resolver, so [`Policy::check_url`] checks them before the request.

use std::collections::BTreeSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

use crate::error::{RegistryError, redact};

/// The address classes T2 distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AddrClass {
    Public,
    Loopback,
    /// 169.254.0.0/16 and fe80::/10, including cloud metadata endpoints.
    LinkLocal,
    /// RFC 1918 and fc00::/7.
    Private,
    /// 100.64.0.0/10.
    Cgnat,
    /// 0.0.0.0/8 and `::`.
    Unspecified,
    /// Multicast, broadcast and 240.0.0.0/4.
    Reserved,
}

impl AddrClass {
    pub fn of(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => Self::of_v4(v4),
            IpAddr::V6(v6) => Self::of_v6(v6),
        }
    }

    fn of_v4(ip: Ipv4Addr) -> Self {
        let [a, b, ..] = ip.octets();
        if ip.is_loopback() {
            Self::Loopback
        } else if ip.is_link_local() {
            Self::LinkLocal
        } else if ip.is_private() {
            Self::Private
        } else if a == 100 && (64..128).contains(&b) {
            Self::Cgnat
        } else if a == 0 {
            Self::Unspecified
        } else if ip.is_multicast() || a >= 240 {
            Self::Reserved
        } else {
            Self::Public
        }
    }

    fn of_v6(ip: Ipv6Addr) -> Self {
        // IPv4-mapped (::ffff:a.b.c.d) and NAT64 (64:ff9b::a.b.c.d) addresses reach
        // the embedded IPv4 address.
        if let Some(v4) = ip.to_ipv4_mapped() {
            return Self::of_v4(v4);
        }
        let s = ip.segments();
        if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
            let [.., hi, lo] = s;
            return Self::of_v4(Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo)));
        }
        if ip.is_loopback() {
            Self::Loopback
        } else if ip.is_unspecified() {
            Self::Unspecified
        } else if s[0] & 0xffc0 == 0xfe80 {
            Self::LinkLocal
        } else if s[0] & 0xfe00 == 0xfc00 {
            Self::Private
        } else if ip.is_multicast() {
            Self::Reserved
        } else {
            Self::Public
        }
    }
}

impl fmt::Display for AddrClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Public => "public",
            Self::Loopback => "loopback",
            Self::LinkLocal => "link-local",
            Self::Private => "private",
            Self::Cgnat => "CGNAT",
            Self::Unspecified => "unspecified",
            Self::Reserved => "reserved",
        })
    }
}

/// A destination refused by the policy (travels through reqwest's error chain
/// when the resolver refuses a name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

/// The destinations one registry client may contact.
#[derive(Debug, Clone)]
pub(crate) struct Policy {
    /// Non-public classes allowed because the registry itself is in them.
    allowed: BTreeSet<AddrClass>,
    /// The registry is spoken to over plain http (it is on loopback).
    plain_http: bool,
}

impl Policy {
    pub(crate) fn new(registry_addrs: &[IpAddr], plain_http: bool) -> Self {
        Self {
            allowed: registry_addrs.iter().map(|&ip| AddrClass::of(ip)).collect(),
            plain_http,
        }
    }

    /// Refuses `host` if any of its addresses is in a class the registry is not in.
    pub(crate) fn check_addrs(&self, host: &str, addrs: &[IpAddr]) -> Result<(), Refusal> {
        if addrs.is_empty() {
            return Err(Refusal(format!("{host} has no addresses")));
        }
        for &ip in addrs {
            let class = AddrClass::of(ip);
            if class != AddrClass::Public && !self.allowed.contains(&class) {
                let ip = ip.to_string();
                return Err(Refusal(if ip == host {
                    format!("{ip} is a {class} address")
                } else {
                    format!("{host} resolves to {ip}, a {class} address")
                }));
            }
        }
        Ok(())
    }

    /// Checks a URL the registry handed us (a redirect, token realm or upload
    /// location) before it is requested: https only (plain http only to loopback,
    /// and only when the registry itself is plain-http loopback), no credentials in
    /// the URL, and IP-literal hosts in an allowed class. Names are checked when
    /// they are resolved.
    pub(crate) fn check_url(&self, url: &Url) -> Result<(), RegistryError> {
        let refuse = |reason: String| RegistryError::Refused {
            url: redact(url),
            reason,
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err(refuse("URL contains credentials".into()));
        }
        match url.scheme() {
            "https" => {}
            "http" if self.plain_http && is_loopback_host(url) => {}
            "http" => return Err(refuse("plain http is allowed only between loopback addresses".into())),
            other => return Err(refuse(format!("unsupported scheme {other:?}"))),
        }
        let ip = match url.host() {
            Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
            Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
            Some(Host::Domain(_)) => return Ok(()),
            None => return Err(refuse("URL has no host".into())),
        };
        self.check_addrs(&ip.to_string(), &[ip]).map_err(|r| refuse(r.0))
    }
}

/// `localhost` or a loopback IP literal (no DNS involved).
pub(crate) fn is_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// System name resolution (blocking).
pub(crate) fn lookup(host: &str) -> std::io::Result<Vec<IpAddr>> {
    Ok((host, 0).to_socket_addrs()?.map(|a| a.ip()).collect())
}

/// Resolves `host` and returns its addresses only if all of them pass the policy.
pub(crate) fn resolve_checked(
    policy: &Policy,
    host: &str,
    lookup: impl FnOnce(&str) -> std::io::Result<Vec<IpAddr>>,
) -> Result<Vec<SocketAddr>, Box<dyn std::error::Error + Send + Sync>> {
    let addrs = lookup(host)?;
    policy.check_addrs(host, &addrs)?;
    Ok(addrs.into_iter().map(|ip| SocketAddr::new(ip, 0)).collect())
}

/// reqwest's DNS hook: every connection is pinned to checked addresses.
pub(crate) struct CheckedResolver {
    pub(crate) policy: Arc<Policy>,
}

impl Resolve for CheckedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = self.policy.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = tokio::task::spawn_blocking(move || resolve_checked(&policy, &host, lookup)).await??;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn classifies_addresses() {
        for (addr, class) in [
            ("8.8.8.8", AddrClass::Public),
            ("127.0.0.1", AddrClass::Loopback),
            ("127.255.0.9", AddrClass::Loopback),
            ("169.254.169.254", AddrClass::LinkLocal),
            ("10.1.2.3", AddrClass::Private),
            ("172.16.0.1", AddrClass::Private),
            ("172.32.0.1", AddrClass::Public),
            ("192.168.1.1", AddrClass::Private),
            ("100.64.0.1", AddrClass::Cgnat),
            ("100.127.255.255", AddrClass::Cgnat),
            ("100.128.0.1", AddrClass::Public),
            ("0.0.0.0", AddrClass::Unspecified),
            ("0.1.2.3", AddrClass::Unspecified),
            ("224.0.0.1", AddrClass::Reserved),
            ("255.255.255.255", AddrClass::Reserved),
            ("::1", AddrClass::Loopback),
            ("::", AddrClass::Unspecified),
            ("fe80::1", AddrClass::LinkLocal),
            ("fd00:ec2::254", AddrClass::Private),
            ("fc00::1", AddrClass::Private),
            ("ff02::1", AddrClass::Reserved),
            ("2606:4700::1111", AddrClass::Public),
            ("::ffff:169.254.169.254", AddrClass::LinkLocal),
            ("::ffff:10.0.0.1", AddrClass::Private),
            ("64:ff9b::a9fe:a9fe", AddrClass::LinkLocal),
            ("64:ff9b::808:808", AddrClass::Public),
        ] {
            assert_eq!(AddrClass::of(ip(addr)), class, "{addr}");
        }
    }

    #[test]
    fn a_registry_may_reach_only_its_own_class() {
        let loopback = Policy::new(&[ip("127.0.0.1")], true);
        assert!(loopback.check_addrs("h", &[ip("127.0.0.2"), ip("::1")]).is_ok());
        assert!(loopback.check_addrs("h", &[ip("1.1.1.1")]).is_ok());
        for bad in ["169.254.169.254", "10.0.0.1", "100.64.0.1", "0.0.0.0", "fe80::1"] {
            assert!(loopback.check_addrs("h", &[ip(bad)]).is_err(), "{bad}");
        }
        let public = Policy::new(&[ip("104.16.0.1")], false);
        assert!(public.check_addrs("h", &[ip("1.1.1.1")]).is_ok());
        // One bad address refuses the whole host.
        let err = public
            .check_addrs("cdn", &[ip("1.1.1.1"), ip("127.0.0.1")])
            .unwrap_err();
        assert_eq!(err.0, "cdn resolves to 127.0.0.1, a loopback address");
        assert!(public.check_addrs("h", &[]).is_err());
        let private = Policy::new(&[ip("10.0.0.5")], false);
        assert!(private.check_addrs("h", &[ip("10.9.9.9")]).is_ok());
        assert!(private.check_addrs("h", &[ip("169.254.169.254")]).is_err());
    }

    #[test]
    fn urls_need_https_except_between_loopback_hosts() {
        let url = |s: &str| Url::parse(s).unwrap();
        let loopback_http = Policy::new(&[ip("127.0.0.1")], true);
        assert!(loopback_http.check_url(&url("http://127.0.0.1:9/x")).is_ok());
        assert!(loopback_http.check_url(&url("http://localhost:9/x")).is_ok());
        assert!(loopback_http.check_url(&url("http://[::1]:9/x")).is_ok());
        assert!(loopback_http.check_url(&url("https://example.com/x")).is_ok());
        for bad in [
            "http://example.com/x",
            "http://169.254.169.254/latest/meta-data",
            "https://169.254.169.254/latest/meta-data",
            "https://10.0.0.1/x",
            "https://[::ffff:a9fe:a9fe]/x",
            "https://user:pw@example.com/x",
            "ftp://example.com/x",
            "file:///etc/passwd",
        ] {
            assert!(loopback_http.check_url(&url(bad)).is_err(), "{bad}");
        }
        let public = Policy::new(&[ip("104.16.0.1")], false);
        assert!(public.check_url(&url("http://127.0.0.1/x")).is_err());
        assert!(public.check_url(&url("https://127.0.0.1/x")).is_err());
        let err = public
            .check_url(&url("https://10.0.0.1/x?sig=secret"))
            .unwrap_err()
            .to_string();
        assert!(!err.contains("secret"), "{err}");
    }

    #[test]
    fn resolved_names_are_checked_and_pinned() {
        let public = Policy::new(&[ip("104.16.0.1")], false);
        let rebinding = |_: &str| Ok(vec![ip("1.2.3.4"), ip("169.254.169.254")]);
        let err = resolve_checked(&public, "evil.example", rebinding).unwrap_err();
        assert!(err.downcast_ref::<Refusal>().is_some(), "{err}");
        let ok = resolve_checked(&public, "cdn.example", |_| Ok(vec![ip("1.2.3.4")])).unwrap();
        assert_eq!(ok, vec![SocketAddr::new(ip("1.2.3.4"), 0)]);
        let missing = |_: &str| Err(std::io::Error::other("no such host"));
        assert!(resolve_checked(&public, "nx.example", missing).is_err());
    }
}
