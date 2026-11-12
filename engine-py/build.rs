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
    // The unit-test binary and the probe tools embed the interpreter pyo3 built
    // against; without a run path they load whichever libpython of the same
    // name the system resolves first, another release with another stdlib.
    // Loaded into that same interpreter, the extension finds it either way.
    if let Some(lib_dir) = pyo3_build_config::get().lib_dir() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{lib_dir}");
    }
}
