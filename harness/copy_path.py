import datetime
import os
import pathlib
import sys

_HERE = (
    str(pathlib.Path(__file__).resolve().parent) if "__file__" in globals() else None
)
sys.path.insert(
    0,
    os.environ.get("RUSTORM_HARNESS")
    or _HERE
    or os.path.join(
        os.environ.get("RUSTORM_WORKSPACE") or pathlib.Path("~/Odoo").expanduser(),
        "odoo-rust-orm",
        "harness",
    ),
)
from _env import dsn_for

import odoo
from odoo.modules.registry import Registry

if not os.environ.get("PYTHONPATH"):
    print("COPY SKIP: no PYTHONPATH; engine_py must be importable")
    sys.exit(3)

import engine_py

dbname = env.cr.dbname  # noqa: F821
conninfo = dsn_for(dbname)

LONG = "x" * 70000

RUN = "copytest-%d" % os.getpid()
CASES = [
    {"name": "copy-plain", "ref": "R1", "comment": "hello"},
    {"name": "copy-empty", "ref": "", "comment": ""},
    {"name": "copy-unicode ñ 日本語 🌍", "ref": "Rü", "comment": "ü\nnewline\ttab"},
    {"name": "copy-null", "ref": None, "comment": None},
    {"name": "copy-long", "ref": "R5", "comment": LONG},
    {"name": "copy-quote'and\"double", "ref": "R6", "comment": "back\\slash"},
    {"name": "copy-bool-t", "ref": "R7", "is_company": True},
    {"name": "copy-bool-f", "ref": "R8", "is_company": False},
    {"name": "copy-zero", "ref": "R9", "color": 0},
    {"name": "copy-neg", "ref": "R10", "color": -7},
    {"name": "copy-big", "ref": "R11", "color": 2147483647},
    {"name": "copy-date", "ref": "R12", "birthdate": datetime.date(1000, 1, 1)},
    {"name": "copy-date2", "ref": "R13", "birthdate": datetime.date(9999, 12, 31)},
    {"name": "copy-float", "ref": "R14", "partner_latitude": 0.1 + 0.2},
    {"name": "copy-float2", "ref": "R15", "partner_latitude": -89.999999},
]
from odoo.orm.runtime.backend import COPY_THRESHOLD

BATCH = [
    dict(case, name="%s#%03d" % (case["name"], copy))
    for copy in range(-(-COPY_THRESHOLD // len(CASES)))
    for case in CASES
]
COLUMNS = [
    "name",
    "ref",
    "comment",
    "is_company",
    "color",
    "birthdate",
    "partner_latitude",
]


def tagged(tag, case) -> str:
    return "%s|%s|%s" % (RUN, tag, case["name"])


def create_batch(tag):
    vals = [dict(c, name=tagged(tag, c)) for c in BATCH]
    return env["res.partner"].create(vals)  # noqa: F821


def read_back(tag):
    import psycopg

    cols = ", ".join(COLUMNS)
    with psycopg.connect(conninfo) as conn, conn.cursor() as cur:
        cur.execute(
            "SELECT %s FROM res_partner WHERE name LIKE %%s ORDER BY name" % cols,
            ("%s|%s|%%" % (RUN, tag),),
        )
        return cur.fetchall()


failures = []

if type(env.cr._cnx).__name__ == "FakeConnection":  # noqa: F821
    print("COPY VACUOUS: the psycopg batch would be written through the rust cursor")
    sys.exit(1)
create_batch("PSYCOPG")
env.cr.commit()  # noqa: F821
reference = read_back("PSYCOPG")
print("COPY psycopg wrote %d rows" % len(reference))

db_shim, _orm_shim = engine_py.install_shims()
rust_db = engine_py.RustDb(conninfo)
db_shim.RUST_DB = rust_db
db_shim.CONNINFO = conninfo
db_shim.install()
db_shim.set_active(True)

with Registry(dbname).cursor() as cr:
    e = odoo.api.Environment(cr, 2, {})
    if type(cr._cnx).__name__ != "FakeConnection":
        failures.append(
            "the db shim did not take: cursor is %s" % type(cr._cnx).__name__
        )
    vals = [dict(c, name=tagged("RUST", c)) for c in BATCH]
    e["res.partner"].create(vals)
    cr.commit()
actual = read_back("RUST")
print("COPY rust    wrote %d rows" % len(actual))

JSON_CODES = [
    "%s%d" % (letter, digit) for letter in "XYZWQVUJKT" for digit in range(10)
][: max(15, COPY_THRESHOLD)]
JSON_CASES = [
    {"code": code, "name": "copy-country-%s \u00f1" % code} for code in JSON_CODES
]


def drop_countries() -> None:
    with Registry(dbname).cursor() as cr:
        cr.execute("DELETE FROM res_country WHERE code = ANY(%s)", (JSON_CODES,))
        cr.commit()


def create_countries(environment, tag):
    vals = [dict(c, name="%s|%s|%s" % (RUN, tag, c["name"])) for c in JSON_CASES]
    return environment["res.country"].create(vals)


def read_countries():
    import psycopg

    with psycopg.connect(conninfo) as conn, conn.cursor() as cur:
        cur.execute(
            "SELECT code, name FROM res_country WHERE code = ANY(%s) ORDER BY code",
            (JSON_CODES,),
        )
        return cur.fetchall()


json_rows = 0
drop_countries()
try:
    with Registry(dbname).cursor() as cr:
        e = odoo.api.Environment(cr, 2, {})
        create_countries(e, "RUST")
        cr.commit()
    got = read_countries()
    json_rows = len(got)
    if json_rows != len(JSON_CASES):
        failures.append(
            "jsonb: wrote %d countries, read back %d" % (len(JSON_CASES), json_rows)
        )
    by_code = {c["code"]: c["name"] for c in JSON_CASES}
    for code, name in got:
        want = "%s|%s|%s" % (RUN, "RUST", by_code[code])
        stored = name.get("en_US") if isinstance(name, dict) else name
        if stored != want:
            failures.append("jsonb %s: stored %r, asked for %r" % (code, stored, want))
finally:
    drop_countries()
print("COPY jsonb rows verified: %d" % json_rows)

if len(reference) != len(actual):
    failures.append("row count: psycopg %d, rust %d" % (len(reference), len(actual)))
else:
    for i, (want_row, got_row) in enumerate(zip(reference, actual, strict=True)):
        for col, want, got in zip(COLUMNS, want_row, got_row, strict=True):
            if col == "name":
                want, got = want.split("|", 2)[2], got.split("|", 2)[2]
            if want != got:
                failures.append(
                    "row %d column %s: psycopg %r, rust %r" % (i, col, want, got)
                )

copies = db_shim.pool_stats().get("copies", 0)
print(
    "COPY streams opened through the rust cursor: %d (threshold %d, %d rows)"
    % (copies, COPY_THRESHOLD, len(BATCH))
)
if len(BATCH) < COPY_THRESHOLD:
    failures.append(
        "only %d cases but COPY_THRESHOLD is %d; this never used COPY"
        % (len(BATCH), COPY_THRESHOLD)
    )
if copies < 1:
    failures.append(
        "the rust cursor opened no COPY stream; the create used INSERT and "
        "this test compared two things neither of which exercised the encoder"
    )

with Registry(dbname).cursor() as cr:
    cr.execute("DELETE FROM res_partner WHERE name LIKE %s", (RUN + "|%",))
    removed = cr.rowcount
    cr.commit()
print("COPY cleaned up %d rows" % removed)

for f in failures[:20]:
    print("  COPY MISMATCH %s" % f[:200])
print(
    "COPY %s (%d rows, %d columns each)"
    % ("OK" if not failures else "FAILED", len(actual), len(COLUMNS))
)
sys.exit(1 if failures else 0)
