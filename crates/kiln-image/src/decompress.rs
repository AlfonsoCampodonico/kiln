//! Opening a stored layer as a bounded, hashed tar stream (spec §6.1, §7.6).

use std::io::{self, BufReader, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kiln_oci::media::Compression;

/// Shared per-image budget of decompressed bytes.
#[derive(Debug, Default)]
pub struct Budget {
    used: AtomicU64,
}

/// Which limit a [`Bounded`] reader tripped.
#[derive(Debug, Default)]
pub struct Tripped {
    layer: AtomicBool,
    image: AtomicBool,
}

impl Tripped {
    pub fn layer(&self) -> bool {
        self.layer.load(Ordering::SeqCst)
    }

    pub fn image(&self) -> bool {
        self.image.load(Ordering::SeqCst)
    }
}

/// Fails reads past `layer_max` bytes, or past the shared image budget.
pub struct Bounded<R> {
    inner: R,
    read: u64,
    layer_max: u64,
    budget: Arc<Budget>,
    image_max: u64,
    tripped: Arc<Tripped>,
}

impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        if self.read > self.layer_max {
            self.tripped.layer.store(true, Ordering::SeqCst);
            return Err(io::Error::other("kiln: layer expansion limit exceeded"));
        }
        if self.budget.used.fetch_add(n as u64, Ordering::SeqCst) + n as u64 > self.image_max {
            self.tripped.image.store(true, Ordering::SeqCst);
            return Err(io::Error::other("kiln: image size limit exceeded"));
        }
        Ok(n)
    }
}

/// The most decompressed bytes a layer of `compressed` bytes may produce: the
/// expansion ratio (with a 1 MiB floor for tiny blobs), never above `layer_cap`.
pub fn layer_limit(compression: Compression, compressed: u64, ratio: u64, layer_cap: u64) -> u64 {
    match compression {
        Compression::None => compressed.min(layer_cap),
        _ => compressed.saturating_mul(ratio).max(1 << 20).min(layer_cap),
    }
}

/// Wraps a stored blob in its decompressor and the limits.
pub fn open_bounded<'a>(
    blob: impl Read + 'a,
    compression: Compression,
    layer_max: u64,
    budget: Arc<Budget>,
    image_max: u64,
    tripped: Arc<Tripped>,
) -> io::Result<Bounded<Box<dyn Read + 'a>>> {
    let inner: Box<dyn Read + 'a> = match compression {
        Compression::None => Box::new(blob),
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(BufReader::new(blob))),
        Compression::Zstd => Box::new(zstd::stream::read::Decoder::new(blob)?),
    };
    Ok(Bounded {
        inner,
        read: 0,
        layer_max,
        budget,
        image_max,
        tripped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn drain(r: &mut dyn Read) -> io::Result<Vec<u8>> {
        let mut v = Vec::new();
        r.read_to_end(&mut v)?;
        Ok(v)
    }

    #[test]
    fn decompresses_gzip_multimember_and_zstd_multiframe() {
        let mut gz = Vec::new();
        for part in [&b"hello "[..], b"world"] {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(part).unwrap();
            gz.extend(e.finish().unwrap());
        }
        let b = Arc::new(Budget::default());
        let t = Arc::new(Tripped::default());
        assert_eq!(
            drain(&mut open_bounded(&gz[..], Compression::Gzip, 100, b.clone(), 100, t.clone()).unwrap()).unwrap(),
            b"hello world"
        );
        let mut zs = zstd::encode_all(&b"one "[..], 3).unwrap();
        zs.extend(zstd::encode_all(&b"two"[..], 3).unwrap());
        assert_eq!(
            drain(&mut open_bounded(&zs[..], Compression::Zstd, 100, b, 1000, t).unwrap()).unwrap(),
            b"one two"
        );
    }

    #[test]
    fn trips_layer_and_image_limits() {
        let zeros = vec![0u8; 10_000];
        let gz = {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            e.write_all(&zeros).unwrap();
            e.finish().unwrap()
        };
        let t = Arc::new(Tripped::default());
        assert!(
            drain(
                &mut open_bounded(
                    &gz[..],
                    Compression::Gzip,
                    5000,
                    Arc::new(Budget::default()),
                    u64::MAX,
                    t.clone()
                )
                .unwrap()
            )
            .is_err()
        );
        assert!(t.layer() && !t.image());
        let t = Arc::new(Tripped::default());
        let b = Arc::new(Budget::default());
        assert!(
            drain(&mut open_bounded(&zeros[..], Compression::None, u64::MAX, b, 4000, t.clone()).unwrap()).is_err()
        );
        assert!(t.image());
    }

    #[test]
    fn layer_limit_rules() {
        assert_eq!(layer_limit(Compression::None, 10, 200, 1000), 10);
        assert_eq!(
            layer_limit(Compression::Gzip, 10, 200, u64::MAX),
            1 << 20,
            "1 MiB floor"
        );
        assert_eq!(layer_limit(Compression::Gzip, 1 << 20, 200, u64::MAX), 200 << 20);
        assert_eq!(
            layer_limit(Compression::Zstd, 1 << 30, 200, 16 << 30),
            16 << 30,
            "capped"
        );
    }
}
