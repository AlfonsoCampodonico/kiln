//! The host side of T9 without a VM: the boot tests' driver against a fake
//! guest on a socket pair (spec §9.5, §11.5).
#![cfg(target_os = "linux")]

mod support;

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::channel;

use kiln_proto::{
    Config, ExitMethod, GuestMessage, Hello, HostMessage, Process, Scratch, Stage, read_message, write_message,
};
use support::driver::{Event, Reply, serve_control};

fn config() -> Config {
    Config {
        process: Process {
            cmd: vec!["/bin/true".into()],
            ..Process::default()
        },
        stop_signal: 15,
        tty: None,
        interactive: false,
        hostname: "h".into(),
        network: None,
        layers: 1,
        scratch: Scratch { size_bytes: 1 << 30 },
        exit_method: ExitMethod::Reboot,
        shutdown_grace_secs: 10,
    }
}

/// Runs the driver against `guest` (which writes to its end) and returns its events.
fn drive(guest: impl FnOnce(&mut UnixStream) + Send + 'static) -> Vec<Event> {
    let (host, mut fake) = UnixStream::pair().unwrap();
    let (tx, rx) = channel();
    let g = std::thread::spawn(move || {
        guest(&mut fake);
        fake
    });
    serve_control(host, &config(), &Reply::Config, &tx);
    drop(g.join().unwrap());
    drop(tx);
    rx.into_iter().collect()
}

fn hello(s: &mut UnixStream) {
    write_message(s, &GuestMessage::Hello(Hello { protocol: 1 })).unwrap();
}

fn violation(events: &[Event]) -> &str {
    match events.last() {
        Some(Event::Violation(why)) => why,
        other => panic!("expected a violation, got {other:?}"),
    }
}

#[test]
fn the_first_hello_gets_config_once() {
    let events = drive(|s| {
        hello(s);
        let reply = read_message::<_, HostMessage>(s).unwrap();
        assert_eq!(reply, Some(HostMessage::Config(Box::new(config()))));
        write_message(s, &GuestMessage::Stage(Stage { n: 3 })).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
    });
    assert!(
        matches!(
            events[..],
            [
                Event::Guest(GuestMessage::Hello(_)),
                Event::Guest(GuestMessage::Stage(_)),
                Event::Closed
            ]
        ),
        "{events:?}"
    );
}

#[test]
fn a_second_hello_is_a_violation() {
    let events = drive(|s| {
        hello(s);
        hello(s);
    });
    assert_eq!(violation(&events), "a second Hello");
}

#[test]
fn messages_before_hello_and_unknown_versions_are_violations() {
    let events = drive(|s| write_message(s, &GuestMessage::Running).unwrap());
    assert_eq!(violation(&events), "a message before Hello");
    let events = drive(|s| write_message(s, &GuestMessage::Hello(Hello { protocol: 2 })).unwrap());
    assert_eq!(violation(&events), "unsupported guest protocol 2");
}

#[test]
fn oversized_frames_and_invalid_json_are_violations() {
    let events = drive(|s| s.write_all(&(70_000u32).to_le_bytes()).unwrap());
    assert!(violation(&events).contains("exceeds"), "{events:?}");
    let events = drive(|s| {
        hello(s);
        s.write_all(&[5, 0, 0, 0, 5, b'{', b'x', b':', b'}']).unwrap();
    });
    assert!(violation(&events).contains("invalid Exited payload"), "{events:?}");
    let events = drive(|s| {
        hello(s);
        s.write_all(&[3, 0, 0, 0, 2, b'{', b'}']).unwrap();
    });
    assert!(violation(&events).contains("not valid in this direction"), "{events:?}");
}
