import os
import pathlib
import tempfile


def _var(name):
    value = os.environ.get(name)
    return value or None


def harness_dir():
    return (
        _var("RUSTORM_HARNESS") or pathlib.Path(pathlib.Path(__file__).resolve()).parent
    )


def root_dir():
    return pathlib.Path(harness_dir()).parent


def workspace():
    return _var("RUSTORM_WORKSPACE") or pathlib.Path(root_dir()).parent


def engine_python_dir():
    return os.path.join(root_dir(), "engine-py", "python")


def pg_host():
    return _var("RUSTORM_PGHOST") or "/var/run/postgresql"


def pg_user():
    return _var("RUSTORM_PGUSER") or _var("USER") or "postgres"


def default_db():
    return _var("RUSTORM_DB") or "rustorm_probe"


def _with_dbname(dsn, dbname):
    if "://" in dsn:
        # the path names the database, as config.rs::with_dbname rewrites it
        scheme, rest = dsn.split("://", 1)
        rest, _, query = rest.partition("?")
        authority = rest.split("/", 1)[0]
        return "%s://%s/%s%s" % (
            scheme,
            authority,
            dbname,
            "?" + query if query else "",
        )
    parts = [kv for kv in dsn.split() if not kv.startswith("dbname=")]
    parts.append("dbname=%s" % dbname)
    return " ".join(parts)


def dsn_for(dbname=None):
    dsn = _var("RUSTORM_DSN")
    if dsn and dbname is None:
        return dsn
    if dsn:
        return _with_dbname(dsn, dbname)
    return "host=%s user=%s dbname=%s" % (pg_host(), pg_user(), dbname or default_db())


def base_env(env, uid=2):
    env = env(user=uid, su=False)
    return env(context=dict(env.context, lang="en_US"))


def out_dir():
    path = _var("RUSTORM_VERIFY_OUT") or os.path.join(
        tempfile.gettempdir(), "rustorm-%d" % os.getpid()
    )
    pathlib.Path(path).mkdir(exist_ok=True, parents=True)
    return path


def out_path(name, envvar=None):
    return (_var(envvar) if envvar else None) or os.path.join(out_dir(), name)


def p50(values):
    ordered = sorted(values)
    return ordered[len(ordered) // 2]


def p95(values):
    ordered = sorted(values)
    return ordered[int(len(ordered) * 0.95) % len(ordered)]
