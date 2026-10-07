//! Hostile guest programs for kiln's boot tests (spec §11.5). Build it static, like
//! `kiln-init`.
//!
//! - As the workload (under the real kiln-init), it connects to the host's control
//!   port a second time and sends `Hello`, as a reset guest would.
//! - As PID 1 (the init layer built around it instead of kiln-init), it speaks the
//!   protocol itself and misbehaves as `Config.process.cmd[0]` says:
//!   `stage-flood`, `exited-then-hang`, `second-stdout`, `no-read`, `hang-before-running`.
//!
//! The host must end the VM in every case.
#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
fn main() {
    use std::time::Duration;

    use kiln_init::linux::sys::Vsock;
    use kiln_proto::{
        Exited, GuestMessage, HOST_CID, Hello, HostMessage, PROTOCOL_VERSION, Stage, port, read_message, write_message,
    };

    let hello = GuestMessage::Hello(Hello {
        protocol: PROTOCOL_VERSION,
    });
    let mut control = Vsock::connect(HOST_CID, port::CONTROL).expect("connect to the control port");
    write_message(&mut control, &hello).expect("send Hello");
    if std::process::id() != 1 {
        println!("hostile: sent a second Hello");
        std::thread::sleep(Duration::from_secs(60));
        return;
    }
    let mode = match read_message::<_, HostMessage>(&mut control) {
        Ok(Some(HostMessage::Config(c))) => c.process.cmd.first().cloned().unwrap_or_default(),
        _ => String::new(),
    };
    let mut send = |m: GuestMessage| write_message(&mut control, &m);
    let boot = |send: &mut dyn FnMut(GuestMessage) -> kiln_proto::Result<()>| {
        for n in 3..=6 {
            let _ = send(GuestMessage::Stage(Stage { n }));
        }
        let _ = send(GuestMessage::Running);
        let _ = send(GuestMessage::Stage(Stage { n: 7 }));
    };
    match mode.as_str() {
        "stage-flood" => while send(GuestMessage::Stage(Stage { n: 3 })).is_ok() {},
        "exited-then-hang" => {
            boot(&mut send);
            let _ = send(GuestMessage::Exited(Exited {
                signaled: false,
                code: 0,
            }));
        }
        "second-stdout" => {
            boot(&mut send);
            let _a = Vsock::connect(HOST_CID, port::STDOUT);
            let _b = Vsock::connect(HOST_CID, port::STDOUT);
            std::thread::sleep(Duration::from_secs(3600));
        }
        "no-read" => boot(&mut send),
        _ => {
            let _ = send(GuestMessage::Stage(Stage { n: 3 }));
        }
    }
    // PID 1 must not exit (the kernel would panic and the VM end): the host must end it.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {}
