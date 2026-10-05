//! `kiln-init`: PID 1 of a kiln guest (spec §9.6). Build it static for
//! `aarch64-unknown-linux-musl` or `x86_64-unknown-linux-musl`.

#[cfg(target_os = "linux")]
fn main() {
    kiln_init::linux::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("kiln-init is PID 1 of a kiln Linux guest; it does not run on this system");
    std::process::exit(1);
}
