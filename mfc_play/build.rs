// SPDX-License-Identifier: GPL-2.0-or-later
//! Puts `memory.x` on the linker search path, and brings in the MFC's firmware and the stream.
//!
//! - `MFC_FIRMWARE`: Samsung's `s5p-mfc.fw`, not in this repository. Without it the image is
//!   built with an empty file and stops before powering the codec.
//! - `MFC_VIDEO`: the H.264 stream to play, in Annex B form (`tools/mp4_annexb.py` takes one out
//!   of an MP4 file). Without it, `mfc_test`'s test stream.
//! - `MFC_FPS`: pictures a second, 24 if unset.

use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    fs::copy("memory.x", out.join("memory.x")).expect("copy memory.x into OUT_DIR");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");
    for name in ["MFC_FIRMWARE", "MFC_VIDEO", "MFC_FPS"] {
        println!("cargo:rerun-if-env-changed={}", name);
    }
    match env::var_os("MFC_FIRMWARE").map(PathBuf::from) {
        Some(path) => {
            println!("cargo:rerun-if-changed={}", path.display());
            fs::copy(&path, out.join("s5p-mfc.fw")).unwrap_or_else(|error| panic!("{}: {}", path.display(), error));
        }
        None => fs::write(out.join("s5p-mfc.fw"), []).expect("write an empty file"),
    }
    let video = env::var_os("MFC_VIDEO").map_or_else(|| PathBuf::from("../mfc_test/data/pcm320x240.h264"), PathBuf::from);
    println!("cargo:rerun-if-changed={}", video.display());
    let data = fs::read(&video).unwrap_or_else(|error| panic!("{}: {}", video.display(), error));
    assert!(data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]), "{}: not Annex B (no start code); see tools/mp4_annexb.py", video.display());
    fs::write(out.join("video.h264"), data).expect("write the stream");
    let fps: u32 = env::var("MFC_FPS").map_or(24, |fps| fps.parse().expect("MFC_FPS: a number"));
    fs::write(out.join("fps.rs"), format!("{}\n", fps)).expect("write fps.rs");
}
