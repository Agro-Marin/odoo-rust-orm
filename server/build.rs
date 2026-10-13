#[allow(dead_code)]
#[path = "../kernel/src/freshness.rs"]
mod freshness;

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .expect("server sits in the workspace root");
    freshness::SERVER.emit_build_stamp(root, "RUSTORM_SOURCE_CRC");
}
