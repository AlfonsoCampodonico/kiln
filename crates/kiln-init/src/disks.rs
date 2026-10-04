//! Block device names: the init layer is `vda`, the scratch disk `vdb`, and app
//! layer `n` is the disk after them (spec §9.1).

/// The kernel's name for virtio disk `index` (0-based): `vda` … `vdz`, `vdaa` …,
/// as `virtblk_name_format` builds it.
pub fn disk_name(index: usize) -> String {
    let mut letters = Vec::new();
    let mut i = index as i64;
    loop {
        letters.push(b'a' + (i % 26) as u8);
        i = i / 26 - 1;
        if i < 0 {
            break;
        }
    }
    letters.reverse();
    format!("vd{}", String::from_utf8(letters).expect("ASCII"))
}

/// The scratch disk.
pub const SCRATCH: &str = "/dev/vdb";

/// App layer `n` (0 is the lowest).
pub fn layer_device(n: usize) -> String {
    format!("/dev/{}", disk_name(n + 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_kernel() {
        assert_eq!(disk_name(0), "vda");
        assert_eq!(disk_name(25), "vdz");
        assert_eq!(disk_name(26), "vdaa");
        assert_eq!(disk_name(27), "vdab");
        assert_eq!(disk_name(51), "vdaz");
        assert_eq!(disk_name(52), "vdba");
        assert_eq!(disk_name(701), "vdzz");
        assert_eq!(disk_name(702), "vdaaa");
        assert_eq!(layer_device(0), "/dev/vdc");
        assert_eq!(SCRATCH, format!("/dev/{}", disk_name(1)));
    }
}
