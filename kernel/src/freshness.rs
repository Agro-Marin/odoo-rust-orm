use std::fs;
use std::path::{Path, PathBuf};

pub const SKIP_ENV: &str = "RUSTORM_SKIP_FRESHNESS_CHECK";
const WORKSPACE_INPUTS: [&str; 2] = ["Cargo.toml", "Cargo.lock"];
const CRATE_INPUTS: [&str; 2] = ["Cargo.toml", "build.rs"];

pub struct Stamp {
    pub owner: &'static str,
    pub bins: bool,
    pub rebuild: &'static str,
}

pub const SERVER: Stamp = Stamp {
    owner: "server",
    bins: false,
    rebuild: "cargo build --release -p odoo-server",
};

pub const ENGINE_PY: Stamp = Stamp {
    owner: "engine-py",
    bins: false,
    rebuild: "harness/install_engine.sh",
};

pub const ENGINE_PY_BINS: Stamp = Stamp {
    owner: "engine-py",
    bins: true,
    rebuild: "cargo build --release -p odoo-engine-py --bins",
};

fn collect_rust_sources(dir: &Path, bins: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !bins && path.file_name().is_some_and(|name| name == "bin") {
                continue;
            }
            collect_rust_sources(&path, bins, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

pub fn skip_requested() -> bool {
    std::env::var_os(SKIP_ENV).is_some_and(|v| !v.is_empty())
}

impl Stamp {
    pub fn crates(&self) -> [&'static str; 2] {
        ["kernel", self.owner]
    }

    pub fn source_inputs(&self, root: &Path) -> Vec<(String, PathBuf)> {
        let mut files: Vec<PathBuf> = WORKSPACE_INPUTS
            .iter()
            .map(|name| root.join(name))
            .collect();
        for krate in self.crates() {
            let dir = root.join(krate);
            files.extend(CRATE_INPUTS.iter().map(|name| dir.join(name)));
            collect_rust_sources(&dir.join("src"), self.bins, &mut files);
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

    pub fn source_crc(&self, root: &Path) -> std::io::Result<String> {
        let mut blob = Vec::new();
        for (rel, path) in self.source_inputs(root) {
            blob.extend_from_slice(rel.as_bytes());
            blob.push(0);
            blob.extend_from_slice(&fs::read(&path)?);
            blob.push(0);
        }
        Ok(format!("{:08x}", crc32(&blob)))
    }

    pub fn emit_build_stamp(&self, root: &Path, env: &str) {
        for krate in self.crates() {
            println!(
                "cargo:rerun-if-changed={}",
                root.join(krate).join("src").display()
            );
        }
        for (_, path) in self.source_inputs(root) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        let crc = self
            .source_crc(root)
            .unwrap_or_else(|e| panic!("checksum the sources for {env}: {e}"));
        println!("cargo:rustc-env={env}={crc}");
    }

    pub fn stale_build(&self, built: &str, root: &Path, binary: &str) -> Option<String> {
        if !root.join(self.owner).join("build.rs").is_file() {
            return None;
        }
        let rebuild = format!(
            "rebuild it with `{} --manifest-path {}`, or set {SKIP_ENV}=1",
            self.rebuild,
            root.join("Cargo.toml").display()
        );
        match self.source_crc(root) {
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

    pub fn exit_if_stale(&self, built: &str, manifest_dir: &str) {
        if skip_requested() {
            return;
        }
        let binary = std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| self.owner.into());
        let root = Path::new(manifest_dir)
            .parent()
            .expect("a linked crate sits in the workspace root");
        if let Some(message) = self.stale_build(built, root, &binary) {
            eprintln!("{message}");
            std::process::exit(2);
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

#[cfg(test)]
mod tests {
    use super::*;

    const STAMPS: [(&str, &Stamp); 3] = [
        ("server", &SERVER),
        ("engine-py", &ENGINE_PY),
        ("engine-py bins", &ENGINE_PY_BINS),
    ];

    fn checkout() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("kernel sits in the workspace root")
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
        let mut files: Vec<PathBuf> = STAMPS
            .iter()
            .flat_map(|(_, stamp)| stamp.source_inputs(&root))
            .map(|(_, p)| p)
            .collect();
        files.push(root.join("README.md"));
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
    fn only_what_is_compiled_into_a_binary_stales_it() {
        for (rel, stales) in [
            ("kernel/src/sqlgen.rs", [true, true, true]),
            ("kernel/src/registry.rs", [true, true, true]),
            ("kernel/Cargo.toml", [true, true, true]),
            ("Cargo.lock", [true, true, true]),
            ("server/src/main.rs", [true, false, false]),
            ("server/build.rs", [true, false, false]),
            ("engine-py/src/lib.rs", [false, true, true]),
            ("engine-py/build.rs", [false, true, true]),
            ("engine-py/src/bin/export_registry.rs", [false, false, true]),
            ("server/src/bin/tool.rs", [false, false, false]),
            ("kernel/tests/pure.rs", [false, false, false]),
            ("README.md", [false, false, false]),
        ] {
            let root = copy_checkout("edit");
            let built: Vec<String> = STAMPS
                .iter()
                .map(|(_, stamp)| stamp.source_crc(&root).expect("checksum"))
                .collect();
            append(&root.join(rel));
            for (((name, stamp), built), stales) in STAMPS.iter().zip(&built).zip(stales) {
                let stale = stamp.stale_build(built, &root, name);
                assert_eq!(
                    stale.is_some(),
                    stales,
                    "{name} after an edit to {rel}: {stale:?}"
                );
                if let Some(message) = stale {
                    assert!(message.contains(stamp.rebuild), "{message}");
                    assert!(message.contains(SKIP_ENV), "{message}");
                }
            }
            fs::remove_dir_all(&root).expect("cleanup");
        }
    }

    #[test]
    fn a_binary_without_its_checkout_is_not_checked() {
        let root = copy_checkout("deployed");
        for (name, stamp) in STAMPS {
            fs::remove_file(root.join(stamp.owner).join("build.rs")).ok();
            assert_eq!(stamp.stale_build("00000000", &root, name), None, "{name}");
        }
        fs::remove_dir_all(&root).expect("cleanup");
    }
}
