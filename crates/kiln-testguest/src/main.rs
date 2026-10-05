//! A hostile guest program for kiln-init's boot tests (spec §11.5): run as the workload,
//! it connects to the host's control port a second time and sends `Hello`, as a
//! reset guest would. The host must kill the VM. Build it static, like `kiln-init`.
#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
fn main() {
    use kiln_init::linux::sys::Vsock;
    use kiln_proto::{GuestMessage, HOST_CID, Hello, PROTOCOL_VERSION, port, write_message};

    let mut control = Vsock::connect(HOST_CID, port::CONTROL).expect("connect to the control port");
    let hello = GuestMessage::Hello(Hello {
        protocol: PROTOCOL_VERSION,
    });
    write_message(&mut control, &hello).expect("send Hello");
    println!("hostile: sent a second Hello");
    // The host should end the VM before this does.
    std::thread::sleep(std::time::Duration::from_secs(60));
}

#[cfg(not(target_os = "linux"))]
fn main() {}
