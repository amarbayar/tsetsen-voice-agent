//! Builds the Swift photos helper on macOS, so `cargo build` is all it takes.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=helper/main.swift");
    println!("cargo:rerun-if-changed=helper/Info.plist");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let helper = out.join("tsetsen-photos");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // The Info.plist goes into the binary: macOS will not ask for Photos
        // access without its usage description.
        let status = Command::new("swiftc")
            .args(["-O", "-o"])
            .arg(&helper)
            .arg("helper/main.swift")
            .args([
                "-Xlinker",
                "-sectcreate",
                "-Xlinker",
                "__TEXT",
                "-Xlinker",
                "__info_plist",
            ])
            .args(["-Xlinker", "helper/Info.plist"])
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => println!(
                "cargo:warning=building the photos helper failed ({s}); photos will be off"
            ),
            Err(e) => println!("cargo:warning=swiftc not found ({e}); photos will be off"),
        }
    }
    println!("cargo:rustc-env=TSETSEN_PHOTOS_HELPER={}", helper.display());
}
