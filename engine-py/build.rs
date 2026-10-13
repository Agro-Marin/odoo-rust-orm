#[allow(dead_code)]
#[path = "../kernel/src/freshness.rs"]
mod freshness;

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .expect("engine-py sits in the workspace root");
    freshness::ENGINE_PY.emit_build_stamp(root, "ENGINE_PY_SOURCE_CRC");
    freshness::ENGINE_PY_BINS.emit_build_stamp(root, "ENGINE_PY_BINS_SOURCE_CRC");
    println!(
        "cargo:rustc-env=ENGINE_PY_PROFILE={}",
        std::env::var("PROFILE").expect("PROFILE")
    );
}
