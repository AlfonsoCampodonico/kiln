//! SHA-256 content digests (`sha256:<hex>`), the only algorithm kiln accepts.

use std::fmt;
use std::io::{self, Read, Write};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

use crate::error::{Result, StoreError};

/// A SHA-256 digest, displayed and parsed as `sha256:<64 lowercase hex>`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest {
    hex: String,
}

impl Digest {
    pub fn parse(s: &str) -> Result<Self> {
        let hex = s
            .strip_prefix("sha256:")
            .ok_or_else(|| StoreError::BadDigest(s.to_string()))?;
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(StoreError::BadDigest(s.to_string()));
        }
        Ok(Self { hex: hex.to_string() })
    }

    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        let mut h = Hasher::new();
        h.update(bytes);
        h.finish()
    }

    pub fn hex(&self) -> &str {
        &self.hex
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.hex)
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.hex)
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Digest::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Incremental SHA-256.
#[derive(Clone, Default)]
pub struct Hasher(Sha256);

impl Hasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn finish(self) -> Digest {
        let out = self.0.finalize();
        Digest {
            hex: out.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }
}

impl Write for Hasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A reader that hashes and counts every byte it passes through.
pub struct HashingReader<R> {
    inner: R,
    hasher: Hasher,
    count: u64,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Hasher::new(),
            count: 0,
        }
    }

    /// Bytes read so far.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Reads the rest of the stream (so trailing bytes are hashed too) and
    /// returns the digest and total byte count.
    pub fn finish_to_eof(mut self) -> io::Result<(Digest, u64)> {
        io::copy(&mut self, &mut io::sink())?;
        Ok((self.hasher.finish(), self.count))
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn digest_of_empty_matches_sha256() {
        assert_eq!(Digest::of(b"").to_string(), EMPTY);
    }

    #[test]
    fn parse_round_trips_and_rejects_bad_input() {
        assert_eq!(Digest::parse(EMPTY).unwrap().to_string(), EMPTY);
        for bad in [
            "",
            "sha256:",
            "sha512:00",
            "sha256:E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855",
            &format!("{EMPTY}0"),
            "sha256:../../etc/passwd",
        ] {
            assert!(Digest::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hashing_reader_hashes_trailing_bytes() {
        let mut r = HashingReader::new(&b"hello world"[..]);
        let mut first = [0u8; 5];
        r.read_exact(&mut first).unwrap();
        let (d, n) = r.finish_to_eof().unwrap();
        assert_eq!((d, n), (Digest::of(b"hello world"), 11));
    }

    #[test]
    fn serde_uses_the_string_form() {
        let d = Digest::of(b"x");
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, format!("\"{d}\""));
        assert_eq!(serde_json::from_str::<Digest>(&json).unwrap(), d);
        assert!(serde_json::from_str::<Digest>("\"sha256:zz\"").is_err());
    }
}
