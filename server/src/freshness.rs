use std::fs;
use std::path::{Path, PathBuf};

pub const SKIP_ENV: &str = "RUSTORM_SKIP_FRESHNESS_CHECK";
const WORKSPACE_INPUTS: [&str; 2] = ["Cargo.toml", "Cargo.lock"];
pub const LINKED_CRATES: [&str; 2] = ["kernel", "server"];
const CRATE_INPUTS: [&str; 2] = ["Cargo.toml", "build.rs"];

fn collect_rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "bin") {
                continue;
            }
            collect_rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

pub fn source_inputs(root: &Path) -> Vec<(String, PathBuf)> {
    let mut files: Vec<PathBuf> = WORKSPACE_INPUTS
        .iter()
        .map(|name| root.join(name))
        .collect();
    for krate in LINKED_CRATES {
        let dir = root.join(krate);
        files.extend(CRATE_INPUTS.iter().map(|name| dir.join(name)));
        collect_rust_sources(&dir.join("src"), &mut files);
    }
    let mut inputs: Vec<(String, PathBuf)> = files
        .into_iter()
        .filter(|path| path.is_file())
        .filter_map(|path| {
            let rel = path
                .strip_prefix(root)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            Some((rel, path))
        })
        .collect();
    inputs.sort_by(|a, b| a.0.cmp(&b.0));
    inputs
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

pub fn source_crc(root: &Path) -> std::io::Result<String> {
    let mut blob = Vec::new();
    for (rel, path) in source_inputs(root) {
        blob.extend_from_slice(rel.as_bytes());
        blob.push(0);
        blob.extend_from_slice(&fs::read(&path)?);
        blob.push(0);
    }
    Ok(format!("{:08x}", crc32(&blob)))
}

pub fn stale_build(built: &str, root: &Path, binary: &str) -> Option<String> {
    if !root.join("server/build.rs").is_file() {
        return None;
    }
    let rebuild = format!(
        "rebuild it with `cargo build --release -p odoo-server --manifest-path {}`, or set {SKIP_ENV}=1",
        root.join("Cargo.toml").display()
    );
    match source_crc(root) {
        Ok(current) if current == built => None,
        Ok(current) => Some(format!(
            "{binary} is stale: it was compiled from Rust sources with crc {built}, but the \
             sources under {} are crc {current}, so the SQL it generates is not the SQL this \
             checkout's kernel generates; {rebuild}",
            root.display()
        )),
        Err(e) => Some(format!(
            "{binary} cannot checksum the Rust sources under {}: {e}; {rebuild}",
            root.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("server sits in the workspace root")
            .to_path_buf()
    }

    fn copy_checkout(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after the epoch")
            .as_nanos();
        let dest = std::env::temp_dir().join(format!(
            "rustorm-freshness-{label}-{}-{nanos}",
            std::process::id()
        ));
        let root = checkout();
        let mut files: Vec<PathBuf> = source_inputs(&root).into_iter().map(|(_, p)| p).collect();
        files.extend(["README.md", "engine-py/src/lib.rs"].map(|rel| root.join(rel)));
        for source in files {
            let target = dest.join(source.strip_prefix(&root).expect("under the checkout"));
            fs::create_dir_all(target.parent().expect("a parent")).expect("mkdir");
            fs::copy(&source, &target).expect("copy");
        }
        dest
    }

    fn append(path: &Path) {
        fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        let mut text = fs::read(path).unwrap_or_default();
        text.push(b'\n');
        fs::write(path, text).expect("write");
    }

    #[test]
    fn the_build_stamp_is_the_checkouts_checksum() {
        assert_eq!(
            env!("RUSTORM_SOURCE_CRC"),
            source_crc(&checkout()).expect("checksum")
        );
    }

    #[test]
    fn only_what_is_compiled_into_rustorm_stales_it() {
        for (rel, stales) in [
            ("kernel/src/sqlgen.rs", true),
            ("kernel/src/registry.rs", true),
            ("server/src/main.rs", true),
            ("server/build.rs", true),
            ("kernel/Cargo.toml", true),
            ("Cargo.lock", true),
            ("README.md", false),
            ("engine-py/src/lib.rs", false),
            ("server/src/bin/tool.rs", false),
            ("kernel/tests/pure.rs", false),
        ] {
            let root = copy_checkout("edit");
            let built = source_crc(&root).expect("checksum");
            assert_eq!(
                stale_build(&built, &root, "rustorm"),
                None,
                "{rel} before the edit"
            );
            append(&root.join(rel));
            let stale = stale_build(&built, &root, "rustorm");
            assert_eq!(stale.is_some(), stales, "an edit to {rel}: {stale:?}");
            if let Some(message) = stale {
                assert!(
                    message.contains("cargo build --release -p odoo-server"),
                    "{message}"
                );
                assert!(message.contains(SKIP_ENV), "{message}");
            }
            fs::remove_dir_all(&root).expect("cleanup");
        }
    }

    #[test]
    fn a_binary_without_its_checkout_is_not_checked() {
        let root = copy_checkout("deployed");
        fs::remove_file(root.join("server/build.rs")).expect("remove");
        assert_eq!(stale_build("00000000", &root, "rustorm"), None);
        fs::remove_dir_all(&root).expect("cleanup");
    }
}
