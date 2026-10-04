//! The root overlay's mount data (spec §9.6 stage 3). It goes to mount(2) as one
//! string: the new mount API's `fsconfig` rejects long `lowerdir` values.

use crate::error::{Failure, Result};

/// Where app layer `n` is mounted.
pub fn layer_dir(n: usize) -> String {
    format!("/kiln/layers/{n}")
}

/// An empty directory that stands in as the only lower layer of an image without layers.
pub const EMPTY_LOWER: &str = "/kiln/empty";
pub const UPPER: &str = "/kiln/rw/upper";
pub const WORK: &str = "/kiln/rw/work";

/// mount(2) copies at most one page of data, including the terminating NUL.
const MAX_DATA: usize = 4095;

/// `lowerdir=<top…bottom>,upperdir,workdir,xino=on,redirect_dir=off,index=off,metacopy=off`.
pub fn mount_data(layers: usize) -> Result<String> {
    let lower = if layers == 0 {
        EMPTY_LOWER.to_string()
    } else {
        (0..layers).rev().map(layer_dir).collect::<Vec<_>>().join(":")
    };
    let data =
        format!("lowerdir={lower},upperdir={UPPER},workdir={WORK},xino=on,redirect_dir=off,index=off,metacopy=off");
    if data.len() > MAX_DATA {
        return Err(Failure::msg(format!(
            "overlay options for {layers} layers exceed one page ({} bytes)",
            data.len()
        )));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_layers_top_first() {
        assert_eq!(
            mount_data(3).unwrap(),
            "lowerdir=/kiln/layers/2:/kiln/layers/1:/kiln/layers/0,upperdir=/kiln/rw/upper,\
             workdir=/kiln/rw/work,xino=on,redirect_dir=off,index=off,metacopy=off"
        );
        assert!(mount_data(0).unwrap().starts_with("lowerdir=/kiln/empty,"));
    }

    #[test]
    fn the_largest_layer_count_fits_in_a_page() {
        let max = kiln_proto::MAX_LAYERS as usize;
        assert!(mount_data(max).unwrap().len() <= MAX_DATA);
        assert!(mount_data(400).is_err());
    }
}
