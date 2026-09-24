use std::fs;
use std::path::{Path, PathBuf};

const ENGINE_INPUTS: [(&str, &str); 7] = [
    ("Cargo.toml", ""),
    ("Cargo.lock", ""),
    ("kernel/Cargo.toml", ""),
    ("kernel/src", ".rs"),
    ("engine-py/Cargo.toml", ""),
    ("engine-py/src", ".rs"),
    ("engine-py/python", ".py"),
];

fn collect(dir: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "bin") {
                continue;
            }
            collect(&path, suffix, out);
        } else if path.to_string_lossy().ends_with(suffix) {
            out.push(path);
        }
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .expect("engine-py sits in the workspace root");

    let mut files = Vec::new();
    for (input, suffix) in ENGINE_INPUTS {
        let path = root.join(input);
        if suffix.is_empty() {
            if path.is_file() {
                files.push(path);
            }
        } else {
            collect(&path, suffix, &mut files);
        }
    }
    let mut inputs: Vec<(String, PathBuf)> = files
        .into_iter()
        .map(|path| {
            let rel = path
                .strip_prefix(root)
                .expect("under the root")
                .to_string_lossy()
                .replace('\\', "/");
            (rel, path)
        })
        .collect();
    inputs.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, path) in &inputs {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let mut blob = Vec::new();
    for (rel, path) in &inputs {
        blob.extend_from_slice(rel.as_bytes());
        blob.push(0);
        blob.extend_from_slice(&fs::read(path).unwrap_or_else(|e| panic!("read {rel}: {e}")));
        blob.push(0);
    }
    println!("cargo:rustc-env=ENGINE_PY_SOURCE_CRC={:08x}", crc32(&blob));
    println!(
        "cargo:rustc-env=ENGINE_PY_PROFILE={}",
        std::env::var("PROFILE").expect("PROFILE")
    );
}
