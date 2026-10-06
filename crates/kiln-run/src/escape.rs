//! The `-t` escape sequence (spec §9.7): `Ctrl-]` then `q` asks the guest to shut
//! down, `Ctrl-]` then `k` kills the VM, `Ctrl-]` twice sends one `Ctrl-]`. Any other
//! byte after `Ctrl-]` is sent with it, unchanged.

/// `Ctrl-]`.
pub const ESCAPE: u8 = 0x1d;

/// What the user's keystrokes mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// Bytes for the guest's terminal.
    Data(Vec<u8>),
    Shutdown,
    Kill,
}

/// Splits terminal input into data and escape commands, across reads.
#[derive(Debug, Default)]
pub struct Escape {
    pending: bool,
}

impl Escape {
    pub fn feed(&mut self, input: &[u8]) -> Vec<Key> {
        let mut out = Vec::new();
        let mut data = Vec::with_capacity(input.len());
        for &b in input {
            if self.pending {
                self.pending = false;
                match b {
                    b'q' | b'Q' => {
                        flush(&mut data, &mut out);
                        out.push(Key::Shutdown);
                    }
                    b'k' | b'K' => {
                        flush(&mut data, &mut out);
                        out.push(Key::Kill);
                    }
                    ESCAPE => data.push(ESCAPE),
                    other => data.extend_from_slice(&[ESCAPE, other]),
                }
            } else if b == ESCAPE {
                self.pending = true;
            } else {
                data.push(b);
            }
        }
        flush(&mut data, &mut out);
        out
    }
}

fn flush(data: &mut Vec<u8>, out: &mut Vec<Key>) {
    if !data.is_empty() {
        out.push(Key::Data(std::mem::take(data)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_data_and_split_reads() {
        let mut e = Escape::default();
        assert_eq!(e.feed(b"ls\r"), [Key::Data(b"ls\r".to_vec())]);
        assert_eq!(
            e.feed(b"a\x1dqb"),
            [Key::Data(b"a".to_vec()), Key::Shutdown, Key::Data(b"b".to_vec())]
        );
        assert_eq!(e.feed(b"\x1d"), []);
        assert_eq!(e.feed(b"k"), [Key::Kill]);
        assert_eq!(e.feed(b"\x1d\x1d"), [Key::Data(vec![ESCAPE])]);
        assert_eq!(e.feed(b"\x1dx"), [Key::Data(vec![ESCAPE, b'x'])]);
        assert_eq!(e.feed(b"\x03"), [Key::Data(vec![3])], "Ctrl-C goes to the guest");
    }
}
