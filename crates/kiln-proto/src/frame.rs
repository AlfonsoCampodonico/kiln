//! Framing: `u32` LE length (type byte + payload, at most [`MAX_FRAME`]), `u8` type, JSON payload.

use std::io::{ErrorKind, Read, Write};

use crate::MAX_FRAME;
use crate::error::{ProtoError, Result};
use crate::message::Message;

/// Validates and writes one message as a single frame (one `write_all`).
pub fn write_message<W: Write + ?Sized, M: Message>(w: &mut W, msg: &M) -> Result<()> {
    msg.validate()?;
    let payload = msg.payload();
    let len = u32::try_from(payload.len() + 1).map_err(|_| ProtoError::Oversize(u32::MAX))?;
    if len > MAX_FRAME {
        return Err(ProtoError::Oversize(len));
    }
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.push(msg.message_type());
    frame.extend_from_slice(&payload);
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

/// Reads one frame and decodes it. `Ok(None)` is a clean EOF before the frame's
/// first byte; EOF anywhere else is [`ProtoError::Truncated`]. An oversize
/// length is rejected before anything else is read.
pub fn read_message<R: Read + ?Sized, M: Message>(r: &mut R) -> Result<Option<M>> {
    let mut len = [0u8; 4];
    if !read_full(r, &mut len, true)? {
        return Ok(None);
    }
    let len = u32::from_le_bytes(len);
    if len == 0 {
        return Err(ProtoError::Empty);
    }
    if len > MAX_FRAME {
        return Err(ProtoError::Oversize(len));
    }
    let mut body = vec![0u8; len as usize];
    read_full(r, &mut body, false)?;
    M::decode(body[0], &body[1..]).map(Some)
}

/// Fills `buf`. Returns `false` on EOF before the first byte when `eof_ok`.
fn read_full<R: Read + ?Sized>(r: &mut R, buf: &mut [u8], eof_ok: bool) -> Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 && eof_ok => return Ok(false),
            Ok(0) => return Err(ProtoError::Truncated),
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GuestMessage, Hello, HostMessage, Signal};

    fn frame(len: u32, ty: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = len.to_le_bytes().to_vec();
        f.push(ty);
        f.extend_from_slice(payload);
        f
    }

    #[test]
    fn round_trips_and_reports_clean_eof() {
        let mut buf = Vec::new();
        write_message(&mut buf, &GuestMessage::Hello(Hello { protocol: 1 })).unwrap();
        write_message(&mut buf, &GuestMessage::Running).unwrap();
        assert_eq!(&buf[..5], &[15, 0, 0, 0, 1]);
        let mut r = buf.as_slice();
        assert_eq!(
            read_message::<_, GuestMessage>(&mut r).unwrap(),
            Some(GuestMessage::Hello(Hello { protocol: 1 }))
        );
        assert_eq!(
            read_message::<_, GuestMessage>(&mut r).unwrap(),
            Some(GuestMessage::Running)
        );
        assert_eq!(read_message::<_, GuestMessage>(&mut r).unwrap(), None);
    }

    #[test]
    fn rejects_bad_lengths_before_reading_the_body() {
        let mut r = &(MAX_FRAME + 1).to_le_bytes()[..];
        assert!(matches!(
            read_message::<_, HostMessage>(&mut r),
            Err(ProtoError::Oversize(n)) if n == MAX_FRAME + 1
        ));
        let mut r = &0u32.to_le_bytes()[..];
        assert!(matches!(read_message::<_, HostMessage>(&mut r), Err(ProtoError::Empty)));
    }

    #[test]
    fn eof_inside_a_frame_is_truncation() {
        let full = frame(15, 1, br#"{"protocol":1}"#);
        for cut in 1..full.len() {
            let mut r = &full[..cut];
            assert!(
                matches!(read_message::<_, GuestMessage>(&mut r), Err(ProtoError::Truncated)),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn invalid_messages_are_never_written() {
        let mut buf = Vec::new();
        assert!(write_message(&mut buf, &HostMessage::Signal(Signal { sig: 99 })).is_err());
        assert!(buf.is_empty());
    }

    #[test]
    fn a_frame_at_the_limit_is_accepted_and_one_over_is_not() {
        // A `Hello` padded with JSON whitespace to exactly MAX_FRAME bytes.
        let pad = MAX_FRAME as usize - 1 - br#"{"protocol":1}"#.len();
        let payload = format!("{{{}\"protocol\":1}}", " ".repeat(pad));
        let mut r = &frame(MAX_FRAME, 1, payload.as_bytes())[..];
        assert!(read_message::<_, GuestMessage>(&mut r).unwrap().is_some());
    }
}
