/// Resource limits for converting one untrusted layer (spec §7.6, threat T3).
///
/// Per-image byte limits and the expansion-ratio limit need the compressed size
/// and are enforced by the pipeline (M1b), not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Maximum uncompressed tar bytes consumed for one layer.
    pub max_layer_bytes: u64,
    /// Maximum filesystem entries in one layer.
    pub max_entries: u64,
    /// Maximum size of one PAX header, GNU long name or link, or xattr value.
    pub max_header_record: u64,
    /// Maximum path length in bytes.
    pub max_path_len: usize,
    /// Maximum number of path components.
    pub max_path_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_layer_bytes: 16 << 30,
            max_entries: 2_000_000,
            max_header_record: 1 << 20,
            max_path_len: 4096,
            max_path_depth: 256,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let l = Limits::default();
        assert_eq!(l.max_layer_bytes, 16 << 30);
        assert_eq!(l.max_entries, 2_000_000);
        assert_eq!(l.max_header_record, 1 << 20);
        assert_eq!(l.max_path_len, 4096);
        assert_eq!(l.max_path_depth, 256);
    }
}
