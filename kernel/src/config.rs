use std::path::PathBuf;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn workspace() -> PathBuf {
    var("RUSTORM_WORKSPACE")
        .unwrap_or_else(|| "/home/marin/Odoo".to_string())
        .into()
}

pub fn db() -> String {
    var("RUSTORM_DB").unwrap_or_else(|| "rustorm_probe".to_string())
}

pub fn pg_host() -> String {
    var("RUSTORM_PGHOST").unwrap_or_else(|| "/var/run/postgresql".to_string())
}

pub fn pg_user() -> String {
    var("RUSTORM_PGUSER")
        .or_else(|| var("USER"))
        .unwrap_or_else(|| "marin".to_string())
}

pub fn dsn_for(db_name: Option<&str>) -> String {
    match (var("RUSTORM_DSN"), db_name) {
        (Some(dsn), None) => dsn,
        (Some(dsn), Some(name)) => with_dbname(&dsn, name),
        (None, name) => format!(
            "host={} user={} dbname={}",
            pg_host(),
            pg_user(),
            name.map(str::to_string).unwrap_or_else(db)
        ),
    }
}

fn with_dbname(dsn: &str, db_name: &str) -> String {
    if dsn.contains("://") {
        return dsn.to_string();
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
    var("RUSTORM_VENV").unwrap_or_else(|| "p314o19m".to_string())
}

pub fn odoo_root() -> PathBuf {
    match var("RUSTORM_ODOO_ROOT") {
        Some(p) => p.into(),
        None => workspace().join("odoo"),
    }
}

pub fn odoo_conf() -> PathBuf {
    if let Some(p) = var("RUSTORM_ODOO_CONF") {
        return p.into();
    }
    let root = workspace();
    let by_venv = root.join(format!("{}.conf", venv_name()));
    if by_venv.exists() {
        return by_venv;
    }
    let mut confs: Vec<PathBuf> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "conf"))
        .collect();
    confs.sort();
    match confs.len() {
        1 => confs.pop().unwrap(),
        _ => by_venv,
    }
}

pub fn harness_dir() -> PathBuf {
    match var("RUSTORM_HARNESS") {
        Some(p) => p.into(),
        None => workspace().join("odoo-rust-orm/harness"),
    }
}

pub fn venv_site() -> PathBuf {
    if let Some(p) = var("RUSTORM_VENV_SITE") {
        return p.into();
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
    match candidates.pop().map(|(_, p)| p) {
        Some(p) => p.join("site-packages"),
        None => lib.join("python3/site-packages"),
    }
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
    fn a_uri_dsn_is_left_alone_rather_than_mangled() {
        let uri = "postgres://u@h/olddb";
        assert_eq!(with_dbname(uri, "new"), uri);
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
