//! Puts `memory.x` on the linker search path, and embeds the WAV samples found.
//!
//! The samples are alsa-utils' `speaker-test` voices, which are not in this repository: they
//! come with alsa-utils, under its own license. They are taken from `AUDIO_SAMPLES_DIR`, by
//! default the buildroot tree beside this one. Without them the test builds all the same,
//! with no samples.

use std::{env, fs, path::PathBuf};

const DEFAULT_SAMPLES_DIR: &str = "../../../buildroot/output/build/alsa-utils-1.2.15.1/speaker-test/samples";
const NAMES: [&str; 9] = [
    "Front_Left",
    "Front_Right",
    "Front_Center",
    "Rear_Left",
    "Rear_Right",
    "Rear_Center",
    "Side_Left",
    "Side_Right",
    "Noise",
];

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    fs::copy("memory.x", out.join("memory.x")).expect("copy memory.x into OUT_DIR");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=AUDIO_SAMPLES_DIR");

    let dir = env::var_os("AUDIO_SAMPLES_DIR").map_or_else(|| PathBuf::from(DEFAULT_SAMPLES_DIR), PathBuf::from);
    let mut list = String::from("/// The samples embedded at build time: name and file.\npub static SAMPLES: &[(&str, &[u8])] = &[\n");
    for name in NAMES {
        let source = dir.join(format!("{name}.wav"));
        println!("cargo:rerun-if-changed={}", source.display());
        let target = out.join(format!("{name}.wav"));
        if fs::copy(&source, &target).is_ok() {
            list.push_str(&format!("    (\"{name}\", include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{name}.wav\"))),\n"));
        }
    }
    list.push_str("];\n");
    fs::write(out.join("samples.rs"), list).expect("write samples.rs");
}
