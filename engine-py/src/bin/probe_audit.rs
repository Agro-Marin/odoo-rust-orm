use anyhow::Result;
use engine_py::cursor::RustDb;
use engine_py::kernel::RustKernel;
use pyo3::prelude::*;
use pyo3::types::PyDict;

const KERNEL_GIL: &str = r##"
import threading, time, json
req = json.dumps({"model":"res.partner","method":"search_read","fields":["name"],"uid":2})
def work(i):
    for _ in range(REPS):
        KERNEL.dispatch(conns[i], req)
def timed(n):
    ts = [threading.Thread(target=work, args=(i,)) for i in range(n)]
    t0 = time.perf_counter()
    for t in ts: t.start()
    for t in ts: t.join()
    return time.perf_counter() - t0
one = timed(1)
many = timed(N)
print("PROBE kernel reps=%d  1thread=%.3fs  %dthreads=%.3fs  ratio=%.2fx"
      % (REPS, one, N, many, many / one))
print("PROBE kernel verdict %s (parallel ~1.0x, serialized ~%.1fx)"
      % ("SERIALIZED" if many > one * (N * 0.7) else "parallel", float(N)))
"##;

const LOGBRIDGE: &str = r##"
import logging, json
logging.basicConfig(level=logging.DEBUG, format="PYLOG %(name)s %(levelname)s %(message)s")
req = json.dumps({"model":"res.country","method":"search_read","fields":["name"],"limit":1,"uid":2})
KERNEL.dispatch(conns[0], req)
bad = json.dumps({"model":"res.country","method":"search_count","limit":3,"order":"name","uid":2})
try:
    KERNEL.dispatch(conns[0], bad)
except Exception as e:
    print("PROBE logbridge refusal raised:", str(e)[:80])
"##;

const RACE: &str = r##"
import json, threading, collections

_r = shim.FakeConnection(conns[0]).cursor()
_r.execute("SELECT id FROM res_users WHERE active AND login = 'rustorm_sweep_probe'")
_row = _r.fetchone()
assert _row, "seed the sweep fixture before the identity race"
IDENTITIES = [{"uid": _row[0], "su": False},
              {"uid": _row[0], "su": True}, {"uid": 2, "su": False}]
QUERY = {"model": "res.country", "method": "search_count", "domain": []}

def dispatch(conn, ident):
    return KERNEL.dispatch(conn, json.dumps({**QUERY, **ident}))

truth = [k.dispatch(conns[0], json.dumps({**QUERY, **i}))
         for k, i in zip(TRUTH_KERNELS, IDENTITIES)]
print("PROBE race truth:", truth)
assert truth[0] != truth[1], (
    "the ordinary and sudo identities must have different answers; "
    "otherwise this fixture cannot detect access-scope leakage", truth)

seen = [collections.Counter() for _ in IDENTITIES]
errors = []
lock = threading.Lock()

def worker(t):
    conn = conns[t % len(conns)]
    for r in range(REPS):
        k = (t + r) % len(IDENTITIES)
        try:
            got = dispatch(conn, IDENTITIES[k])
        except Exception as e:
            with lock: errors.append(str(e)[:80])
            continue
        with lock: seen[k][got] += 1

ts = [threading.Thread(target=worker, args=(t,)) for t in range(N)]
for t in ts: t.start()
for t in ts: t.join()

bad = []
for k, (ident, counts) in enumerate(zip(IDENTITIES, seen)):
    wrong = {v: n for v, n in counts.items() if v != truth[k]}
    if wrong:
        bad.append((ident, truth[k], wrong))
total = sum(sum(c.values()) for c in seen)
print("PROBE race dispatches=%d threads=%d errors=%d wrong=%d"
      % (total, N, len(errors), len(bad)))
for ident, want, wrong in bad:
    print("PROBE race MISMATCH %s expected=%s got=%s" % (ident, want, wrong))
for e in errors[:3]:
    print("PROBE race ERROR", e)
assert not errors and not bad and total == N * REPS, (
    "identity race failed or did not execute every call", total, errors[:3], bad)
"##;

const TYPES: &str = r##"
import datetime, decimal, psycopg
from psycopg.types.json import Json, Jsonb

CASES = [
    ("boolean",     [True, False, None]),
    ("int2",        [0, -1, 32767, None]),
    ("int4",        [0, -2147483648, 2147483647, None]),
    ("int8",        [0, -9223372036854775808, 9223372036854775807, None]),
    ("float4",      [0.0, 1.5, -2.25, None]),
    ("float8",      [0.0, 1.5, -1e30, None]),
    ("numeric",     ["0", "1.25", "-99999.99999", None]),
    ("varchar",     ["", "plain", "quote'd", "unicode \u00e9\u4e2d", None]),
    ("text",        ["", "multi\nline", None]),
    ("date",        ["2026-01-31", "1970-01-01", None]),
    ("timestamp",   ["2026-01-31 12:34:56", "2026-01-31 12:34:56.789012", None]),
    ("timestamptz", ["2026-01-31 12:34:56+00", None]),
    ("jsonb",       ['{"a": 1, "b": [1, 2], "c": null}', '"scalar"', "[]", None,
                     '{"big": 100000000000000000000, "f": 1e20, "z": -0.0}',
                     Jsonb({"t": ("a", "b"), 1: "int key", "n": 10**20, "f": 1e20}),
                     Jsonb([{"selection": [("draft", "Draft"), ("done", "Done")]}])]),
    ("json",        ['{"x": true}', None, Json({"t": (1, 2), "n": 10**20})]),
    ("bytea",       [b"", b"\x00\x01\xff", None]),
    ("int4[]",      [[1, 2, 3], [], None]),
    ("int8[]",      [[1, 2], None]),
    ("text[]",      [["a", "b"], [], None]),
    ("bool[]",      [[True, False], None]),
    ("float8[]",    [[1.5, 2.5], None]),
    # Arrays of everything the scalar list above already covers. Each of these
    # decoded to RAW BINARY BYTES until 2026-09-09 -- a `date[]` column read
    # back as b"\x00\x00\x00\x01..." -- and the reason no gate saw it is
    # that this list did not name them. A type layer is only covered for the
    # types its corpus mentions.
    ("numeric[]",     [["1.25", "-3.5"], [], None]),
    ("date[]",        [["2026-01-31", "1970-01-01"], None]),
    ("time[]",        [["01:02:03"], None]),
    ("timestamp[]",   [["2026-01-31 12:34:56"], None]),
    ("timestamptz[]", [["2026-01-31 12:34:56+00"], None]),
    ("bytea[]",       [[b"ab", b"\x00\xff"], None]),
    ("oid[]",         [[1, 2], None]),
    # pgvector. Not a catalog type -- its oid is per-database, so it is
    # matched by NAME -- but it is in this workspace's database template and
    # agromarin's AI modules store embeddings in it. It had already cost this
    # repo once, encoded as TEXT into a binary COPY; it was reading back as
    # raw bytes and refusing every write until 2026-09-09.
]

# Extension types: skipped, not failed, where the extension is absent -- the
# probe must run against any database, and a case that cannot be created is
# not a mismatch. Both are matched by NAME in the cursor, their oids being
# per-database.
EXTENSION_CASES = [
    ("vector", "vector(3)",  ["[1,2,3]", None]),
    ("postgis", "geometry",  ["SRID=4326;POINT(1 2)", None]),
]

# Read-only: this cursor DECODES these but cannot yet ENCODE them, so only
# psycopg writes and both read. Splitting the direction is what lets the gate
# hold at zero while still covering the half that works -- the alternative was
# no coverage at all, which is how the array types above stayed broken.
# (required extension or None, type, values)
READ_ONLY_CASES = [
    (None, "interval", ["1 day", "1 month 2 days 03:04:05", None]),
    # geography DECODES fine and cannot be written: PostgreSQL has an
    # implicit text->geometry cast and none for geography, and this transport
    # binds every parameter in binary against the statement's resolved type,
    # so a text literal cannot reach the server's input function the way
    # psycopg's untyped parameter does. Writing it needs an EWKB encoder.
    ("postgis", "geography", ["SRID=4326;POINT(1 2)", None]),
    (None, "uuid", ["00000000-0000-0000-0000-000000000001", None]),
]

def norm(v):
    if isinstance(v, memoryview):
        return bytes(v)
    if isinstance(v, decimal.Decimal):
        return float(v)
    if isinstance(v, tuple):
        return list(v)
    return v

rust = shim.FakeConnection(conns[2])
rcur = rust.cursor()
pc = psycopg.connect(CONNINFO, autocommit=True)
pcur = pc.cursor()

bad, checked = [], 0
pcur.execute("SELECT extname FROM pg_extension")
have = {r[0] for r in pcur.fetchall()}
skipped = [ext for ext, _, _ in EXTENSION_CASES if ext not in have]
CASES = CASES + [(ct, vs) for ext, ct, vs in EXTENSION_CASES if ext in have]

for coltype, values in CASES:
    # A type name is not an identifier: `vector(3)` and `int4[]` both need
    # flattening before they can name a table.
    t = "probe_t_" + "".join(
        ch if ch.isalnum() or ch == "_" else "_" for ch in coltype.replace("[]", "_arr")
    )
    pcur.execute("DROP TABLE IF EXISTS %s" % t)
    pcur.execute("CREATE TABLE %s (i serial primary key, w text, v %s)" % (t, coltype))
    for v in values:
        checked += 1
        try:
            rcur.execute("INSERT INTO %s (w, v) VALUES ('rust', %%s)" % t, (v,))
            rust.commit()
        except Exception as e:
            bad.append((coltype, repr(v)[:34], "WRITE RAISED " + str(e)[:48], "ok"))
            try:
                rust.rollback()
            except Exception:
                pass
            pcur.execute("DELETE FROM %s" % t)
            continue
        pcur.execute("INSERT INTO %s (w, v) VALUES ('py', %%s)" % t, (v,))

        try:
            rcur.execute("SELECT w, v FROM %s ORDER BY i" % t)
            by_rust = [(r[0], norm(r[1])) for r in rcur.fetchall()]
            rust.commit()
        except Exception as e:
            bad.append((coltype, repr(v)[:34], "READ RAISED " + str(e)[:48], "ok"))
            try:
                rust.rollback()
            except Exception:
                pass
            pcur.execute("DELETE FROM %s" % t)
            continue
        pcur.execute("SELECT w, v FROM %s ORDER BY i" % t)
        by_py = [(r[0], norm(r[1])) for r in pcur.fetchall()]

        if by_rust != by_py:
            bad.append((coltype, repr(v)[:34], repr(by_rust)[:64], repr(by_py)[:64]))
        else:
            vals = {w: val for w, val in by_rust}
            if len(vals) == 2 and vals.get("rust") != vals.get("py"):
                bad.append((coltype, repr(v)[:34],
                            "wrote " + repr(vals.get("rust"))[:52],
                            "wrote " + repr(vals.get("py"))[:52]))
        pcur.execute("DELETE FROM %s" % t)
    pcur.execute("DROP TABLE %s" % t)

class _SubDate(datetime.date):
    pass

class _SubDatetime(datetime.datetime):
    pass

class _SubInt(int):
    pass

UNTYPED_CONTEXT = [
    ("date", datetime.date(2026, 1, 31)),
    ("date subclass", _SubDate(2026, 1, 31)),
    ("datetime", datetime.datetime(2026, 1, 31, 12, 34, 56)),
    ("datetime subclass", _SubDatetime(2026, 1, 31, 12, 34, 56)),
    ("int subclass", _SubInt(7)),
    ("float", 1.5),
    ("bool", True),
    ("none", None),
    ("str", "plain"),
]
for name, v in UNTYPED_CONTEXT:
    checked += 1
    sql = "SELECT CONCAT('k-', %s)"
    try:
        rcur.execute(sql, (v,))
        by_rust = rcur.fetchall()[0][0]
        rust.commit()
    except Exception as e:
        by_rust = "RAISED " + type(e).__name__
        rust.rollback()
    try:
        pcur.execute(sql, (v,))
        by_py = pcur.fetchall()[0][0]
    except Exception as e:
        by_py = "RAISED " + type(e).__name__
    if by_rust != by_py:
        bad.append(("untyped ctx", name, repr(by_rust)[:64], repr(by_py)[:64]))

MULTI = """
    CREATE INDEX IF NOT EXISTS probe_multi_a ON probe_multi (i);
    CREATE INDEX IF NOT EXISTS probe_multi_b ON probe_multi (i, w);
    """
pcur.execute("DROP TABLE IF EXISTS probe_multi")
pcur.execute("CREATE TABLE probe_multi (i serial primary key, w text)")
for name, params in (("multi/none", None), ("multi/empty-tuple", ()), ("multi/empty-list", [])):
    checked += 1
    try:
        rcur.execute(MULTI, params)
        rust.commit()
        by_rust = "ok"
    except Exception as e:
        by_rust = "RAISED " + type(e).__name__ + ": " + str(e).strip()
        rust.rollback()
    if by_rust != "ok":
        bad.append(("untyped ctx", name, by_rust[:64], "'ok'"))
    pcur.execute("DROP INDEX IF EXISTS probe_multi_a; DROP INDEX IF EXISTS probe_multi_b")
pcur.execute("DROP TABLE probe_multi")

for value in (["a", "b"], [], ["NULL", None, "a,b", 'a"b', "a\\b"]):
    checked += 1
    rcur.execute("SELECT %s", (value,))
    by_rust = rcur.fetchone()[0]
    rust.commit()
    by_py = pcur.execute("SELECT %s", (value,)).fetchone()[0]
    if by_rust != by_py:
        bad.append(("untyped array", repr(value), repr(by_rust), repr(by_py)))

for query, params in (
    ("SELECT '%%(city)s' AS a, %s AS b", ["x"]),
    ("SELECT '%%(city)s' AS a", None),
    ("SELECT '%%' AS a", ()),
    ("SELECT %(v)s AS a, 'x %% y' AS b", {"v": 1}),
    ("SELECT 'a' LIKE '%%a%%' AS a, %s AS b", [2]),
):
    checked += 1
    rcur.execute(query, params)
    by_rust = rcur.fetchall()
    rust.commit()
    by_py = pcur.execute(query, params).fetchall()
    if by_rust != by_py:
        bad.append(("percent", query[:30], repr(by_rust), repr(by_py)))

print("PROBE types checked=%d mismatches=%d" % (checked, len(bad)))
for b in bad:
    print("PROBE types MISMATCH %-12s value=%-20s rust=%-42s psycopg=%s" % b)
"##;

const GIL: &str = r#"
import threading, time, psycopg

def timed(fn, n):
    ts = [threading.Thread(target=fn, args=(i,)) for i in range(n)]
    t0 = time.perf_counter()
    for t in ts: t.start()
    for t in ts: t.join()
    return time.perf_counter() - t0

def rust_work(i):
    conns[i].execute("SELECT pg_sleep(%s)", [SLEEP])

pyconns = [psycopg.connect(CONNINFO) for _ in range(N)]
def py_work(i):
    with pyconns[i].cursor() as c:
        c.execute("SELECT pg_sleep(%s)", [SLEEP])

rust = timed(rust_work, N)
py   = timed(py_work, N)
serial_floor = SLEEP * N
print("PROBE gil threads=%d sleep=%.2fs" % (N, SLEEP))
print("PROBE gil rust_wall=%.3fs  psycopg_wall=%.3fs  serial_floor=%.2fs  parallel_floor=%.2fs"
      % (rust, py, serial_floor, SLEEP))
print("PROBE gil verdict rust=%s psycopg=%s"
      % ("SERIALIZED" if rust > serial_floor * 0.8 else "parallel",
         "SERIALIZED" if py   > serial_floor * 0.8 else "parallel"))
"#;

const DESC: &str = r#"
import rust_db_shim as shim
shim.RUST_DB = RUST_DB
shim.CONNINFO = CONNINFO
cnx = shim.FakeConnection(conns[0])
cur = cnx.cursor()
cur.execute("SELECT id, login FROM res_users WHERE id = -1")
print("PROBE desc zero_rows description=%r rowcount=%r" % (cur.description, cur.rowcount))
cur.execute("SELECT id, login FROM res_users WHERE id = 2")
print("PROBE desc one_row  description=%r" % ([c.name for c in cur.description],))

import psycopg
pc = psycopg.connect(CONNINFO)
with pc.cursor() as c:
    c.execute("SELECT id, login FROM res_users WHERE id = -1")
    print("PROBE desc psycopg zero_rows description=%r" % ([d.name for d in c.description],))

cur2 = cnx.cursor()
cur2.execute("CREATE TEMP TABLE probe_many (a int)")
cur2.executemany("INSERT INTO probe_many VALUES (%s)", [(1,), (2,), (3,)])
print("PROBE many rust_rowcount=%r (psycopg reports the total, i.e. 3)" % cur2.rowcount)
cur2.execute("SELECT count(*) FROM probe_many")
print("PROBE many rows_actually_inserted=%r" % (cur2.fetchone()[0],))
"#;

const COPYABORT: &str = r#"
cnx2 = shim.FakeConnection(conns[1])
cur = cnx2.cursor()
cur.execute("CREATE TEMP TABLE probe_copy (a int, b text)")
try:
    with cur.copy("COPY probe_copy (a, b) FROM STDIN (FORMAT BINARY)") as cp:
        cp.set_types([23, 25])
        cp.write_row((1, "one"))
        cp.write_row((2, "two"))
        raise ValueError("caller blew up mid-COPY")
except ValueError as e:
    print("PROBE copyabort caught=%s" % e)
cur.execute("SELECT count(*) FROM probe_copy")
n = cur.fetchone()[0]
print("PROBE copyabort rows_landed=%d  (psycopg3 aborts the COPY -> expected 0)" % n)
"#;

fn main() -> Result<()> {
    engine_py::logbridge::install_stderr();
    let which = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let db = odoo_kernel::config::db();
    let dsn = odoo_kernel::config::dsn_for(Some(&db));
    let rt = std::sync::Arc::new(tokio::runtime::Runtime::new()?);
    let rust_db = RustDb::new(dsn.clone(), rt.clone());

    let n: usize = 4;

    Python::initialize();
    Python::attach(|py| -> PyResult<()> {
        let sys = py.import("sys")?;
        let path = sys.getattr("path")?;
        path.call_method1(
            "insert",
            (0, odoo_kernel::config::venv_site().display().to_string()),
        )?;

        let ns = PyDict::new(py);
        let conns = pyo3::types::PyList::empty(py);
        for _ in 0..n {
            conns.append(rust_db.connect(py, None)?.into_pyobject(py)?)?;
        }
        ns.set_item("conns", &conns)?;
        ns.set_item("N", n)?;
        ns.set_item("SLEEP", 0.4f64)?;
        ns.set_item("CONNINFO", dsn.as_str())?;
        ns.set_item(
            "RUST_DB",
            Py::new(py, RustDb::new(dsn.clone(), rt.clone()))?,
        )?;

        engine_py::install_shims_py(py)?;
        let run = |src: &str| -> PyResult<()> {
            py.run(&std::ffi::CString::new(src).unwrap(), Some(&ns), Some(&ns))
        };
        let desc_done = std::cell::Cell::new(false);
        let desc = || -> PyResult<()> {
            if desc_done.replace(true) {
                return Ok(());
            }
            run(DESC)
        };
        if which == "all" || which == "gil" {
            run(GIL)?;
        }
        if which == "types" {
            desc()?;
            run(TYPES)?;
        }
        if which == "race" {
            desc()?;
            let path = std::env::args().nth(2).ok_or_else(|| {
                pyo3::exceptions::PyValueError::new_err("usage: probe_audit race <export.json>")
            })?;

            let export = std::fs::read_to_string(&path).map_err(|e| {
                pyo3::exceptions::PyOSError::new_err(format!(
                    "cannot read the export at {path}: {e}"
                ))
            })?;

            let shared = RustKernel::build(py, &rust_db, &export)?;
            ns.set_item("KERNEL", Py::new(py, shared)?)?;
            let truth = pyo3::types::PyList::empty(py);
            for _ in 0..3 {
                truth.append(Py::new(py, RustKernel::build(py, &rust_db, &export)?)?)?;
            }
            ns.set_item("TRUTH_KERNELS", &truth)?;
            ns.set_item("REPS", 200)?;
            run(RACE)?;
        }
        if which == "kernel" {
            let path = std::env::args()
                .nth(2)
                .expect("usage: probe_audit kernel <export.json>");
            let export = std::fs::read_to_string(path).unwrap();
            let kernel = RustKernel::build(py, &rust_db, &export)?;
            ns.set_item("KERNEL", Py::new(py, kernel)?)?;
            ns.set_item("REPS", 40)?;
            run(KERNEL_GIL)?;
        }
        if which == "logbridge" {
            let path = std::env::args()
                .nth(2)
                .expect("usage: probe_audit logbridge <export.json>");
            let export = std::fs::read_to_string(path).unwrap();
            let kernel = RustKernel::build(py, &rust_db, &export)?;
            ns.set_item("KERNEL", Py::new(py, kernel)?)?;
            run(LOGBRIDGE)?;
        }
        if which == "all" || which == "desc" {
            desc()?;
        }
        if which == "all" || which == "copyabort" {
            desc()?;
            run(COPYABORT)?;
        }
        Ok(())
    })
    .map_err(|e| anyhow::anyhow!("python error: {e}"))?;
    Ok(())
}
