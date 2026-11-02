//! Puts `link.x` on the linker search path of every crate that depends on us.

use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    fs::copy("link.x", out.join("link.x")).expect("copy link.x into OUT_DIR");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=link.x");
    println!("cargo:rerun-if-changed=build.rs");
}
