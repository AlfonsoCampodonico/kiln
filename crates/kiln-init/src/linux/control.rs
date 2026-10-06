//! Stage 2: connect vsock port 1024, send `Hello`, receive `Config` (spec §9.6).
//! Later host messages are read by a thread and handed to the supervisor.

use std::sync::Mutex;
use std::sync::mpsc::Sender;

use kiln_proto::{Config, GuestMessage, HOST_CID, Hello, HostMessage, PROTOCOL_VERSION, port, read_message};

use super::sys::Vsock;
use super::{CONTROL, EXIT, Event, HOST_FAILED, send};
use crate::error::{Context, Failure, Result};

/// Returns the validated `Config`; host messages after it go to `events`.
pub fn connect(events: Sender<Event>) -> Result<Config> {
    let stream = Vsock::connect(HOST_CID, port::CONTROL).context("connect to the host on vsock port 1024")?;
    let mut reader = stream.try_clone().context("dup the control socket")?;
    CONTROL
        .set(Mutex::new(stream))
        .expect("the control channel is set up once");
    send(&GuestMessage::Hello(Hello {
        protocol: PROTOCOL_VERSION,
    }))?;
    let config = match read_message::<_, HostMessage>(&mut reader) {
        Ok(Some(HostMessage::Config(config))) => *config,
        Ok(Some(other)) => {
            return Err(Failure::msg(format!(
                "protocol violation: expected Config, got {other:?}"
            )));
        }
        Ok(None) => return Err(Failure::msg("the host closed the control connection before Config")),
        Err(e) => return Err(Failure::msg(format!("protocol violation: {e}"))),
    };
    EXIT.set(config.exit_method).expect("Config arrives once");
    std::thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            loop {
                let event = match read_message::<_, HostMessage>(&mut reader) {
                    Ok(Some(msg)) => Event::Host(msg),
                    Ok(None) => {
                        let _ = HOST_FAILED.set("the host closed the control connection".into());
                        Event::HostClosed
                    }
                    Err(e) => {
                        let _ = HOST_FAILED.set(format!("protocol violation: {e}"));
                        Event::HostError(e)
                    }
                };
                let last = !matches!(event, Event::Host(_));
                if events.send(event).is_err() || last {
                    return;
                }
            }
        })
        .context("spawn the control reader")?;
    Ok(config)
}
