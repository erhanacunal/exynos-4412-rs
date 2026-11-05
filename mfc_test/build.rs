// SPDX-License-Identifier: GPL-2.0-or-later
//! Puts `memory.x` on the linker search path, and brings in the MFC's firmware.
//!
//! The firmware, Samsung's `s5p-mfc.fw`, is not in this repository. Set `MFC_FIRMWARE` to its
//! path. Without it the image is built with an empty file, and the test stops before powering
//! the codec.

use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    fs::copy("memory.x", out.join("memory.x")).expect("copy memory.x into OUT_DIR");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=MFC_FIRMWARE");
    match env::var_os("MFC_FIRMWARE").map(PathBuf::from) {
        Some(path) => {
            println!("cargo:rerun-if-changed={}", path.display());
            fs::copy(&path, out.join("s5p-mfc.fw")).unwrap_or_else(|error| panic!("{}: {}", path.display(), error));
        }
        None => fs::write(out.join("s5p-mfc.fw"), []).expect("write an empty file"),
    }
}
