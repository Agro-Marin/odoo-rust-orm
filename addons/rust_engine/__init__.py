import atexit
import logging
import math
import os
import pathlib
import threading
import time
import zlib

_logger = logging.getLogger(__name__)

_STATE = {
    "db": None,
    "conninfo": None,
    "psycopg_info": None,
    "rust_db": None,
    "shims": None,
    "port": None,
    "reporter": None,
    "switch_conn": None,
}
_LOCK = threading.Lock()


_TOKIO_KEYS = frozenset(
    {
        "host",
        "hostaddr",
        "port",
        "user",
        "password",
        "dbname",
        "options",
        "application_name",
        "sslmode",
        "sslrootcert",
        "connect_timeout",
        "tcp_user_timeout",
        "keepalives",
        "keepalives_idle",
        "keepalives_interval",
        "keepalives_retries",
        "target_session_attrs",
        "channel_binding",
        "load_balance_hosts",
    }
)

_TOKIO_RENAMES = {"keepalives_count": "keepalives_retries"}

_TOKIO_HARMLESS = frozenset({"min_protocol_version"})


_SOCKET_DIRS = ("/var/run/postgresql", "/run/postgresql", "/tmp")


def _default_socket_dir():
    for path in _SOCKET_DIRS:
        if pathlib.Path(path).is_dir() and any(
            entry.name.startswith(".s.PGSQL.") for entry in pathlib.Path(path).iterdir()
        ):
            return path
    return "localhost"


class _CannotArm(Exception):
    """A configuration the kernel must not silently reinterpret."""


def _session_options(info):
    from odoo.db.pool import _prepare_connection_options
    from odoo.db.settings import current

    settings = current()
    idle_session_ms = max(900, int(settings.conn_max_idle * 1.5)) * 1000
    kwargs = {k: v for k, v in info.items() if k != "dsn"}
    return _prepare_connection_options(
        info.get("dsn", ""), kwargs, idle_session_ms, session_gucs=settings.session_gucs
    )


def _uri_with(uri, extra):
    from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit

    parts = urlsplit(uri)
    query = dict(parse_qsl(parts.query))
    for key, value in extra.items():
        key = _TOKIO_RENAMES.get(key, key)
        if key in _TOKIO_KEYS and value not in (None, "") and key not in query:
            query[key] = str(value)
    return urlunsplit(parts._replace(query=urlencode(query)))


def _connection_specs(db_name):
    from odoo.db import get_connection_info_for_database

    _, info = get_connection_info_for_database(db_name)
    _logger.debug(
        "rust_engine: connection options for %s: %s",
        db_name,
        sorted(k for k in info if k != "password"),
    )
    psycopg_info = dict(info)
    info = dict(info, options=_session_options(info))
    if "dsn" in info:
        return _uri_with(
            info["dsn"], {k: v for k, v in info.items() if k != "dsn"}
        ), psycopg_info

    sslmode = str(info.get("sslmode") or "").strip().lower()
    if sslmode == "verify-ca":
        raise _CannotArm(
            "db_sslmode is verify-ca, which checks the chain but not the host "
            "name; the rust connector offers require and verify-full only"
        )

    if not info.get("host") and not info.get("hostaddr"):
        info = dict(info, host=os.environ.get("PGHOST") or _default_socket_dir())

    parts, dropped = [], []
    for key, value in info.items():
        if value is None or value == "":
            continue
        key = _TOKIO_RENAMES.get(key, key)
        if key not in _TOKIO_KEYS:
            if key not in _TOKIO_HARMLESS:
                dropped.append(key)
            continue
        text = str(value).replace("\\", "\\\\").replace("'", "\\'")
        parts.append("%s='%s'" % (key, text))
    if dropped:
        _logger.warning(
            "rust_engine: the rust connector does not understand %s; "
            "connections it opens will not carry them",
            ", ".join(sorted(dropped)),
        )
    dsn = " ".join(parts)
    _logger.debug(
        "rust_engine: rust connector dsn carries %s",
        sorted(p.split("=", 1)[0] for p in parts if not p.startswith("password=")),
    )
    return dsn, psycopg_info


DEFAULT_TICK = 60

CHECKOUT = pathlib.Path(__file__).resolve().parents[2]
# the files `engine-py/build.rs` checksums into `__source_crc__`, in its order
SOURCE_INPUTS = (
    ("Cargo.toml", ""),
    ("Cargo.lock", ""),
    ("kernel/Cargo.toml", ""),
    ("kernel/src", ".rs"),
    ("engine-py/Cargo.toml", ""),
    ("engine-py/src", ".rs"),
    ("engine-py/python", ".py"),
)
SKIP_FRESHNESS_ENV = "RUSTORM_SKIP_FRESHNESS_CHECK"


def source_crc(root: pathlib.Path) -> str:
    files = []
    for name, suffix in SOURCE_INPUTS:
        path = root / name
        if not suffix:
            files += [path] if path.is_file() else []
        elif path.is_dir():
            files += [
                p
                for p in path.rglob("*" + suffix)
                if p.is_file() and "bin" not in p.relative_to(path).parts[:-1]
            ]
    blob = b"".join(
        rel.encode() + b"\0" + path.read_bytes() + b"\0"
        for rel, path in sorted((p.relative_to(root).as_posix(), p) for p in files)
    )
    return f"{zlib.crc32(blob):08x}"


def stale_extension(engine_py, root: pathlib.Path = CHECKOUT) -> str | None:
    """Why this `engine_py` must not serve this checkout, or None.

    The shims are compiled into the extension, so an extension built before a
    change to them imports cleanly and serves the old code: the workspace
    venv's copy once routed `web_read_group` at 3.96x Python, a defect its
    checkout had already fixed. An addon deployed without its checkout has
    nothing to compare against and is not refused.
    """
    if (
        os.environ.get(SKIP_FRESHNESS_ENV)
        or not (root / "engine-py/build.rs").is_file()
    ):
        return None
    built = getattr(engine_py, "__source_crc__", None)
    current = source_crc(root)
    where = getattr(engine_py, "__file__", "?")
    rebuild = (
        f"run {root}/harness/install_engine.sh, which builds it and installs it "
        f"into this interpreter, or set {SKIP_FRESHNESS_ENV}=1"
    )
    if built != current:
        was = (
            "predates the source stamp" if built is None else f"was built from {built}"
        )
        return f"the engine_py at {where} {was}, but {root} is {current}; {rebuild}"
    profile = getattr(engine_py, "__profile__", None)
    if profile != "release":
        return f"the engine_py at {where} is a {profile} build; {rebuild}"
    return None


# The kernel logs below DEBUG, where Python has no level. `engine_py` registers
# the name, but too late to be selected: Odoo resolves a `--log-handler
# name:LEVEL` with `getattr(logging, LEVEL, logging.INFO)` in `odoo/logutils.py`
# and runs that before any addon is imported, so `:TRACE` read as INFO and the
# finest level was unreachable from the command line.
TRACE_LEVEL = 5


def _open_trace_level() -> None:
    logging.addLevelName(TRACE_LEVEL, "TRACE")
    logging.TRACE = TRACE_LEVEL
    from odoo.tools import config

    for item in config.get("log_handler") or ():
        name, _, level = str(item).strip().partition(":")
        if level.strip().upper() == "TRACE":
            logging.getLogger(name).setLevel(TRACE_LEVEL)
            _logger.debug("rust_engine: opened %r at TRACE", name)


PARAM_MODE = "rust_engine.mode"
# a model that raised this many unexpected kernel errors is served from Python
# until reset_breaker(); refusals and divergences do not count
DEFAULT_BREAKER = 3
PARAM_SAMPLE = "rust_engine.verify_sample"


def _switch_connection():
    import psycopg

    pid = os.getpid()
    owner, conn = _STATE.get("switch_conn") or (None, None)
    if owner != pid:
        # a socket inherited across fork() belongs to the parent; closing it
        # here would send its Terminate message, so it is only dropped
        conn = None
    if conn is None or conn.closed:
        conn = psycopg.connect(**_STATE["psycopg_info"], autocommit=True)
        _STATE["switch_conn"] = (pid, conn)
    return conn


def _read_params():
    import psycopg

    conn = _switch_connection()
    try:
        with conn.cursor() as cur:
            # read over a private psycopg connection, never the rust cursor:
            # the kill switch has to work when the engine itself is the problem
            cur.execute(
                "SELECT key, value FROM ir_config_parameter WHERE key = ANY(%s)",
                ([PARAM_MODE, PARAM_SAMPLE],),
            )
            return dict(cur.fetchall())
    except psycopg.Error:
        _STATE["switch_conn"] = None
        conn.close()
        raise


def _set_mode(orm_shim, mode) -> None:
    """One switch, both layers.

    `mode` decides two things that used to be decided separately. The ORM
    shim's mode says whether reads are routed to the kernel. The db shim's
    `ACTIVE` says whether the process's connections are rust ones at all --
    and until it existed, `db_shim.install()` at post_load replaced the driver
    for EVERY cursor in the process regardless of the mode, so `off` stopped
    the routing and left the driver swap in place. That is not what "changes
    nothing until someone says otherwise" means. `off` now means off for both;
    `shadow` and `on` both need rust cursors, because the kernel runs inside
    the caller's transaction.

    Raises `ValueError` on an unknown mode, before touching either layer.
    """
    orm_shim.set_mode(mode)
    shims = _STATE["shims"]
    if shims is not None:
        shims[0].set_active(mode != "off")


def _apply_params(orm_shim, params) -> None:
    mode = (params.get(PARAM_MODE) or "").strip().lower()
    if mode and mode != orm_shim.MODE:
        try:
            _set_mode(orm_shim, mode)
            _logger.warning(
                "rust_engine: %s in the database set the routing mode to %r",
                PARAM_MODE,
                mode,
            )
        except ValueError:
            _logger.error(
                "rust_engine: %s is %r, which is not on|off|shadow; ignoring",
                PARAM_MODE,
                mode,
            )
    sample = (params.get(PARAM_SAMPLE) or "").strip()
    if sample:
        try:
            if not math.isclose(float(sample), orm_shim.SAMPLE):
                orm_shim.set_sample(float(sample))
        except ValueError as exc:
            _logger.error(
                "rust_engine: %s is %r and was ignored: %s", PARAM_SAMPLE, sample, exc
            )


def _report(orm_shim, final=False) -> None:
    snap = orm_shim.stats()
    # a refusal is the kernel declining a call it cannot answer and Python
    # answering it: routine, and not what an operator reads "error" as
    refused = snap.get("kernel_refused", 0)
    _logger.info(
        "rust kernel%s: mode=%s sample=%.3f routed=%d share=%.2f "
        "gate=%d refused=%d error=%d flush=%d verified=%d diff=%d tripped=%s "
        "quarantined=%s registry_stale=%d",
        " (final)" if final else "",
        snap["mode"],
        snap["sample"],
        snap["kernel"],
        snap["routed_share"],
        snap["fallback_gate"],
        refused,
        snap["fallback_error"] - refused,
        snap["fallback_flush"],
        snap["shadow_ok"],
        snap["shadow_diff"],
        snap["tripped"] or "-",
        snap.get("quarantined") or "-",
        snap.get("registry_stale", 0),
    )
    # The eight commonest reasons a call was NOT routed. Each distinct reason
    # is one capability to widen; `odoo.rust_kernel.gate` at DEBUG names the
    # individual calls behind each count.
    for (model, method, reason), n in snap.get("gate_reasons", ()):
        _logger.info("rust kernel gate: %5d  %s.%s: %s", n, model, method, reason)
    for model, msg in sorted(snap.get("errors", {}).items()):
        _logger.debug("rust kernel first error on %s: %s", model, msg)


def _start_reporter(orm_shim, seconds) -> None:
    if seconds <= 0:
        _logger.debug(
            "rust_engine: rust_engine_report_seconds is %r; no periodic report", seconds
        )
        return
    _logger.debug(
        "rust_engine: reporting routing stats every %d s in pid %d",
        seconds,
        os.getpid(),
    )

    def tick() -> None:
        while True:
            time.sleep(seconds)
            try:
                _apply_params(orm_shim, _read_params())
            except Exception:
                _logger.exception("rust_engine: could not read the kill switch")
            try:
                _report(orm_shim)
            except Exception:
                _logger.exception("rust_engine: the routing reporter failed")
                continue

    threading.Thread(target=tick, name="rust_engine.tick", daemon=True).start()
    # the periodic line is a snapshot; the totals a run ends on are what a
    # stage reads, so log them once more when the process exits
    atexit.register(lambda: _report(orm_shim, final=True))


def _build_kernel(registry):
    import engine_py

    orm_shim = _STATE["shims"][1]
    started = time.monotonic()
    export = engine_py.export_registry(registry)
    export_ms = (time.monotonic() - started) * 1000
    kernel = engine_py.RustKernel.build(_STATE["rust_db"], export)
    orm_shim.snapshot_orders(registry)
    orm_shim.DBNAME = _STATE["db"]
    # This runs once per worker, on the first registry load: a slow startup
    # after arming is one of these two halves, and they have different fixes.
    _logger.info(
        "rust kernel ready for %s: %d models, routing mode %r "
        "(export %.0f ms, build %.0f ms, pid %d)",
        _STATE["db"],
        kernel.model_count,
        orm_shim.MODE,
        export_ms,
        (time.monotonic() - started) * 1000 - export_ms,
        os.getpid(),
    )
    _report_here()
    return kernel


def _report_here() -> None:
    if _STATE.get("reporter") == os.getpid():
        return
    from odoo.tools import config

    _STATE["reporter"] = os.getpid()
    _start_reporter(
        _STATE["shims"][1],
        int(config.get("rust_engine_report_seconds") or DEFAULT_TICK),
    )


def _build_kernel_for(registry):
    return _build_kernel(registry)


def _arm_registry_hook() -> None:
    from odoo.orm.runtime.registry import Registry

    orig_new = Registry.new.__func__

    def new(cls, db_name, *args, **kw):
        registry = orig_new(cls, db_name, *args, **kw)
        if db_name == _STATE["db"]:
            orm_shim = _STATE["shims"][1]
            try:
                orm_shim.KERNEL = _build_kernel(registry)
                orm_shim.forget_gates()
            except Exception:
                _, orm_shim = _STATE["shims"]
                orm_shim.KERNEL = None
                _logger.exception(
                    "could not build the rust kernel for %s; "
                    "this database will be served from python",
                    db_name,
                )
        return registry

    Registry.new = classmethod(new)


def _apply_config(orm_shim, config) -> None:
    mode = str(config.get("rust_engine_mode") or "off").strip().lower()
    try:
        _set_mode(orm_shim, mode)
    except ValueError:
        _logger.error(
            "rust_engine: rust_engine_mode is %r, which is not on|off|shadow; "
            "routing stays off",
            mode,
        )
        _set_mode(orm_shim, "off")
    sample = config.get("rust_engine_verify_sample") or 0
    try:
        orm_shim.set_sample(sample)
    except (TypeError, ValueError) as exc:
        _logger.error(
            "rust_engine: rust_engine_verify_sample is %r and was ignored: %s",
            sample,
            exc,
        )
        orm_shim.set_sample(0)
    for key, attr in (("rust_engine_only", "ONLY"), ("rust_engine_except", "EXCEPT")):
        raw = config.get(key)
        if raw:
            setattr(
                orm_shim,
                attr,
                frozenset(m.strip() for m in str(raw).split(",") if m.strip()),
            )
    breaker = config.get("rust_engine_breaker")
    if breaker in (None, ""):
        breaker = DEFAULT_BREAKER
    try:
        orm_shim.BREAKER = max(0, int(breaker))
    except TypeError, ValueError:
        _logger.error(
            "rust_engine: rust_engine_breaker is %r, not an integer; using %d",
            breaker,
            DEFAULT_BREAKER,
        )
        orm_shim.BREAKER = DEFAULT_BREAKER


def _arm_port(engine_py, orm_shim, config, db_name) -> None:
    """Install the persistence port, or leave `env.backend` as the fork built it.

    Nothing here may abort `start()`. By the time it runs the db shim and the
    method routing are already installed, and the registry hook is armed only
    AFTER it: an exception here once left a server with the shims in and no
    hook to publish a kernel -- half armed, and reported only as a CRITICAL
    "Couldn't load module rust_engine" line. It was reached through an
    `engine_py` older than this addon, the venv's copy predating the port,
    which every odoo-bin run without `PYTHONPATH` on the fresh build imports.
    The addon and the extension are versioned apart, so an older extension is
    a state to serve through, not an error.
    """
    if str(config.get("rust_engine_port", "on")).strip().lower() == "off":
        return
    install_backend = getattr(engine_py, "install_backend", None)
    if install_backend is None:
        _logger.warning(
            "rust_engine: the engine_py extension at %s predates the persistence "
            "port; env.backend stays the fork's own. Rebuild it to serve writes "
            "natively",
            getattr(engine_py, "__file__", "?"),
        )
        return
    try:
        port = install_backend()
        port.KERNEL_FOR = lambda env: (
            orm_shim.KERNEL
            if orm_shim._bound_db(env) and orm_shim._ensure_kernel(env)
            else None
        )
        port.install(dbname=db_name)
    except Exception:
        _logger.exception(
            "rust_engine: could not install the persistence port; env.backend "
            "stays the fork's own and the rest of the engine is armed as usual"
        )
        return
    _STATE["port"] = port


def start() -> None:
    from odoo.tools import config

    _open_trace_level()
    db_name = config.get("rust_engine_db")
    if not db_name:
        _logger.info(
            "rust_engine loaded but no rust_engine_db is configured; doing nothing"
        )
        return

    with _LOCK:
        if _STATE["shims"] is not None:
            return

        try:
            import engine_py
        except ImportError:
            _logger.exception(
                "rust_engine: the engine_py extension is not importable; "
                "this server will run entirely on python"
            )
            return

        stale = stale_extension(engine_py)
        if stale:
            _logger.error(
                "rust_engine: refusing to arm for %s: %s; "
                "this server will run entirely on python",
                db_name,
                stale,
            )
            return

        try:
            conninfo, psycopg_info = _connection_specs(db_name)
        except _CannotArm as exc:
            _logger.error(
                "rust_engine: refusing to arm for %s: %s; "
                "this server will run entirely on python",
                db_name,
                exc,
            )
            return

        try:
            rust_db = engine_py.RustDb(
                conninfo, int(config.get("rust_engine_threads") or 4)
            )
            rust_db.connect()
        except Exception:
            _logger.exception(
                "rust_engine: cannot reach %s over the rust connector; "
                "this server will run entirely on python",
                db_name,
            )
            return

        db_shim, orm_shim = engine_py.install_shims()
        _STATE.update(
            db=db_name,
            conninfo=conninfo,
            psycopg_info=psycopg_info,
            rust_db=rust_db,
            shims=(db_shim, orm_shim),
        )

        db_shim.RUST_DB = rust_db
        db_shim.CONNINFO = conninfo
        db_shim.PSYCOPG_CONNINFO = psycopg_info
        db_shim.install()

        orm_shim.DBNAME = db_name
        orm_shim.KERNEL_FACTORY = _build_kernel_for
        orm_shim.PROCESS_HOOK = _report_here
        # `install()` above only patches; whether a borrow hands out a rust
        # connection is `ACTIVE`, and that follows the mode from here on.
        _apply_config(orm_shim, config)
        orm_shim.install()

        _arm_port(engine_py, orm_shim, config, db_name)

        capture_path = config.get("rust_engine_capture") or os.environ.get(
            "RUSTORM_CAPTURE"
        )
        if capture_path:
            from . import capture

            capture.arm(capture_path, db_name)

        _arm_registry_hook()
        _logger.info(
            "rust_engine armed for %s in mode %r; the kernel is built when "
            "that database's registry loads",
            db_name,
            orm_shim.MODE,
        )
        _logger.debug(
            "rust_engine: sample=%s breaker=%s only=%s except=%s capture=%s "
            "RUSTORM_LOG=%r",
            orm_shim.SAMPLE,
            orm_shim.BREAKER,
            sorted(orm_shim.ONLY) or "-",
            sorted(orm_shim.EXCEPT) or "-",
            capture_path or "-",
            os.environ.get("RUSTORM_LOG", "") or "warn (default)",
        )
