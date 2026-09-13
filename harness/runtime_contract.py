import contextlib
import importlib.util
import json
import os
import subprocess
import sys
import unittest
from pathlib import Path

from _env import default_db, root_dir, workspace

DB = default_db()
if not any(part in DB for part in ("rustorm", "scratch", "probe")):
    raise RuntimeError("runtime contracts require an owned scratch database")
sys.path.insert(0, os.environ.get("RUSTORM_ODOO_ROOT", str(Path(workspace()) / "odoo")))
import engine_py as engine
import psycopg

import odoo
from odoo.modules.registry import Registry
from odoo.tools import config

config.parse_config(["-c", os.environ["RUSTORM_ODOO_CONF"], "-d", DB, "--no-http"])
reg = Registry(DB)
spec = importlib.util.spec_from_file_location(
    "runtime_addon", Path(root_dir()) / "addons/rust_engine/__init__.py"
)
addon = importlib.util.module_from_spec(spec)
spec.loader.exec_module(addon)
dsn, conninfo = addon._connection_specs(DB)
rust_db = engine.RustDb(dsn)
dbshim, shim = engine.install_shims()
dbshim.RUST_DB, dbshim.CONNINFO, dbshim.PSYCOPG_CONNINFO = rust_db, dsn, conninfo
dbshim.install()
shim.DBNAME, shim.MODE, shim.BREAKER = DB, "off", 3
shim.install()


@contextlib.contextmanager
def env_for(uid=1):
    with reg.cursor() as cr:
        assert type(cr._cnx).__name__ == "FakeConnection"
        yield odoo.api.Environment(cr, uid, {"lang": "en_US"})


def rebuild():
    global reg
    reg.signal_changes()
    reg = Registry.new(DB)
    shim.KERNEL = engine.RustKernel.build(rust_db, engine.export_registry(reg))
    shim.forget_gates()


with env_for() as e:
    e["ir.rule"].search([("name", "=", "Rust ORM snapshot contract")]).unlink()
    user = e["res.users"].search([("login", "=", "rustorm_runtime_contract")])
    if not user:
        user = (
            e["res.users"]
            .with_context(no_reset_password=True)
            .create(
                {
                    "name": "Runtime contract",
                    "login": "rustorm_runtime_contract",
                    "group_ids": [(6, 0, [e.ref("base.group_user").id])],
                }
            )
        )
    uid = user.id
    country = e.ref("base.be")
    cid, currency = country.id, country.currency_id.id
    assert currency
    rule = e["ir.rule"].search([("name", "=", "Rust ORM runtime hidden currency")])
    if not rule:
        rule = e["ir.rule"].create(
            {
                "name": "Rust ORM runtime hidden currency",
                "model_id": e["ir.model"]._get_id("res.currency"),
                "domain_force": repr([("id", "!=", currency)]),
            }
        )
    field = e["ir.model.fields"].search(
        [("model", "=", "res.country"), ("name", "=", "x_runtime_contract")]
    )
    if not field:
        field = e["ir.model.fields"].create(
            {
                "name": "x_runtime_contract",
                "field_description": "Runtime contract",
                "model_id": e["ir.model"]._get_id("res.country"),
                "ttype": "char",
                "state": "manual",
            }
        )
    field.write({"groups": [(5, 0, 0)]})
    fid, rule_id = field.id, rule.id
    e.cr.commit()
rebuild()

# Maintenance queries must not pin an old snapshot, and an ORM dispatch
# must never perform its multi-query read without transaction isolation.
with env_for() as e:
    e.cr.rollback()
    conn = e.cr._cnx
    conn.autocommit = True
    try:
        assert conn.execute("SELECT 1").fetchone() == (1,)
        assert not conn._rust.in_transaction
        with unittest.TestCase().assertRaisesRegex(
            engine.KernelRefused, "repeatable-read"
        ):
            shim.KERNEL.dispatch(
                conn._rust,
                json.dumps(
                    {"model": "res.country", "method": "search_count", "uid": uid}
                ),
            )
    finally:
        conn.autocommit = False
print(
    "CONTRACT maintenance autocommit stays idle and ORM dispatch requires a transaction",
    flush=True,
)

# A redacted wire relation must not overwrite its real foreign key in cache.
answers = []
for mode, preloaded in (("off", False), ("on", False), ("on", True), ("shadow", False)):
    shim.MODE = mode
    with env_for(uid) as e:
        record = e["res.country"].browse(cid)
        if preloaded:
            assert record.currency_id.id == currency
        before = shim.STATS["kernel"]
        rows = record.search_read([("id", "=", cid)], ["name", "currency_id"])
        if mode != "off":
            assert shim.STATS["kernel"] > before, shim.stats()
        assert rows[0]["currency_id"] is False, rows
        assert record.currency_id.id == currency, (mode, preloaded, rows)
        answers.append(rows)
assert all(rows == answers[0] for rows in answers)
print("CONTRACT cache preserves redacted relation (cold/preloaded/shadow)", flush=True)

# A PostgreSQL failure rolls back the dispatch savepoint and arms the breaker.
shim.MODE = "on"
shim.reset_breaker()
with psycopg.connect(**conninfo) as blocker:
    blocker.execute("LOCK res_country IN ACCESS EXCLUSIVE MODE")
    for _ in range(3):
        with env_for() as e:
            e.cr.execute("SET LOCAL lock_timeout = '40ms'")
            try:
                shim._dispatch(e["res.country"], "search_count", domain=[])
            except engine.KernelDatabaseError as exc:
                shim._record_error(e["res.country"], exc)
                e.cr.execute("SELECT 42")
                assert e.cr.fetchone()[0] == 42
            else:
                raise AssertionError("expected real lock timeout")
assert shim.STATS["errors_by_model"]["res.country"] == 3
assert not shim._policy_allows("res.country")
shim.reset_breaker()
print("CONTRACT SQL failures typed, transaction usable, breaker trips", flush=True)

# The dispatch savepoint is released without waiting on it. Whatever the call
# ended in -- rows, a refusal before any statement, a refusal after one -- the
# next statement must run, and must find no kernel savepoint left open: a
# leaked one per routed call would pile subtransactions onto a long request.
shim.MODE = "on"
with env_for(uid) as e:
    conn = e.cr._cnx._rust
    outcomes = []
    for request in (
        {"model": "res.country", "method": "search_count", "uid": uid},
        {"model": "res.country", "method": "no_such_method", "uid": uid},
        {
            "model": "res.country",
            "method": "search_read",
            "uid": uid,
            "fields": ["x_runtime_contract"],
            "domain": [["id", "=", cid]],
        },
    ):
        request |= {"registry_sequence": reg.registry_sequence}
        try:
            shim.KERNEL.dispatch(conn, json.dumps(request))
            outcomes.append("answered")
        except engine.KernelRefused as exc:
            outcomes.append(type(exc).__name__)
        e.cr.execute("SELECT 42")
        assert e.cr.fetchone()[0] == 42, "the statement after a dispatch misread"
        # A leaked kernel savepoint lies below this one, so releasing it
        # succeeds and takes this one with it; only an error naming the
        # kernel's savepoint says there was none.
        try:
            with e.cr.savepoint(flush=False):
                e.cr.execute("RELEASE SAVEPOINT rust_kernel_dispatch")
        except Exception as exc:
            closed = 'savepoint "rust_kernel_dispatch" does not exist' in str(exc)
        else:
            closed = False
        assert closed, "a kernel savepoint was left open after %s" % request["method"]
    assert outcomes[0] == "answered" and outcomes[1] != "answered", outcomes
    e.cr.rollback()
shim.MODE = "off"
print(
    "CONTRACT every dispatch closes its savepoint and the next statement runs (%s)"
    % ", ".join(outcomes),
    flush=True,
)


# Exercise quarantine through the real wrapper, not just its counter helper.
class WrongCount:
    def __init__(self, target):
        self.target, self.calls = target, 0

    def dispatch(self, conn, request):
        self.calls += 1
        result = self.target.dispatch(conn, request)
        assert json.loads(request)["method"] == "search_count"
        return json.dumps(json.loads(result) + 1)


native = shim.KERNEL
faulty = WrongCount(native)
shim.MODE = "off"
with env_for(uid) as e:
    expected = e["res.country"].search_count([])
shim.KERNEL, shim.MODE, shim.SAMPLE = faulty, "on", 1
with env_for(uid) as e:
    assert e["res.country"].search_count([]) == expected
    assert faulty.calls == 1
    assert "res.country" in shim.STATS["quarantined"]
    shim.SAMPLE = 0
    assert e["res.country"].search_count([]) == expected
    assert faulty.calls == 1, "quarantined model called native kernel again"
shim.KERNEL = native
shim.reset_breaker()
print(
    "CONTRACT sampled mismatch returns Python and quarantines later requests",
    flush=True,
)

shim.MODE = "off"
with env_for() as e:
    e["res.country"].browse(cid).x_runtime_contract = "private marker"
    e.cr.commit()
old_export = engine.export_registry(reg)
unstamped = json.loads(old_export)
unstamped.pop("registry_sequence")
try:
    engine.RustKernel.build(rust_db, json.dumps(unstamped))
except engine.KernelRefused:
    pass
else:
    raise AssertionError("unstamped export accepted")
old_kernel = engine.RustKernel.build(rust_db, old_export)
request = json.dumps(
    {
        "model": "res.country",
        "method": "search_read",
        "uid": uid,
        "su": False,
        "domain": [["id", "=", cid]],
        "fields": ["x_runtime_contract"],
    }
)
with env_for() as e:
    assert (
        json.loads(old_kernel.dispatch(e.cr._cnx._rust, request))[0][
            "x_runtime_contract"
        ]
        == "private marker"
    )
addon._STATE.update(db=DB, rust_db=rust_db, shims=(dbshim, shim))
addon._arm_registry_hook()
with env_for() as e:
    e["ir.model.fields"].browse(fid).write(
        {"groups": [(6, 0, [e.ref("base.group_system").id])]}
    )
    e.cr.commit()
reg.signal_changes()
for _ in range(2):
    with env_for() as e:
        try:
            old_kernel.dispatch(e.cr._cnx._rust, request)
        except engine.KernelRegistryStale:
            pass
        else:
            raise AssertionError("retained kernel served obsolete field access")
try:
    engine.RustKernel.build(rust_db, old_export)
except engine.KernelRegistryStale:
    pass
else:
    raise AssertionError("old export accepted after registry signal")
before = shim.KERNEL
reg = Registry.new(DB)
assert shim.KERNEL is not None and shim.KERNEL is not before
with env_for() as e:
    try:
        shim.KERNEL.dispatch(e.cr._cnx._rust, request)
    except engine.KernelAccessDenied:
        pass
    else:
        raise AssertionError("fresh registry allowed restricted field")
with env_for(uid) as e:
    try:
        e["res.country"].browse(cid).read(["x_runtime_contract"])
    except odoo.exceptions.AccessError:
        pass
    else:
        raise AssertionError("Python control allowed restricted field")
print(
    "CONTRACT retained kernel and old export refuse; registry hook restores correct access",
    flush=True,
)


# A new global kernel must not serve a request retaining the older Python
# registry, even if that request starts a new SQL snapshot after publication.
def change_in_other_process(code):
    result = subprocess.run(
        [
            sys.executable,
            str(
                Path(
                    os.environ.get("RUSTORM_ODOO_ROOT", str(Path(workspace()) / "odoo"))
                )
                / "odoo-bin"
            ),
            "shell",
            "-c",
            os.environ["RUSTORM_ODOO_CONF"],
            "-d",
            DB,
            "--no-http",
        ],
        input=code + "\nenv.cr.commit(); env.registry.signal_changes()\n",
        text=True,
        capture_output=True,
        check=True,
        timeout=60,
    )
    assert result.returncode == 0


old_reg = reg
with old_reg.cursor() as old_cr:
    old_env = odoo.api.Environment(old_cr, uid, {})
    old_cr.execute("SELECT count(*) FROM ir_rule")
    old_cr.fetchone()
    change_in_other_process(
        f"env['ir.model.fields'].browse({fid}).write({{'groups': [(5, 0, 0)]}})"
    )
    reg = Registry.new(DB)
    assert old_reg is not reg
    assert old_env["res.country"]._fields["x_runtime_contract"].groups
    assert not reg["res.country"]._fields["x_runtime_contract"].groups
    for mode in ("off", "on", "shadow"):
        shim.MODE = mode
        before = shim.STATS["kernel"]
        try:
            old_env["res.country"].search_read(
                [("id", "=", cid)], ["x_runtime_contract"]
            )
        except odoo.exceptions.AccessError:
            pass
        else:
            raise AssertionError(
                (mode, "old Python registry used newer field permissions")
            )
        assert shim.STATS["kernel"] == before
    # Without a Python registry stamp the native snapshot check still refuses.
    with unittest.TestCase().assertRaisesRegex(
        engine.KernelRefused, "snapshot predates"
    ):
        shim.KERNEL.dispatch(old_cr._cnx._rust, request)
    # Committing retains this environment's Python registry but starts a fresh
    # SQL snapshot on its next query.
    old_cr.commit()
    assert old_env.registry is old_reg
    shim.MODE = "on"
    with unittest.TestCase().assertRaises(odoo.exceptions.AccessError):
        old_env["res.country"].search_read([("id", "=", cid)], ["x_runtime_contract"])
with env_for(uid) as e:
    assert (
        json.loads(shim.KERNEL.dispatch(e.cr._cnx._rust, request))[0][
            "x_runtime_contract"
        ]
        == "private marker"
    )
print(
    "CONTRACT old Python registries and SQL snapshots refuse without poisoning the fresh kernel",
    flush=True,
)

# A cache-only signal can refresh security without replacing the model map.
shim.MODE = "off"
with env_for() as e:
    r = e["ir.rule"].create(
        {
            "name": "Rust ORM snapshot contract",
            "model_id": e["ir.model"]._get_id("res.country"),
            "domain_force": '[(1, "=", 1)]',
        }
    )
    snapshot_rule_id = r.id
    e.cr.commit()
reg.signal_changes()
shim.KERNEL = engine.RustKernel.build(rust_db, engine.export_registry(reg))
count_request = json.dumps(
    {"model": "res.country", "method": "search_count", "uid": uid}
)
where_request = json.dumps(
    {
        "model": "res.country",
        "method": "search",
        "uid": uid,
        "root_active_test": False,
        "trusted_domain": True,
        "domain": [],
    }
)
with env_for(uid) as old:
    initial = json.loads(shim.KERNEL.dispatch(old.cr._cnx._rust, count_request))
    assert initial > 1
    # the port's WHERE compile keeps the snapshot it checked for the rest of
    # the transaction too; it has to be refused just the same once security
    # moves past it
    assert shim.KERNEL.search_where(old.cr._cnx._rust, where_request, offline=False)
    change_in_other_process(
        f"env['ir.rule'].browse({snapshot_rule_id}).write({{'domain_force': {repr([('id', '=', cid)])!r}}})"
    )
    with env_for(uid) as fresh:
        assert json.loads(shim.KERNEL.dispatch(fresh.cr._cnx._rust, count_request)) == 1
    with unittest.TestCase().assertRaisesRegex(
        engine.KernelRefused, "snapshot predates"
    ):
        shim.KERNEL.dispatch(old.cr._cnx._rust, count_request)
    for offline in (True, False):
        with unittest.TestCase().assertRaisesRegex(
            engine.KernelRefused, "snapshot predates"
        ):
            shim.KERNEL.search_where(old.cr._cnx._rust, where_request, offline=offline)
with env_for() as e:
    e["ir.rule"].browse(snapshot_rule_id).unlink()
    e.cr.commit()
reg.signal_changes()
print(
    "CONTRACT security refresh cannot make old transactions use future rule metadata",
    flush=True,
)

# Native success versus a Python exception is a mismatch, not a retry signal.
previous_only = shim.ONLY
shim.ONLY = {"res.country"}
shim.MODE, shim.SAMPLE = "on", 1
shim.reset_breaker()
original_baseline = shim._baseline
expected_error = odoo.exceptions.AccessError("injected authoritative Python denial")
python_calls = []


def denying_baseline(fn, *args, **kwargs):
    if fn.__name__ == "search_count" and args[0]._name == "res.country":
        python_calls.append(1)
        raise expected_error
    return original_baseline(fn, *args, **kwargs)


shim._baseline = denying_baseline
try:
    with env_for(uid) as e:
        before = shim.STATS["kernel"]
        with unittest.TestCase().assertRaises(odoo.exceptions.AccessError) as caught:
            e["res.country"].search_count([])
        assert caught.exception is expected_error
        assert shim.STATS["kernel"] == before + 1
        assert python_calls == [1]
        assert "res.country" in shim.STATS["quarantined"]
finally:
    shim._baseline = original_baseline
shim.SAMPLE = 0
with env_for(uid) as e:
    before = shim.STATS["kernel"]
    e["res.country"].search_count([])
    assert shim.STATS["kernel"] == before
shim.reset_breaker()
shim.ONLY = previous_only
shim.MODE = "off"
print(
    "CONTRACT Python exception is preserved once and quarantines native success immediately",
    flush=True,
)

# A routed _read_group hands back many2one group values that PREFETCH TOGETHER,
# as _read_group_postprocess_groupby builds them. Browsed one by one, reading a
# field of the groups fetched once per record per field: routed web_read_group
# ran 3.95x slower than Python on captured traffic, every answer correct. The
# check is on the query count, because nothing about the rows would show it.
shim.reset_breaker()
previous_mode, previous_sample = shim.MODE, shim.SAMPLE
shim.MODE, shim.SAMPLE = "on", 0.0
try:
    with env_for() as e:
        partners = e["res.partner"]
        # two countries of its own, so the check does not depend on the
        # fixture; the transaction rolls back with the context manager
        seeded = partners.create(
            [
                {"name": "prefetch contract A", "country_id": e.ref("base.be").id},
                {"name": "prefetch contract B", "country_id": e.ref("base.fr").id},
            ]
        )
        e.flush_all()
        routed_before = shim.STATS["kernel"]
        rows = partners._read_group(
            [("id", "in", seeded.ids)], ["country_id"], ["__count"]
        )
        assert shim.STATS["kernel"] == routed_before + 1, "the group read did not route"
        groups = [row[0] for row in rows]
        assert len(groups) >= 2, (
            "the contract needs partners in two countries; seed more"
        )
        ids = {g.id for g in groups}
        shared = [set(g._prefetch_ids) >= ids for g in groups]
        assert all(shared), "routed group records do not prefetch together"
        e.invalidate_all()
        before = e.cr.sql_log_count
        names = [g.name for g in groups]
        queries = e.cr.sql_log_count - before
        assert all(names) and queries <= 2, (
            "reading the name of %d routed groups took %d queries"
            % (len(groups), queries)
        )
        # env_for's cursor commits on a clean exit; the seeded partners are
        # this check's alone
        e.cr.rollback()
finally:
    shim.MODE, shim.SAMPLE = previous_mode, previous_sample
print(
    "CONTRACT routed many2one groups prefetch together: one query reads every group's field",
    flush=True,
)

# A write to res.users taints the cursor only through the fields Odoo itself
# invalidates its user caches for. A preference or an avatar leaves the cursor
# routable and the answer Python's; a group change still gates it.
shim.reset_breaker()
previous_mode, previous_sample = shim.MODE, shim.SAMPLE
shim.MODE, shim.SAMPLE = "on", 0.0
try:
    with env_for() as e:
        me = e["res.users"].browse(uid)
        assert "odoobot_state" not in me._get_fields_invalidation()
        assert "group_ids" in me._get_fields_invalidation()
        me.write({"signature": "<p>runtime contract</p>"})
        assert e.cr not in shim.DIRTY_CRS, "a signature write tainted the cursor"
        routed = shim.STATS["kernel"]
        answer = e["res.country"].search_read([("code", "=", "BE")], ["name"])
        assert shim.STATS["kernel"] == routed + 1, (
            "the read after a harmless write did not route"
        )
        shim.MODE = "off"
        assert answer == e["res.country"].search_read([("code", "=", "BE")], ["name"])
        shim.MODE = "on"
        me.write({"group_ids": [(4, e.ref("base.group_partner_manager").id)]})
        assert e.cr in shim.DIRTY_CRS, "a group write did not taint the cursor"
        routed = shim.STATS["kernel"]
        e["res.country"].search_read([("code", "=", "BE")], ["name"])
        assert shim.STATS["kernel"] == routed, "a read after a group write routed"
        e.cr.rollback()
finally:
    shim.MODE, shim.SAMPLE = previous_mode, previous_sample
print(
    "CONTRACT a res.users write taints only through Odoo's own invalidation fields",
    flush=True,
)

# web_search_read resolves a many2one with rules of its own. read() redacts a
# target the user cannot read to False; web_read keeps its id, and {"id": id}
# when a name was asked for. The routed call used the kernel's label, which is
# read()'s answer: 124 of 1,018 routed web_search_read calls across the sweep
# users disagreed with Python. The committed rule above hides Belgium's
# currency from every user.
shim.reset_breaker()
previous_mode, previous_sample = shim.MODE, shim.SAMPLE
shim.MODE, shim.SAMPLE = "on", 0.0
try:
    with env_for(uid) as e:
        countries = e["res.country"]
        for spec in (
            {"currency_id": {}},
            {"currency_id": {"fields": {"display_name": {}}}},
        ):
            routed = shim.STATS["kernel"]
            answer = countries.web_search_read([("id", "=", cid)], spec)
            assert shim.STATS["kernel"] > routed, "web_search_read did not route"
            shim.MODE = "off"
            python = countries.web_search_read([("id", "=", cid)], spec)
            shim.MODE = "on"
            assert python["records"][0]["currency_id"], (
                "the contract expects Python to keep the hidden currency's id"
            )
            assert answer["records"] == python["records"], "routed %r != python %r" % (
                answer["records"],
                python["records"],
            )
        # web_read's own call, which a web_search_read the gate refuses makes
        routed = shim.STATS["kernel"]
        answer = countries.browse(cid).read(["currency_id"], load=None)
        assert shim.STATS["kernel"] > routed, "read(load=None) did not route"
        shim.MODE = "off"
        python = countries.browse(cid).read(["currency_id"], load=None)
        shim.MODE = "on"
        assert answer == python, "routed %r != python %r" % (answer, python)
        e.cr.rollback()
finally:
    shim.MODE, shim.SAMPLE = previous_mode, previous_sample
print(
    "CONTRACT routed web_search_read keeps an unreadable many2one target as web_read does",
    flush=True,
)

if "mail.mail" in reg:
    shim.reset_breaker()
    previous_mode, previous_sample = shim.MODE, shim.SAMPLE
    shim.MODE, shim.SAMPLE = "on", 0.0
    try:
        with env_for(2) as e:
            request = {
                "model": "mail.mail",
                "method": "search_read",
                "uid": 2,
                "fields": ["mail_message_id"],
                "registry_sequence": reg.registry_sequence,
            }
            with unittest.TestCase().assertRaisesRegex(
                engine.KernelRefused, "_check_access"
            ):
                shim.KERNEL.dispatch(e.cr._cnx._rust, json.dumps(request))
            request["raw_many2one"] = ["mail_message_id"]
            shim.KERNEL.dispatch(e.cr._cnx._rust, json.dumps(request))
            e.cr.rollback()
    finally:
        shim.MODE, shim.SAMPLE = previous_mode, previous_sample
    print(
        "CONTRACT the kernel refuses to label a comodel that decides access in _check_access",
        flush=True,
    )

closing = rust_db.connect()
with psycopg.connect(**conninfo, autocommit=True) as watcher:
    rust_conn = dbshim.FakeConnection(closing)
    backend_pid = rust_conn.execute("SELECT pg_backend_pid()").fetchone()[0]
    rust_conn.rollback()
    rust_conn.close()
    for _ in range(50):
        alive = watcher.execute(
            "SELECT count(*) FROM pg_stat_activity WHERE pid = %s", [backend_pid]
        ).fetchone()[0]
        if not alive:
            break
        __import__("time").sleep(0.1)
    assert not alive, "a closed rust connection kept its backend %s open" % backend_pid
print("CONTRACT closing a rust connection disconnects its backend", flush=True)

pool = dbshim._RustPool(
    max_size=1, check=lambda conn: conn.execute("SELECT 1").fetchone()
)
pool._new_connection = lambda: dbshim.FakeConnection(rust_db.connect())
with psycopg.connect(**conninfo, autocommit=True) as watcher:
    victim = pool.getconn()
    victim_pid = victim.execute("SELECT pg_backend_pid()").fetchone()[0]
    victim.rollback()
    pool.putconn(victim)
    watcher.execute("SELECT pg_terminate_backend(%s)", [victim_pid])
    for _ in range(50):
        if victim.closed:
            break
        __import__("time").sleep(0.1)
    assert victim.closed, (
        "a rust connection whose backend %s was terminated still reads as open"
        % victim_pid
    )
    replacement = pool.getconn(timeout=5)
    assert replacement is not victim, "the pool lent a connection whose backend is gone"
    assert replacement.execute("SELECT 1").fetchone()[0] == 1
    replacement.rollback()
    pool.putconn(replacement)
    pool.close()
print(
    "CONTRACT a rust connection whose backend dies reads as closed and is not lent again",
    flush=True,
)

# Release committed rule/field policy changes before the rest of the corpus.
with env_for() as e:
    e["ir.rule"].browse(rule_id).unlink()
    e["ir.model.fields"].browse(fid).write({"groups": [(5, 0, 0)]})
    e.cr.commit()
reg.signal_changes()
print("RUNTIME CONTRACTS PASS", flush=True)
