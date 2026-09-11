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

# Concurrency had no coverage at all, and the GIL fix is what made these paths
# genuinely parallel -- so any cache cross-talk that was masked by
# serialization is now live. Two identities, interleaved on N threads: a rule
# cache keyed carelessly would serve one user's rules to the other, which is
# the worst outcome available here and would be invisible single-threaded.
# The lowest active non-admin user, same rule the corpus's "other" symbol
# uses. Its record rules differ most, which is what makes cross-talk visible.
_r = shim.FakeConnection(conns[0]).cursor()
_r.execute("SELECT id FROM res_users WHERE active AND id NOT IN (1,2) ORDER BY id LIMIT 1")
_row = _r.fetchone()
IDENTITIES = [{"uid": 2, "su": False}, {"uid": 2, "su": True}]
if _row:
    IDENTITIES.append({"uid": _row[0], "su": False})
QUERY = {"model": "res.partner", "method": "search_count", "domain": []}

def dispatch(conn, ident):
    return KERNEL.dispatch(conn, json.dumps({**QUERY, **ident}))

# Truth from a FRESH kernel per identity -- each has its own caches, so it
# cannot be contaminated by the very cross-talk this probe is looking for.
# Deriving it from the shared kernel made the baseline self-referential:
# deliberately dropping uid from the rule-cache key poisoned the truth too, and
# the run reported "every identity agrees" as a warning instead of a failure.
truth = [k.dispatch(conns[0], json.dumps({**QUERY, **i}))
         for k, i in zip(TRUTH_KERNELS, IDENTITIES)]
print("PROBE race truth:", truth)
if len(set(truth)) == 1:
    # Say so rather than let a clean run imply coverage it does not have: if
    # every identity sees the same rows, serving one user's rules to another
    # produces the right answer by accident.
    print("PROBE race WARNING: every identity sees the same result, so "
          "cross-talk would be invisible on this database")

seen = [collections.Counter() for _ in IDENTITIES]
errors = []
lock = threading.Lock()

def worker(t):
    conn = conns[t % len(conns)]
    for r in range(REPS):
        k = (t + r) % len(IDENTITIES)      # interleave the identities
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
"##;

const TYPES: &str = r##"
import decimal, psycopg

# Every Odoo statement goes through this cursor, and its type layer -- which
# already hid three defects -- is covered by one parity case. This compares it
# against psycopg on the same values, both directions: a value written by one
# and read by the other must arrive as the same Python object.
#
# psycopg drives DDL in AUTOCOMMIT and the rust side commits after every
# statement. Both holding open transactions on the same table deadlocks on the
# DROP -- which is worth stating, because that is a property of the shim's
# cursor (Odoo semantics: BEGIN on first use, hold until commit) and not a
# quirk of the probe.
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
    ("jsonb",       ['{"a": 1, "b": [1, 2], "c": null}', '"scalar"', "[]", None]),
    ("json",        ['{"x": true}', None]),
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
    # Wider than rust_decimal's 96-bit mantissa, which the WRITE side still
    # goes through; the read side decodes the wire format itself and must be
    # exact at any width and keep the display scale, as psycopg does.
    (None, "numeric", ["123456789012345678901234567890.5", "1.250",
                       "-0.000000000000000000000000000001", None]),
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
    # No Decimal -> float here. That normalisation ran on BOTH sides, so the
    # rust cursor returning a float where psycopg returns a Decimal compared
    # equal for as long as it existed -- the gate could not see the one type
    # it was most worth checking. The types are part of what is compared now.
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
        # One bad value must not abort the sweep; the point is the whole table
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
            # both readers agree; now check the two WRITERS agree with each other
            vals = {w: val for w, val in by_rust}
            if len(vals) == 2 and vals.get("rust") != vals.get("py"):
                bad.append((coltype, repr(v)[:34],
                            "wrote " + repr(vals.get("rust"))[:52],
                            "wrote " + repr(vals.get("py"))[:52]))
        pcur.execute("DELETE FROM %s" % t)
    pcur.execute("DROP TABLE %s" % t)

for ext, coltype, values in READ_ONLY_CASES:
    if ext is not None and ext not in have:
        skipped.append(ext)
        continue
    t = "probe_ro_" + "".join(ch if ch.isalnum() or ch == "_" else "_" for ch in coltype)
    pcur.execute("DROP TABLE IF EXISTS %s" % t)
    pcur.execute("CREATE TABLE %s (i serial primary key, v %s)" % (t, coltype))
    for v in values:
        checked += 1
        pcur.execute("INSERT INTO %s (v) VALUES (%%s)" % t, (v,))
        try:
            rcur.execute("SELECT v FROM %s ORDER BY i" % t)
            by_rust = [norm(r[0]) for r in rcur.fetchall()]
            rust.commit()
        except Exception as e:
            bad.append((coltype, repr(v)[:34], "READ RAISED " + str(e)[:48], "ok"))
            try:
                rust.rollback()
            except Exception:
                pass
            pcur.execute("DELETE FROM %s" % t)
            continue
        pcur.execute("SELECT v FROM %s ORDER BY i" % t)
        by_py = [norm(r[0]) for r in pcur.fetchall()]
        if by_rust != by_py:
            bad.append((coltype, repr(v)[:34], repr(by_rust)[:64], repr(by_py)[:64]))
        pcur.execute("DELETE FROM %s" % t)
    pcur.execute("DROP TABLE %s" % t)

print("PROBE types checked=%d mismatches=%d skipped_extensions=%s"
      % (checked, len(bad), ",".join(sorted(set(skipped))) or "none"))
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
import sys
sys.path.insert(0, SHIM_DIR)
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

# executemany rowcount
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
    let which = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let db = odoo_kernel::config::db();
    let dsn = odoo_kernel::config::dsn_for(Some(&db));
    let rt = std::sync::Arc::new(tokio::runtime::Runtime::new()?);
    let rust_db = RustDb::new(dsn.clone(), rt.clone());

    let n: usize = 4;
    let shim_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("python");

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
        ns.set_item("SHIM_DIR", shim_dir.display().to_string())?;
        ns.set_item(
            "RUST_DB",
            Py::new(py, RustDb::new(dsn.clone(), rt.clone()))?,
        )?;

        let run = |src: &str| -> PyResult<()> {
            py.run(&std::ffi::CString::new(src).unwrap(), Some(&ns), Some(&ns))
        };
        if which == "all" || which == "gil" {
            run(GIL)?;
        }
        if which == "types" {
            run(DESC)?;
            run(TYPES)?;
        }
        if which == "race" {
            run(DESC)?;
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
            run(DESC)?;
        }
        if which == "all" || which == "copyabort" {
            run(DESC)?;
            run(COPYABORT)?;
        }
        Ok(())
    })
    .map_err(|e| anyhow::anyhow!("python error: {e}"))?;
    Ok(())
}
