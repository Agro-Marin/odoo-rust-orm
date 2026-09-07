"""Load the Rust read kernel into a running Odoo.

`post_load` is the only hook that runs early enough to patch `ConnectionPool`
before anything borrows from it, and it runs too early to build the kernel:
there is no registry yet. So this splits in two. `start()` binds the connection
layer and arms a hook; the hook builds the kernel the first time the bound
database's registry is loaded, FROM THAT REGISTRY, and rebuilds it whenever the
registry is rebuilt. Nothing is read from an export file, so nothing can drift
from the metamodel actually serving the requests.

Everything here fails closed. An import that fails, a database that is not the
bound one, a model the kernel declines, a divergence in shadow mode -- all of
them end with Odoo answering out of Python exactly as it would with this module
absent. The routing decision is a configuration value with a default of `off`.
"""
import logging
import os
import threading
import time

_logger = logging.getLogger(__name__)

_STATE = {"db": None, "conninfo": None, "psycopg_info": None, "rust_db": None,
          "shims": None, "reporter": None}
_LOCK = threading.Lock()


_TOKIO_KEYS = frozenset({
    "host", "hostaddr", "port", "user", "password", "dbname", "options",
    "application_name", "sslmode", "connect_timeout", "tcp_user_timeout",
    "keepalives", "keepalives_idle", "keepalives_interval",
    "keepalives_retries",
    "target_session_attrs", "channel_binding", "load_balance_hosts",
})

_TOKIO_RENAMES = {"keepalives_count": "keepalives_retries"}

_TOKIO_HARMLESS = frozenset({"min_protocol_version"})


_SOCKET_DIRS = ("/var/run/postgresql", "/run/postgresql", "/tmp")


def _default_socket_dir():
    for path in _SOCKET_DIRS:
        if os.path.isdir(path) and any(
            name.startswith(".s.PGSQL.") for name in os.listdir(path)
        ):
            return path
    return "localhost"


class _CannotArm(Exception):
    """A configuration the kernel must not silently reinterpret."""


def _connection_specs(db_name):
    """Both dialects of the same connection, as `(rust_dsn, psycopg_info)`.

    Two connectors read this configuration and they do not spell it the same
    way, so returning one and stashing the other where a caller cannot see it
    is how they drift. `rust_dsn` is translated for tokio-postgres;
    `psycopg_info` is the dict Odoo itself built, untouched.

    Reusing `get_connection_info_for_database` rather than rebuilding a dsn from
    `db_host`/`db_port`/`db_user` means the kernel connects by exactly the
    rules the server does -- replica overrides, application_name and any
    `db_sslmode` included. A hand-built dsn agrees with it until one of those
    is configured, and then disagrees silently.

    Two things stand between that dict and a dsn tokio-postgres accepts, and
    both have to be handled out loud rather than by filtering to what parses:
    a parameter it spells differently, and a parameter it cannot honour.
    """
    from odoo.db import get_connection_info_for_database

    _, info = get_connection_info_for_database(db_name)
    psycopg_info = dict(info)
    if "dsn" in info:
        return info["dsn"], psycopg_info

    sslmode = str(info.get("sslmode") or "").strip().lower()
    if sslmode and sslmode not in ("disable", "allow", "prefer"):
        raise _CannotArm(
            "db_sslmode is %r and the rust connector is built without TLS; "
            "arming it would downgrade this database's connections to "
            "plaintext" % sslmode
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
    return " ".join(parts), psycopg_info


DEFAULT_TICK = 60

PARAM_MODE = "rust_engine.mode"
PARAM_SAMPLE = "rust_engine.verify_sample"


def _read_params():
    """The kill switch, read by SQL rather than through `get_param`.

    `get_param` is ormcached, and this runs in a thread with no request around
    it to have processed the cache signal that a write to `ir.config_parameter`
    raises -- so through the ORM this could keep reading the old value for as
    long as the process lives, which is the one thing a kill switch may not do.
    One row, once a tick, straight from the table.

    Reading it through the ORM would also route through the shim being
    switched off, which is a dependency an off switch should not have.
    """
    import psycopg

    with psycopg.connect(**_STATE["psycopg_info"]) as conn, conn.cursor() as cur:
        cur.execute(
            "SELECT key, value FROM ir_config_parameter WHERE key = ANY(%s)",
            ([PARAM_MODE, PARAM_SAMPLE],),
        )
        return dict(cur.fetchall())


def _apply_params(orm_shim, params):
    """Let the database override the config file, and say so once per change."""
    mode = (params.get(PARAM_MODE) or "").strip().lower()
    if mode and mode != orm_shim.MODE:
        try:
            orm_shim.set_mode(mode)
            _logger.warning(
                "rust_engine: %s in the database set the routing mode to %r",
                PARAM_MODE, mode,
            )
        except ValueError:
            _logger.error(
                "rust_engine: %s is %r, which is not on|off|shadow; ignoring",
                PARAM_MODE, mode,
            )
    sample = (params.get(PARAM_SAMPLE) or "").strip()
    if sample:
        try:
            if float(sample) != orm_shim.SAMPLE:
                orm_shim.set_sample(float(sample))
        except ValueError as exc:
            _logger.error(
                "rust_engine: %s is %r and was ignored: %s", PARAM_SAMPLE, sample, exc
            )


def _start_reporter(orm_shim, seconds):
    """Report what this process routed, and obey the kill switch.

    Two jobs in one thread because both want the same slow tick and a second
    thread per worker buys nothing.

    THE REPORT. A rollout needs one number nobody had: the share of reads the
    kernel answered. `stats()` has it and is a process-global dict no request
    ever prints, so in a running worker the difference between "routing
    everything" and "armed and silently routing nothing" was invisible from
    outside -- and both of those states have occurred on this server, the
    second caused by a dsn comparison that read a quoted database name.

    THE SWITCH. `set_mode()` changes one process, and a prefork server is
    several: turning the kernel off across a running server meant restarting
    it. That is a poor answer for a component whose entire argument is that it
    fails closed -- failing closed per call is not the same as being able to
    stop. `ir.config_parameter` is the one piece of state every worker can
    see, so `rust_engine.mode = off` there stops routing everywhere within a
    tick, with no restart and no shell.

    Started from `_build_kernel` rather than from `start()` on purpose: it
    runs after the prefork fork, so each worker gets its own thread rather
    than inheriting one that a fork would not have kept running.
    """
    if seconds <= 0:
        return

    def tick():
        while True:
            time.sleep(seconds)
            try:
                _apply_params(orm_shim, _read_params())
            except Exception:
                _logger.exception("rust_engine: could not read the kill switch")
            try:
                snap = orm_shim.stats()
                _logger.info(
                    "rust kernel: mode=%s sample=%.3f routed=%d share=%.2f "
                    "gate=%d error=%d flush=%d verified=%d diff=%d tripped=%s",
                    snap["mode"], snap["sample"], snap["kernel"],
                    snap["routed_share"], snap["fallback_gate"],
                    snap["fallback_error"], snap["fallback_flush"],
                    snap["shadow_ok"], snap["shadow_diff"], snap["tripped"] or "-",
                )
            except Exception:
                _logger.exception("rust_engine: the routing reporter failed")
                return

    threading.Thread(target=tick, name="rust_engine.tick", daemon=True).start()


def _build_kernel(registry):
    """Build the kernel from a live registry and start routing.

    Called from two places, and it needs both. `Registry.new` covers a RELOAD
    -- installing a module, upgrading one, any `ir.model` change signals one --
    which is what keeps the metamodel current. The shim's factory covers the
    first read in a process that has no kernel, which in prefork is every HTTP
    worker: they are forked before the master loads a registry, so the hook
    alone builds a kernel in the one process that serves no requests.
    """
    import engine_py

    orm_shim = _STATE["shims"][1]
    export = engine_py.export_registry(registry)
    kernel = engine_py.RustKernel.build(_STATE["rust_db"], export)
    orm_shim.DBNAME = _STATE["db"]
    _logger.info(
        "rust kernel ready for %s: %d models, routing mode %r",
        _STATE["db"], kernel.model_count, orm_shim.MODE,
    )
    _report_here()
    return kernel


def _report_here():
    """Start this process's reporter, once.

    Keyed on the pid, not on the thread object: a fork gives the child a
    Thread naming a thread the child does not have, so an `is None` test was
    False in every worker and only the master ever reported -- which is
    exactly the process whose traffic nobody needs to see.
    """
    if _STATE.get("reporter") == os.getpid():
        return
    from odoo.tools import config

    _STATE["reporter"] = os.getpid()
    _start_reporter(
        _STATE["shims"][1],
        int(config.get("rust_engine_report_seconds") or DEFAULT_TICK),
    )


def _build_kernel_for(registry):
    """The shim's factory: the shim stores what this returns."""
    return _build_kernel(registry)


def _arm_registry_hook():
    """Rebuild the kernel after every load of the bound database's registry.

    `Registry.new` is the one entry point that returns a fully loaded registry,
    and it is also what a module install or a signalling invalidation goes
    through, so wrapping it covers first load and every reload with one patch.
    A failure here must not stop the server from starting: the kernel is an
    optimisation, and Odoo without it is Odoo.
    """
    from odoo.orm.runtime.registry import Registry

    orig_new = Registry.new.__func__

    def new(cls, db_name, **kw):
        registry = orig_new(cls, db_name, **kw)
        if db_name == _STATE["db"]:
            try:
                _STATE["shims"][1].KERNEL = _build_kernel(registry)
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


def start():
    """post_load: bind the connection layer, arm the registry hook."""
    from odoo.tools import config

    db_name = config.get("rust_engine_db")
    if not db_name:
        _logger.info(
            "rust_engine loaded but no rust_engine_db is configured; "
            "doing nothing"
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

        try:
            conninfo, psycopg_info = _connection_specs(db_name)
        except _CannotArm as exc:
            _logger.error(
                "rust_engine: refusing to arm for %s: %s; "
                "this server will run entirely on python", db_name, exc,
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
                "this server will run entirely on python", db_name,
            )
            return

        db_shim, orm_shim = engine_py.install_shims()
        _STATE.update(db=db_name, conninfo=conninfo, psycopg_info=psycopg_info,
                      rust_db=rust_db, shims=(db_shim, orm_shim))

        db_shim.RUST_DB = rust_db
        db_shim.CONNINFO = conninfo
        db_shim.PSYCOPG_CONNINFO = psycopg_info
        db_shim.install()

        orm_shim.DBNAME = db_name
        orm_shim.KERNEL_FACTORY = _build_kernel_for
        orm_shim.PROCESS_HOOK = _report_here
        orm_shim.set_mode(config.get("rust_engine_mode") or "off")
        orm_shim.set_sample(config.get("rust_engine_verify_sample") or 0)
        orm_shim.install()

        capture_path = config.get("rust_engine_capture") or os.environ.get("RUSTORM_CAPTURE")
        if capture_path:
            from . import capture

            capture.arm(capture_path, db_name)

        _arm_registry_hook()
        _logger.info(
            "rust_engine armed for %s in mode %r; the kernel is built when "
            "that database's registry loads", db_name, orm_shim.MODE,
        )
