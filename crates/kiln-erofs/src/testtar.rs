//! Byte-exact tar stream builder for tests and fixtures. Not for production use.

/// Header options for one entry.
#[derive(Debug, Clone)]
pub struct Opts {
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime: u64,
    pub pax: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: 1_700_000_000,
            pax: Vec::new(),
        }
    }
}

impl Opts {
    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }
    pub fn uid(mut self, uid: u64) -> Self {
        self.uid = uid;
        self
    }
    pub fn gid(mut self, gid: u64) -> Self {
        self.gid = gid;
        self
    }
    pub fn mtime(mut self, mtime: u64) -> Self {
        self.mtime = mtime;
        self
    }
    pub fn pax(mut self, key: &str, value: &[u8]) -> Self {
        self.pax.push((key.as_bytes().to_vec(), value.to_vec()));
        self
    }
    pub fn xattr(self, name: &str, value: &[u8]) -> Self {
        self.pax(&format!("SCHILY.xattr.{name}"), value)
    }
}

/// Encodes one PAX record with a self-consistent length prefix.
pub fn pax_record(key: &[u8], value: &[u8]) -> Vec<u8> {
    let body = key.len() + value.len() + 3; // ' ', '=', '\n'
    let mut len = body + 1;
    while len.to_string().len() + body != len {
        len = body + len.to_string().len();
    }
    let mut rec = format!("{len} ").into_bytes();
    rec.extend_from_slice(key);
    rec.push(b'=');
    rec.extend_from_slice(value);
    rec.push(b'\n');
    rec
}

/// Appends raw tar entries to an in-memory buffer.
#[derive(Debug, Default)]
pub struct TarBuilder {
    buf: Vec<u8>,
}

impl TarBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    fn header(&mut self, name: &[u8], typeflag: u8, size: u64, link: &[u8], dev: (u32, u32), o: &Opts) {
        let mut h = tar::Header::new_ustar();
        h.set_mode(o.mode);
        h.set_uid(o.uid);
        h.set_gid(o.gid);
        h.set_mtime(o.mtime);
        h.set_size(size);
        if typeflag == b'3' || typeflag == b'4' {
            h.set_device_major(dev.0).expect("ustar device major");
            h.set_device_minor(dev.1).expect("ustar device minor");
        }
        let old = h.as_old_mut();
        let n = name.len().min(100);
        old.name[..n].copy_from_slice(&name[..n]);
        let l = link.len().min(100);
        old.linkname[..l].copy_from_slice(&link[..l]);
        old.linkflag = [typeflag];
        h.set_cksum();
        self.buf.extend_from_slice(h.as_bytes());
    }

    fn data(&mut self, d: &[u8]) {
        self.buf.extend_from_slice(d);
        let pad = (512 - d.len() % 512) % 512;
        self.buf.resize(self.buf.len() + pad, 0);
    }

    /// Appends a PAX local header (`x`) carrying `records`.
    pub fn pax_header(&mut self, records: &[(Vec<u8>, Vec<u8>)]) -> &mut Self {
        let mut payload = Vec::new();
        for (k, v) in records {
            payload.extend(pax_record(k, v));
        }
        self.header(
            b"././@PaxHeader",
            b'x',
            payload.len() as u64,
            b"",
            (0, 0),
            &Opts::default(),
        );
        self.data(&payload);
        self
    }

    /// Appends any entry. Long names and links get GNU `L`/`K` records first.
    pub fn entry(
        &mut self,
        name: &[u8],
        typeflag: u8,
        data: &[u8],
        link: &[u8],
        dev: (u32, u32),
        o: &Opts,
    ) -> &mut Self {
        if !o.pax.is_empty() {
            self.pax_header(&o.pax.clone());
        }
        if name.len() > 100 {
            let mut v = name.to_vec();
            v.push(0);
            self.header(b"././@LongLink", b'L', v.len() as u64, b"", (0, 0), &Opts::default());
            self.data(&v);
        }
        if link.len() > 100 {
            let mut v = link.to_vec();
            v.push(0);
            self.header(b"././@LongLink", b'K', v.len() as u64, b"", (0, 0), &Opts::default());
            self.data(&v);
        }
        self.header(name, typeflag, data.len() as u64, link, dev, o);
        self.data(data);
        self
    }

    pub fn dir(&mut self, path: &str, o: &Opts) -> &mut Self {
        self.entry(path.as_bytes(), b'5', b"", b"", (0, 0), o)
    }
    pub fn file(&mut self, path: &str, data: &[u8], o: &Opts) -> &mut Self {
        self.entry(path.as_bytes(), b'0', data, b"", (0, 0), o)
    }
    pub fn symlink(&mut self, path: &str, target: &str, o: &Opts) -> &mut Self {
        self.entry(path.as_bytes(), b'2', b"", target.as_bytes(), (0, 0), o)
    }
    pub fn hardlink(&mut self, path: &str, target: &str) -> &mut Self {
        self.entry(path.as_bytes(), b'1', b"", target.as_bytes(), (0, 0), &Opts::default())
    }
    pub fn chardev(&mut self, path: &str, major: u32, minor: u32, o: &Opts) -> &mut Self {
        self.entry(path.as_bytes(), b'3', b"", b"", (major, minor), o)
    }
    pub fn fifo(&mut self, path: &str, o: &Opts) -> &mut Self {
        self.entry(path.as_bytes(), b'6', b"", b"", (0, 0), o)
    }
    /// Whiteout hiding `path` (e.g. `etc/foo` → `etc/.wh.foo`).
    pub fn whiteout(&mut self, path: &str) -> &mut Self {
        let marker = match path.rsplit_once('/') {
            Some((dir, name)) => format!("{dir}/.wh.{name}"),
            None => format!(".wh.{path}"),
        };
        self.entry(marker.as_bytes(), b'0', b"", b"", (0, 0), &Opts::default())
    }
    /// Opaque marker for `dir` (`""` is the layer root).
    pub fn opaque(&mut self, dir: &str) -> &mut Self {
        let marker = if dir.is_empty() {
            ".wh..wh..opq".to_string()
        } else {
            format!("{dir}/.wh..wh..opq")
        };
        self.entry(marker.as_bytes(), b'0', b"", b"", (0, 0), &Opts::default())
    }
    /// The entries so far, without the end-of-archive marker.
    pub fn bytes(&self) -> Vec<u8> {
        self.buf.clone()
    }
    /// The entries plus the two zero blocks that end an archive.
    pub fn finish(&self) -> Vec<u8> {
        let mut v = self.buf.clone();
        v.resize(v.len() + 1024, 0);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pax_record_length_is_self_consistent() {
        let cases: [&[u8]; 4] = [b"", b"x", &[b'y'; 95], &[b'z'; 995]];
        for v in cases {
            let rec = pax_record(b"path", v);
            let sp = rec.iter().position(|&b| b == b' ').unwrap();
            let len: usize = std::str::from_utf8(&rec[..sp]).unwrap().parse().unwrap();
            assert_eq!(len, rec.len());
        }
    }

    #[test]
    fn builds_readable_tar() {
        let tar = TarBuilder::new()
            .dir("etc", &Opts::default().mode(0o755))
            .file("etc/hostname", b"kiln\n", &Opts::default())
            .finish();
        assert_eq!(tar.len() % 512, 0);
        let mut ar = tar::Archive::new(&tar[..]);
        let names: Vec<String> = ar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect();
        assert_eq!(names, vec!["etc", "etc/hostname"]);
    }
}
