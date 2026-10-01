//! The in-memory filesystem tree built from one layer.

/// Seconds and nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp {
    pub sec: i64,
    pub nsec: u32,
}
