//! `console.log` (spec §5.3, §9.7): the guest's serial console, kept to at most
//! 1 MiB, and its sanitised tail for failure reports (T8).
//!
//! The VMM writes the console to a FIFO; kiln copies it into the log through
//! [`Ring`], which drops the oldest half of the file when it would outgrow its
//! cap, so a guest that floods its console costs bounded disk.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use kiln_proto::sanitize::clean_line;

/// The most `console.log` holds.
pub const CAP: u64 = 1 << 20;
/// Lines of the console printed when a run fails.
pub const TAIL_LINES: usize = 20;

/// An append-only file of at most `cap` bytes.
pub struct Ring {
    path: PathBuf,
    file: File,
    len: u64,
    cap: u64,
}

impl Ring {
    /// Creates `path` (which must not exist).
    pub fn create(path: &Path, cap: u64) -> io::Result<Self> {
        let file = create_new(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            len: 0,
            cap: cap.max(2),
        })
    }

    pub fn write(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        if bytes.len() as u64 >= self.cap {
            bytes = &bytes[bytes.len() - (self.cap / 2) as usize..];
            self.keep_tail(0)?;
        } else if self.len + bytes.len() as u64 > self.cap {
            self.keep_tail(self.cap / 2)?;
        }
        self.file.write_all(bytes)?;
        self.len += bytes.len() as u64;
        Ok(())
    }

    /// Rewrites the file with only its last `keep` bytes.
    fn keep_tail(&mut self, keep: u64) -> io::Result<()> {
        let keep = keep.min(self.len);
        let mut tail = vec![0u8; keep as usize];
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(self.len - keep))?;
        f.read_exact(&mut tail)?;
        let tmp = self.path.with_extension("log.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut new = create_new(&tmp)?;
        new.write_all(&tail)?;
        std::fs::rename(&tmp, &self.path)?;
        self.file = new;
        self.len = keep;
        Ok(())
    }
}

fn create_new(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.append(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    o.open(path)
}

/// The last `n` lines of the console, each sanitised (T8); empty lines dropped.
pub fn tail(path: &Path, n: usize) -> Vec<String> {
    let Ok(mut f) = File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    // Enough for `n` long lines; the ring is at most 1 MiB anyway.
    let start = len.saturating_sub(64 * 1024);
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(start)).is_err() || f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<String> = text
        .lines()
        .skip(usize::from(start > 0))
        .map(clean_line)
        .filter(|l| !l.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_newest_bytes_under_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.log");
        let mut r = Ring::create(&path, 100).unwrap();
        r.write(b"0123456789").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"0123456789");
        for i in 0..50u8 {
            r.write(&[b'a' + i % 26; 7]).unwrap();
            assert!(std::fs::metadata(&path).unwrap().len() <= 100);
        }
        let log = std::fs::read(&path).unwrap();
        assert!(log.ends_with(&[b'a' + 49 % 26; 7]), "{log:?}");
        // A write larger than the cap keeps its own end.
        let big: Vec<u8> = (0..1000u32).map(|i| (i % 256) as u8).collect();
        r.write(&big).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), &big[950..]);
        r.write(b"x").unwrap();
        assert_eq!(std::fs::read(&path).unwrap().len(), 51);
        assert!(Ring::create(&path, 100).is_err(), "an existing log is never reused");
    }

    #[test]
    fn the_tail_is_sanitised() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.log");
        let mut text = String::new();
        for i in 0..30 {
            text.push_str(&format!("line {i}\n"));
        }
        text.push_str("evil \x1b[2J\u{9b}31m end\r\n\n");
        std::fs::write(&path, text).unwrap();
        let t = tail(&path, 3);
        assert_eq!(t[..2], ["line 28", "line 29"]);
        assert!(!t[2].chars().any(char::is_control), "{:?}", t[2]);
        assert!(tail(&dir.path().join("missing"), 3).is_empty());
    }
}
