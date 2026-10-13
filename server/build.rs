#[allow(dead_code)]
#[path = "src/freshness.rs"]
mod freshness;

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .expect("server sits in the workspace root");
    for krate in freshness::LINKED_CRATES {
        println!(
            "cargo:rerun-if-changed={}",
            root.join(krate).join("src").display()
        );
    }
    for (_, path) in freshness::source_inputs(root) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let crc = freshness::source_crc(root).unwrap_or_else(|e| panic!("checksum the sources: {e}"));
    println!("cargo:rustc-env=RUSTORM_SOURCE_CRC={crc}");
}
