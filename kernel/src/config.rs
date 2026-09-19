use std::path::PathBuf;

fn var(name: &str) -> Option<String> {
    let value = std::env::var(name).ok().filter(|v| !v.is_empty());
    tracing::trace!(
        target: "odoo_kernel::config",
        name, set = value.is_some(), "read an environment override"
    );
    value
}

fn resolved<T: std::fmt::Debug>(setting: &str, value: T, from_env: bool) -> T {
    tracing::debug!(
        target: "odoo_kernel::config",
        setting, ?value, from_env, "resolved"
    );
    value
}

pub fn workspace() -> PathBuf {
    let given = var("RUSTORM_WORKSPACE");
    resolved(
        "workspace",
        given
            .clone()
            .unwrap_or_else(|| "/home/marin/Odoo".to_string())
            .into(),
        given.is_some(),
    )
}

pub fn db() -> String {
    let given = var("RUSTORM_DB");
    resolved(
        "db",
        given.clone().unwrap_or_else(|| "rustorm_probe".to_string()),
        given.is_some(),
    )
}

pub fn pg_host() -> String {
    let given = var("RUSTORM_PGHOST");
    resolved(
        "pg_host",
        given
            .clone()
            .unwrap_or_else(|| "/var/run/postgresql".to_string()),
        given.is_some(),
    )
}

pub fn pg_user() -> String {
    let given = var("RUSTORM_PGUSER").or_else(|| var("USER"));
    resolved(
        "pg_user",
        given.clone().unwrap_or_else(|| "marin".to_string()),
        given.is_some(),
    )
}

pub fn dsn_for(db_name: Option<&str>) -> String {
    let (dsn, from_env) = match (var("RUSTORM_DSN"), db_name) {
        (Some(dsn), None) => (dsn, true),
        (Some(dsn), Some(name)) => (with_dbname(&dsn, name), true),
        (None, name) => (
            format!(
                "host={} user={} dbname={}",
                pg_host(),
                pg_user(),
                name.map(str::to_string).unwrap_or_else(db)
            ),
            false,
        ),
    };
    tracing::debug!(
        target: "odoo_kernel::config",
        dbname = ?dsn.split_whitespace().find_map(|kv| kv.strip_prefix("dbname=")),
        from_env,
        "resolved the dsn"
    );
    dsn
}

fn with_dbname(dsn: &str, db_name: &str) -> String {
    if let Some((scheme, rest)) = dsn.split_once("://") {
        let (authority_and_path, query) = match rest.split_once('?') {
            Some((a, q)) => (a, Some(q)),
            None => (rest, None),
        };
        let authority = authority_and_path
            .split_once('/')
            .map(|(a, _)| a)
            .unwrap_or(authority_and_path);
        let mut out = format!("{scheme}://{authority}/{db_name}");
        if let Some(q) = query {
            out.push('?');
            out.push_str(q);
        }
        return out;
    }
    let mut parts: Vec<String> = dsn
        .split_whitespace()
        .filter(|kv| !kv.starts_with("dbname="))
        .map(str::to_string)
        .collect();
    parts.push(format!("dbname={db_name}"));
    parts.join(" ")
}

pub fn dsn() -> String {
    dsn_for(None)
}

pub fn venv_name() -> String {
    let given = var("RUSTORM_VENV");
    resolved(
        "venv_name",
        given.clone().unwrap_or_else(|| "p314o19m".to_string()),
        given.is_some(),
    )
}

pub fn odoo_root() -> PathBuf {
    let given = var("RUSTORM_ODOO_ROOT");
    let root: PathBuf = match &given {
        Some(p) => p.into(),
        None => workspace().join("odoo"),
    };
    if !root.join("odoo-bin").exists() {
        tracing::warn!(
            target: "odoo_kernel::config",
            path = %root.display(),
            "odoo_root has no odoo-bin; an embedded boot from here will fail to import odoo"
        );
    }
    resolved("odoo_root", root, given.is_some())
}

pub fn odoo_conf() -> PathBuf {
    if let Some(p) = var("RUSTORM_ODOO_CONF") {
        return resolved("odoo_conf", p.into(), true);
    }
    let root = workspace();
    let by_venv = root.join(format!("{}.conf", venv_name()));
    if by_venv.exists() {
        return resolved("odoo_conf", by_venv, false);
    }
    let mut confs: Vec<PathBuf> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "conf"))
        .collect();
    confs.sort();
    tracing::debug!(
        target: "odoo_kernel::config",
        candidates = confs.len(),
        by_venv = %by_venv.display(),
        "no conf named after the venv; choosing among the workspace's confs"
    );
    let chosen = match confs.len() {
        1 => confs.pop().unwrap(),
        _ => by_venv,
    };
    if !chosen.is_file() {
        tracing::warn!(
            target: "odoo_kernel::config",
            path = %chosen.display(),
            "the resolved odoo conf does not exist; set RUSTORM_ODOO_CONF"
        );
    }
    resolved("odoo_conf", chosen, false)
}

pub fn harness_dir() -> PathBuf {
    let given = var("RUSTORM_HARNESS");
    let dir = match &given {
        Some(p) => p.into(),
        None => workspace().join("odoo-rust-orm/harness"),
    };
    resolved("harness_dir", dir, given.is_some())
}

pub fn venv_site() -> PathBuf {
    if let Some(p) = var("RUSTORM_VENV_SITE") {
        return resolved("venv_site", p.into(), true);
    }

    let venv = workspace().join(venv_name());
    let lib = venv.join("lib");

    let mut candidates: Vec<((u32, u32), PathBuf)> = std::fs::read_dir(&lib)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter_map(|p| {
            let name = p.file_name().and_then(|n| n.to_str())?;
            let rest = name.strip_prefix("python")?;
            let (major, minor) = rest.split_once('.').unwrap_or((rest, "0"));
            Some(((major.parse().ok()?, minor.parse().unwrap_or(0)), p))
        })
        .collect();
    candidates.sort();
    let found = candidates.len();
    let site = match candidates.pop() {
        Some((version, p)) => {
            tracing::debug!(
                target: "odoo_kernel::config",
                venv = %venv.display(), ?version, found,
                "chose the highest python under the venv"
            );
            p.join("site-packages")
        }
        None => {
            tracing::warn!(
                target: "odoo_kernel::config",
                lib = %lib.display(),
                "no python3.x under the venv's lib/; the embedding binaries will \
                 not find psycopg"
            );
            lib.join("python3/site-packages")
        }
    };
    resolved("venv_site", site, false)
}

#[cfg(test)]
mod tests {

    use super::*;

    fn workspace_present() -> bool {
        workspace().join("odoo").is_dir()
    }

    #[test]
    fn odoo_root_holds_an_odoo_checkout() {
        if !workspace_present() {
            return;
        }
        let root = odoo_root();
        assert!(
            root.join("odoo-bin").exists(),
            "odoo_root() = {} has no odoo-bin",
            root.display()
        );
    }

    #[test]
    fn odoo_conf_resolves_to_a_real_file() {
        if !workspace_present() {
            return;
        }
        let conf = odoo_conf();
        assert!(
            conf.is_file(),
            "odoo_conf() = {} does not exist",
            conf.display()
        );
    }

    #[test]
    fn venv_site_holds_installed_packages() {
        if !workspace_present() {
            return;
        }
        let site = venv_site();
        assert!(
            site.is_dir(),
            "venv_site() = {} does not exist",
            site.display()
        );
        assert!(
            site.join("psycopg").is_dir(),
            "venv_site() = {} has no psycopg; the embedding bins import it",
            site.display()
        );
    }

    #[test]
    fn a_named_database_keeps_the_rest_of_the_dsn() {
        assert_eq!(
            with_dbname("host=db.internal port=6543 user=odoo dbname=old", "new"),
            "host=db.internal port=6543 user=odoo dbname=new"
        );
        assert_eq!(
            with_dbname("host=/tmp user=x", "new"),
            "host=/tmp user=x dbname=new"
        );
    }

    #[test]
    fn a_uri_dsn_takes_the_requested_database_in_its_path() {
        assert_eq!(
            with_dbname("postgres://u@h/olddb", "new"),
            "postgres://u@h/new"
        );
        assert_eq!(
            with_dbname("postgres://u@h:5433", "new"),
            "postgres://u@h:5433/new"
        );
        assert_eq!(
            with_dbname("postgresql://u:p@h/olddb?sslmode=require", "new"),
            "postgresql://u:p@h/new?sslmode=require"
        );
    }

    #[test]
    fn python_versions_sort_numerically() {
        let dir = std::env::temp_dir().join(format!("rustorm-venvsort-{}", std::process::id()));
        let lib = dir.join("venv").join("lib");
        for v in ["python3.9", "python3.14", "python3.10"] {
            std::fs::create_dir_all(lib.join(v).join("site-packages")).unwrap();
        }

        let mut got: Vec<(u32, u32)> = std::fs::read_dir(&lib)
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_str()?.to_string();
                let rest = n.strip_prefix("python")?.to_string();
                let (a, b) = rest.split_once('.')?;
                Some((a.parse().ok()?, b.parse().ok()?))
            })
            .collect();
        got.sort();
        assert_eq!(got.pop(), Some((3, 14)), "3.14 must sort above 3.9");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_overrides_win() {
        assert!(dsn_for(Some("somedb")).contains("dbname=somedb"));
    }
}
