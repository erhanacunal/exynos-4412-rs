//! Puts `memory.x` on the linker search path, and brings in the WiFi chip's firmware.
//!
//! WiFi is optional. Without `WIFI_FIRMWARE_DIR` (the directory that holds the firmware and NVRAM
//! files, not in this repository), `WIFI_SSID` and `WIFI_PASSWORD`, the image is built with empty
//! files and takes images over the UART only. The network's name and passphrase go into the
//! image, so it must not be published. `WIFI_BAND`, `2.4` or `5`, limits joining to access points
//! of that band.

use std::{env, fs, path::PathBuf};

const FILES: [(&str, &str); 2] = [
    ("brcmfmac4334-sdio.samsung,mehmet-board.bin", "firmware.bin"),
    ("brcmfmac4334-sdio.samsung,mehmet-board.txt", "nvram.txt"),
];

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    fs::copy("memory.x", out.join("memory.x")).expect("copy memory.x into OUT_DIR");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=WIFI_FIRMWARE_DIR");

    for name in ["WIFI_SSID", "WIFI_PASSWORD", "WIFI_BAND"] {
        println!("cargo:rerun-if-env-changed={name}");
        if let Ok(value) = env::var(name) {
            println!("cargo:rustc-env={name}={value}");
        }
    }

    let dir = env::var_os("WIFI_FIRMWARE_DIR").map(PathBuf::from);
    for (name, output) in FILES {
        match &dir {
            Some(dir) => {
                let source = dir.join(name);
                println!("cargo:rerun-if-changed={}", source.display());
                fs::copy(&source, out.join(output))
                    .unwrap_or_else(|error| panic!("{}: {}", source.display(), error));
            }
            None => fs::write(out.join(output), []).expect("write an empty file"),
        }
    }
}
