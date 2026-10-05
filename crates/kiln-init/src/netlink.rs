//! rtnetlink requests for `lo` and `eth0` (spec §9.6 stage 5): the guest has no
//! `ip` binary, so init builds the three messages it needs itself. All values
//! are native-endian, as netlink requires.

use std::net::Ipv4Addr;

const NLMSG_ERROR: u16 = 2;
const RTM_NEWLINK: u16 = 16;
const RTM_NEWADDR: u16 = 20;
const RTM_NEWROUTE: u16 = 24;

const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;

const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;
const IFF_UP: u32 = 0x1;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RT_TABLE_MAIN: u8 = 254;
const RTPROT_BOOT: u8 = 3;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RTN_UNICAST: u8 = 1;

const HEADER: usize = 16;

/// Starts a message: `nlmsghdr` with the length patched in by [`finish`].
fn header(kind: u16, flags: u16, seq: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(64);
    m.extend_from_slice(&0u32.to_ne_bytes());
    m.extend_from_slice(&kind.to_ne_bytes());
    m.extend_from_slice(&(flags | NLM_F_REQUEST | NLM_F_ACK).to_ne_bytes());
    m.extend_from_slice(&seq.to_ne_bytes());
    m.extend_from_slice(&0u32.to_ne_bytes());
    m
}

/// Appends an `rtattr`, padded to 4 bytes.
fn attr(m: &mut Vec<u8>, kind: u16, data: &[u8]) {
    m.extend_from_slice(&((4 + data.len()) as u16).to_ne_bytes());
    m.extend_from_slice(&kind.to_ne_bytes());
    m.extend_from_slice(data);
    m.resize(m.len().next_multiple_of(4), 0);
}

fn finish(mut m: Vec<u8>) -> Vec<u8> {
    let len = (m.len() as u32).to_ne_bytes();
    m[..4].copy_from_slice(&len);
    m
}

/// `ip link set <index> up`.
pub fn link_up(seq: u32, index: u32) -> Vec<u8> {
    let mut m = header(RTM_NEWLINK, 0, seq);
    m.extend_from_slice(&[AF_UNSPEC, 0]);
    m.extend_from_slice(&0u16.to_ne_bytes()); // ifi_type
    m.extend_from_slice(&(index as i32).to_ne_bytes());
    m.extend_from_slice(&IFF_UP.to_ne_bytes()); // ifi_flags
    m.extend_from_slice(&IFF_UP.to_ne_bytes()); // ifi_change
    finish(m)
}

/// `ip addr add <addr>/<prefix> dev <index>`.
pub fn add_address(seq: u32, index: u32, addr: Ipv4Addr, prefix: u8) -> Vec<u8> {
    let mut m = header(RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, seq);
    m.extend_from_slice(&[AF_INET, prefix, 0, RT_SCOPE_UNIVERSE]);
    m.extend_from_slice(&index.to_ne_bytes());
    attr(&mut m, IFA_LOCAL, &addr.octets());
    attr(&mut m, IFA_ADDRESS, &addr.octets());
    finish(m)
}

/// `ip route add default via <gateway> dev <index>`.
pub fn add_default_route(seq: u32, index: u32, gateway: Ipv4Addr) -> Vec<u8> {
    let mut m = header(RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, seq);
    m.extend_from_slice(&[
        AF_INET,
        0,
        0,
        0,
        RT_TABLE_MAIN,
        RTPROT_BOOT,
        RT_SCOPE_UNIVERSE,
        RTN_UNICAST,
    ]);
    m.extend_from_slice(&0u32.to_ne_bytes()); // rtm_flags
    attr(&mut m, RTA_GATEWAY, &gateway.octets());
    attr(&mut m, RTA_OIF, &index.to_ne_bytes());
    finish(m)
}

/// The kernel's answer to request `seq` in one received datagram: `Some(Ok)` for
/// an ACK, `Some(Err(errno))` for an error, `None` if it is not there.
pub fn ack(buf: &[u8], seq: u32) -> Option<Result<(), i32>> {
    let mut rest = buf;
    while rest.len() >= HEADER {
        let len = u32::from_ne_bytes(rest[..4].try_into().ok()?) as usize;
        if len < HEADER || len > rest.len() {
            return None;
        }
        let kind = u16::from_ne_bytes(rest[4..6].try_into().ok()?);
        let msg_seq = u32::from_ne_bytes(rest[8..12].try_into().ok()?);
        if kind == NLMSG_ERROR && msg_seq == seq && len >= HEADER + 4 {
            let err = i32::from_ne_bytes(rest[HEADER..HEADER + 4].try_into().ok()?);
            return Some(if err == 0 {
                Ok(())
            } else {
                Err(err.checked_neg().unwrap_or(i32::MAX))
            });
        }
        rest = &rest[len.next_multiple_of(4).min(rest.len())..];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_well_formed() {
        let m = link_up(1, 2);
        assert_eq!(m.len(), 32);
        assert_eq!(u32::from_ne_bytes(m[..4].try_into().unwrap()), 32);
        assert_eq!(u16::from_ne_bytes(m[4..6].try_into().unwrap()), RTM_NEWLINK);
        assert_eq!(
            u16::from_ne_bytes(m[6..8].try_into().unwrap()),
            NLM_F_REQUEST | NLM_F_ACK
        );
        assert_eq!(i32::from_ne_bytes(m[20..24].try_into().unwrap()), 2);

        let a = add_address(2, 3, Ipv4Addr::new(172, 30, 0, 2), 30);
        assert_eq!(a.len(), 16 + 8 + 8 + 8);
        assert_eq!(&a[16..20], &[AF_INET, 30, 0, 0]);
        assert_eq!(&a[28..32], &[172, 30, 0, 2]);

        let r = add_default_route(3, 3, Ipv4Addr::new(172, 30, 0, 1));
        assert_eq!(r.len(), 16 + 12 + 8 + 8);
        assert_eq!(&r[32..36], &[172, 30, 0, 1]);
    }

    fn error_msg(seq: u32, err: i32) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&36u32.to_ne_bytes());
        m.extend_from_slice(&NLMSG_ERROR.to_ne_bytes());
        m.extend_from_slice(&0u16.to_ne_bytes());
        m.extend_from_slice(&seq.to_ne_bytes());
        m.extend_from_slice(&0u32.to_ne_bytes());
        m.extend_from_slice(&err.to_ne_bytes());
        m.extend_from_slice(&[0; 16]);
        m
    }

    #[test]
    fn acks_and_errors_are_matched_by_sequence() {
        assert_eq!(ack(&error_msg(7, 0), 7), Some(Ok(())));
        assert_eq!(ack(&error_msg(7, -17), 7), Some(Err(17)));
        assert_eq!(ack(&error_msg(6, 0), 7), None);
        let mut two = error_msg(6, 0);
        two.extend(error_msg(7, -1));
        assert_eq!(ack(&two, 7), Some(Err(1)));
        assert_eq!(ack(&error_msg(7, i32::MIN), 7), Some(Err(i32::MAX)));
        assert_eq!(ack(&[0; 8], 1), None);
        assert_eq!(ack(&error_msg(7, 0)[..20], 7), None);
    }
}
