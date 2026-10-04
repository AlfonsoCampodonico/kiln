//! Property tests: every valid message round-trips; arbitrary bytes never panic
//! the decoder and never decode to an invalid message.

use std::net::Ipv4Addr;

use kiln_proto::{
    Config, ExitMethod, Exited, GuestMessage, Hello, HostMessage, InitFailed, MAX_FRAME, Message, Network, Process,
    Scratch, Shutdown, Signal, Stage, WindowSize, read_message, write_message,
};
use proptest::prelude::*;

fn text() -> impl Strategy<Value = String> {
    "[^\u{0}]{0,40}"
}

fn guest() -> impl Strategy<Value = GuestMessage> {
    prop_oneof![
        (1u32..).prop_map(|protocol| GuestMessage::Hello(Hello { protocol })),
        (1u8..=7).prop_map(|n| GuestMessage::Stage(Stage { n })),
        Just(GuestMessage::Running),
        (0i32..=255).prop_map(|code| GuestMessage::Exited(Exited { signaled: false, code })),
        (1i32..=64).prop_map(|code| GuestMessage::Exited(Exited { signaled: true, code })),
        (1u8..=7, proptest::option::of(1i32..4096), "\\PC{0,200}")
            .prop_map(|(s, e, m)| GuestMessage::InitFailed(InitFailed::new(s, e, &m))),
    ]
}

fn config() -> impl Strategy<Value = Config> {
    (
        proptest::collection::vec("[a-z/]{1,10}", 1..4),
        proptest::collection::vec(text(), 0..4),
        proptest::collection::vec("[A-Z_]{1,8}=[^\u{0}]{0,20}", 0..6),
        proptest::option::of("/[a-z/]{0,20}"),
        proptest::option::of("[a-z0-9]{1,8}(:[a-z0-9]{1,8})?"),
        (1i32..=31, any::<bool>(), any::<bool>(), "[a-z][a-z0-9.-]{0,20}"),
        (
            0u32..=128,
            16_384u64..=(1 << 20),
            any::<bool>(),
            any::<u32>(),
            any::<bool>(),
        ),
    )
        .prop_map(|(entrypoint, cmd, env, working_dir, user, sig, more)| {
            let (stop_signal, tty, interactive, hostname) = sig;
            let (layers, blocks, poweroff, grace, net) = more;
            Config {
                process: Process {
                    entrypoint,
                    cmd,
                    env,
                    working_dir,
                    user,
                },
                stop_signal,
                tty: tty.then_some(WindowSize { rows: 24, cols: 80 }),
                interactive,
                hostname,
                network: net.then(|| Network {
                    address: Ipv4Addr::new(172, 30, 0, 2),
                    prefix_len: 30,
                    gateway: Ipv4Addr::new(172, 30, 0, 1),
                    dns: vec![Ipv4Addr::new(172, 30, 0, 1)],
                }),
                layers,
                scratch: Scratch {
                    size_bytes: blocks * 4096,
                },
                exit_method: if poweroff {
                    ExitMethod::Poweroff
                } else {
                    ExitMethod::Reboot
                },
                shutdown_grace_secs: grace,
            }
        })
}

fn host() -> impl Strategy<Value = HostMessage> {
    prop_oneof![
        config().prop_map(|c| HostMessage::Config(Box::new(c))),
        any::<u32>().prop_map(|grace_secs| HostMessage::Shutdown(Shutdown { grace_secs })),
        (1i32..=31).prop_map(|sig| HostMessage::Signal(Signal { sig })),
        (any::<u16>(), any::<u16>()).prop_map(|(rows, cols)| HostMessage::WindowSize(WindowSize { rows, cols })),
    ]
}

fn round_trip<M: Message + PartialEq + std::fmt::Debug>(msgs: &[M]) {
    let mut buf = Vec::new();
    for m in msgs {
        write_message(&mut buf, m).unwrap();
    }
    let mut r = buf.as_slice();
    for m in msgs {
        assert_eq!(read_message::<_, M>(&mut r).unwrap().as_ref(), Some(m));
    }
    assert!(read_message::<_, M>(&mut r).unwrap().is_none());
}

/// Decodes `bytes` as a stream until EOF or the first error. Anything decoded must be valid.
fn decode_all<M: Message>(bytes: &[u8]) {
    let mut r = bytes;
    while let Ok(Some(m)) = read_message::<_, M>(&mut r) {
        m.validate().expect("decoded messages are valid");
    }
}

fn framed(ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = ((payload.len() + 1) as u32).to_le_bytes().to_vec();
    f.push(ty);
    f.extend_from_slice(payload);
    f
}

/// One `"key":value` member, mostly with real field names and plausible values, so
/// that decoding gets past the object check into serde and `validate`.
fn member() -> impl Strategy<Value = String> {
    let key = prop_oneof![
        4 => proptest::sample::select(vec![
            "protocol", "n", "signaled", "code", "stage", "errno", "message", "graceSecs", "sig", "rows", "cols",
            "process", "stopSignal", "interactive", "hostname", "layers", "scratch", "exitMethod", "shutdownGraceSecs",
        ])
        .prop_map(String::from),
        1 => "[a-zA-Z\\u{1b}]{1,8}",
    ];
    let value = prop_oneof![
        3 => (-5i64..70).prop_map(|n| n.to_string()),
        1 => any::<i64>().prop_map(|n| n.to_string()),
        1 => Just("true".to_string()),
        1 => Just("null".to_string()),
        1 => "[a-z]{0,6}".prop_map(|s| format!("\"{s}\"")),
        1 => Just("{}".to_string()),
        1 => Just("[]".to_string()),
    ];
    (key, value).prop_map(|(k, v)| format!("\"{k}\":{v}"))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn guest_messages_round_trip(msgs in proptest::collection::vec(guest(), 1..8)) {
        round_trip(&msgs);
    }

    #[test]
    fn host_messages_round_trip(msgs in proptest::collection::vec(host(), 1..8)) {
        round_trip(&msgs);
    }

    #[test]
    fn arbitrary_streams_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        decode_all::<GuestMessage>(&bytes);
        decode_all::<HostMessage>(&bytes);
    }

    #[test]
    fn arbitrary_payloads_never_panic(ty in 0u8..12, payload in "\\PC{0,120}") {
        decode_all::<GuestMessage>(&framed(ty, payload.as_bytes()));
        decode_all::<HostMessage>(&framed(ty, payload.as_bytes()));
    }

    #[test]
    fn corrupted_frames_fail_or_stay_valid(msg in host(), flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..4)) {
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        for (at, byte) in flips {
            let i = at % buf.len();
            buf[i] ^= byte | 1;
        }
        decode_all::<HostMessage>(&buf);
    }

    #[test]
    fn corrupted_guest_frames_fail_or_stay_valid(msg in guest(), flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..4)) {
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        for (at, byte) in flips {
            let i = at % buf.len();
            buf[i] ^= byte | 1;
        }
        decode_all::<GuestMessage>(&buf);
    }

    #[test]
    fn object_shaped_payloads_reach_serde_and_validate(ty in 0u8..12, members in proptest::collection::vec(member(), 0..6)) {
        let payload = format!("{{{}}}", members.join(","));
        decode_all::<GuestMessage>(&framed(ty, payload.as_bytes()));
        decode_all::<HostMessage>(&framed(ty, payload.as_bytes()));
    }

    #[test]
    fn oversize_lengths_are_always_refused(len in (MAX_FRAME + 1)..=u32::MAX) {
        let mut r = &len.to_le_bytes()[..];
        prop_assert!(read_message::<_, GuestMessage>(&mut r).is_err());
    }
}
