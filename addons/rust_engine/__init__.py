import atexit
import logging
import math
import os
import pathlib
import sys
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
    "tick": None,
    "tick_resume": None,
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
    pass


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
TICK_JOIN_SECONDS = 10

CHECKOUT = pathlib.Path(__file__).resolve().parents[2]
ENGINE_PYTHON = "engine-py/python"
WORKSPACE_INPUTS = ("Cargo.toml", "Cargo.lock")
LINKED_CRATES = ("kernel", "engine-py")
CRATE_INPUTS = ("Cargo.toml", "build.rs")
SKIP_FRESHNESS_ENV = "RUSTORM_SKIP_FRESHNESS_CHECK"


def source_inputs(root: pathlib.Path) -> list[pathlib.Path]:
    files = [root / name for name in WORKSPACE_INPUTS]
    for crate in LINKED_CRATES:
        files += [root / crate / name for name in CRATE_INPUTS]
        src = root / crate / "src"
        files += [
            p for p in src.rglob("*.rs") if "bin" not in p.relative_to(src).parts[:-1]
        ]
    return [p for p in files if p.is_file()]


def source_crc(root: pathlib.Path) -> str:
    blob = b"".join(
        rel.encode() + b"\0" + path.read_bytes() + b"\0"
        for rel, path in sorted(
            (p.relative_to(root).as_posix(), p) for p in source_inputs(root)
        )
    )
    return f"{zlib.crc32(blob):08x}"


def stale_extension(engine_py, root: pathlib.Path = CHECKOUT) -> str | None:
    if os.environ.get(SKIP_FRESHNESS_ENV):
        return None
    where = getattr(engine_py, "__file__", "?")
    if (
        not (root / ENGINE_PYTHON).is_dir()
        or not (root / "engine-py/build.rs").is_file()
    ):
        return (
            f"the engine_py at {where} needs its Python half from {root / ENGINE_PYTHON}, "
            f"and {root} is not an odoo-rust-orm checkout"
        )
    built = getattr(engine_py, "__source_crc__", None)
    current = source_crc(root)
    rebuild = (
        f"run {root}/harness/install_engine.sh, which builds it and installs it "
        f"into this interpreter, or set {SKIP_FRESHNESS_ENV}=1"
    )
    if built != current:
        was = (
            "predates the source stamp"
            if built is None
            else f"was compiled from Rust sources with crc {built}"
        )
        return (
            f"the engine_py at {where} {was}, but the Rust sources under {root} "
            f"are crc {current}; {rebuild}"
        )
    profile = getattr(engine_py, "__profile__", None)
    if profile != "release":
        return f"the engine_py at {where} is a {profile} build; {rebuild}"
    return None


def _use_checkout_python(root: pathlib.Path = CHECKOUT) -> None:
    path = str(root / ENGINE_PYTHON)
    if path not in sys.path:
        sys.path.insert(0, path)


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
DEFAULT_BREAKER = 3
PARAM_SAMPLE = "rust_engine.verify_sample"


def _switch_connection():
    import psycopg

    pid = os.getpid()
    owner, conn = _STATE.get("switch_conn") or (None, None)
    if owner != pid:
        conn = None
    if conn is None or conn.closed:
        conn = psycopg.connect(**_STATE["psycopg_info"], autocommit=True)
        _STATE["switch_conn"] = (pid, conn)
    return conn


def _close_switch_connection() -> None:
    owner, conn = _STATE.get("switch_conn") or (None, None)
    _STATE["switch_conn"] = None
    if conn is not None and owner == os.getpid():
        try:
            conn.close()
        except Exception as exc:
            _logger.debug("rust_engine: closing the kill-switch connection: %s", exc)


def _read_params():
    import psycopg

    conn = _switch_connection()
    try:
        with conn.cursor() as cur:
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
    port = _STATE.get("port")
    if port is not None:
        pstats = port.stats()
        native = pstats.get("native", {})
        _logger.info(
            "rust port%s: create_rows=%d update_rows=%d delegated=%s share=%.2f",
            " (final)" if final else "",
            native.get("create_rows", 0),
            native.get("update_rows", 0),
            pstats.get("delegated") or "-",
            pstats.get("native_share", 0.0),
        )
    for (model, method, reason), n in snap.get("gate_reasons", ()):
        _logger.info("rust kernel gate: %5d  %s.%s: %s", n, model, method, reason)
    for model, msg in sorted(snap.get("errors", {}).items()):
        _logger.debug("rust kernel first error on %s: %s", model, msg)


def _tick(orm_shim, seconds, stop) -> None:
    while not stop.wait(seconds):
        try:
            _apply_params(orm_shim, _read_params())
        except Exception:
            _logger.exception("rust_engine: could not read the kill switch")
        try:
            _report(orm_shim)
        except Exception:
            _logger.exception("rust_engine: the routing reporter failed")


def _start_tick(orm_shim, seconds) -> None:
    stop = threading.Event()
    thread = threading.Thread(
        target=_tick,
        args=(orm_shim, seconds, stop),
        name="rust_engine.tick",
        daemon=True,
    )
    _STATE["tick"] = (thread, stop, seconds)
    thread.start()


def _stop_tick():
    tick, _STATE["tick"] = _STATE["tick"], None
    if tick is None:
        return None
    thread, stop, seconds = tick
    stop.set()
    thread.join(TICK_JOIN_SECONDS)
    if thread.is_alive():
        _logger.warning(
            "rust_engine: the tick thread did not stop within %d s; pid %d "
            "forks with it running",
            TICK_JOIN_SECONDS,
            os.getpid(),
        )
    return seconds


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
    _start_tick(orm_shim, seconds)
    atexit.register(lambda: _report(orm_shim, final=True))


def _before_fork() -> None:
    _STATE["tick_resume"] = _stop_tick()
    _close_switch_connection()


def _after_fork_in_parent() -> None:
    seconds, _STATE["tick_resume"] = _STATE["tick_resume"], None
    if seconds and _STATE["tick"] is None:
        _start_tick(_STATE["shims"][1], seconds)


def _after_fork_in_child() -> None:
    _STATE["tick_resume"] = None


def _create_kernel(registry):
    import engine_py

    orm_shim = _STATE["shims"][1]
    started = time.monotonic()
    export = engine_py.export_registry(registry)
    export_ms = (time.monotonic() - started) * 1000
    kernel = engine_py.RustKernel.build(_STATE["rust_db"], export)
    orm_shim.snapshot_orders(export)
    orm_shim.DBNAME = _STATE["db"]
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


def _create_kernel_for(registry):
    return _create_kernel(registry)


def _arm_registry_hook() -> None:
    from odoo.modules.registry import Registry

    orig_new = Registry.new.__func__

    def new(cls, db_name, *args, **kw):
        registry = orig_new(cls, db_name, *args, **kw)
        if db_name == _STATE["db"]:
            orm_shim = _STATE["shims"][1]
            try:
                # the export reads through the ORM; a read that reaches the
                # port must be served by python while the kernel is built
                with orm_shim.building():
                    orm_shim.KERNEL = _create_kernel(registry)
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
            if orm_shim._bound_db(env) and orm_shim._initialize_kernel_if_ready(env)
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


CONFIG_KEYS = (
    "rust_engine_breaker",
    "rust_engine_capture",
    "rust_engine_db",
    "rust_engine_except",
    "rust_engine_mode",
    "rust_engine_only",
    "rust_engine_port",
    "rust_engine_report_seconds",
    "rust_engine_verify_sample",
)


def _routed_database(config):
    explicit = config.get("rust_engine_db")
    if explicit:
        return explicit
    served = config.get("db_name") or []
    if isinstance(served, str):
        served = [name.strip() for name in served.split(",") if name.strip()]
    return served[0] if len(served) == 1 else None


_ROUTING_MODES = frozenset(("on", "shadow"))


class _Unarmed(Exception):
    pass


def start(config=None) -> None:
    if config is None:
        from odoo.tools import config

    _open_trace_level()
    config.claim_file_options(*CONFIG_KEYS)
    requested = str(config.get("rust_engine_mode") or "off").strip().lower()
    try:
        _start(config, requested)
    except _Unarmed as exc:
        # opting in to routing (or misspelling it) and running without it are
        # not the same server: a server-wide module's exception is only
        # logged, so this one exits
        if requested != "off":
            raise SystemExit(
                f"rust_engine: {exc}; rust_engine_mode is {requested!r}, so the "
                "server refuses to start instead of running entirely on python"
            ) from exc.__cause__
        _logger.warning(
            "rust_engine: %s; rust_engine_mode is off, so nothing is routed",
            exc,
            exc_info=exc.__cause__,
        )


def _start(config, requested) -> None:
    if requested not in _ROUTING_MODES | {"off"}:
        raise _Unarmed(f"rust_engine_mode is {requested!r}, which is not on|off|shadow")
    db_name = _routed_database(config)
    if not db_name:
        if requested in _ROUTING_MODES:
            raise _Unarmed(
                "no rust_engine_db is configured and the server does not name "
                "exactly one database"
            )
        _logger.info(
            "rust_engine loaded but no rust_engine_db is configured and the "
            "server does not name exactly one database; doing nothing"
        )
        return

    with _LOCK:
        if _STATE["shims"] is not None:
            return

        try:
            import engine_py
        except ImportError as exc:
            raise _Unarmed("the engine_py extension is not importable") from exc

        stale = stale_extension(engine_py)
        if stale:
            raise _Unarmed(f"cannot arm for {db_name}: {stale}")

        try:
            conninfo, psycopg_info = _connection_specs(db_name)
        except _CannotArm as exc:
            raise _Unarmed(f"cannot arm for {db_name}: {exc}") from exc

        try:
            rust_db = engine_py.RustDb(conninfo)
            rust_db.connect().close()
        except Exception as exc:
            raise _Unarmed(f"cannot reach {db_name} over the rust connector") from exc

        _use_checkout_python()
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
        orm_shim.KERNEL_FACTORY = _create_kernel_for
        orm_shim.PROCESS_HOOK = _report_here
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
        os.register_at_fork(
            before=_before_fork,
            after_in_parent=_after_fork_in_parent,
            after_in_child=_after_fork_in_child,
        )
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
