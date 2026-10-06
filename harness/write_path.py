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

if not os.environ.get("PYTHONPATH"):
    print("WRITE SKIP: no PYTHONPATH; engine_py must be importable")
    sys.exit(3)

import engine_py

dbname = env.cr.dbname  # noqa: F821
conninfo = dsn_for(dbname)
RUN = "writetest-%d" % os.getpid()

COLUMNS = ("name", "ref", "comment", "color", "is_company", "partner_latitude")
CASES = [
    {
        "name": "%s-plain" % RUN,
        "ref": "W1",
        "comment": "hello",
        "color": 3,
        "is_company": True,
        "partner_latitude": 0.1 + 0.2,
    },
    {
        "name": "%s-empty" % RUN,
        "ref": "",
        "comment": "",
        "color": 0,
        "is_company": False,
        "partner_latitude": -89.999999,
    },
    {
        "name": "%s-null" % RUN,
        "ref": None,
        "comment": None,
        "color": -7,
        "is_company": False,
        "partner_latitude": 0.0,
    },
    {
        "name": "%s-unicode \u00f1 \u65e5\u672c\u8a9e \U0001f30d" % RUN,
        "ref": "W4",
        "comment": "\u00fc\nnewline\ttab",
        "color": 2147483647,
        "is_company": True,
        "partner_latitude": 12.5,
    },
    {
        "name": "%s-quote'and\"double" % RUN,
        "ref": "W5",
        "comment": "back\\slash",
        "color": 1,
        "is_company": False,
        "partner_latitude": -0.000001,
    },
]
UNIFORM = {"function": "uniform-%s" % RUN}


def seed():
    model = env["res.partner"]  # noqa: F821
    recs = model.create([{"name": "%s-seed-%d" % (RUN, i)} for i in range(len(CASES))])
    env.cr.flush()  # noqa: F821
    for rec, case in zip(recs, CASES, strict=True):
        rec.write(dict(case))
    env.cr.flush()  # noqa: F821
    recs.write(UNIFORM)
    env.cr.flush()  # noqa: F821
    return recs


def stored(recs):
    import psycopg

    cols = ", ".join(COLUMNS)
    with psycopg.connect(conninfo) as conn, conn.cursor() as cur:
        cur.execute(
            "SELECT %s, function FROM res_partner WHERE id = ANY(%%s) ORDER BY id"
            % cols,
            (list(recs.ids),),
        )
        return cur.fetchall()


port = engine_py.install_backend()
import rust_orm_shim

rust_orm_shim.set_mode("on")
failures = []

if port.installed() is None:
    print("WRITE SKIP: the port is not installed in this process")
    sys.exit(3)

backend = env.cr.transaction.backend  # noqa: F821
if type(backend).__name__ != "RustBackend":
    print(
        "WRITE SKIP: env.backend is %s, so this database is not armed"
        % type(backend).__name__
    )
    sys.exit(3)

port.reset_stats()
native_recs = seed()
env.cr.commit()  # noqa: F821
after_native = port.stats()
native_rows = stored(native_recs)

ran = after_native["native"].get("update_rows", 0)
print("WRITE native update_rows: %d (delegated %s)" % (ran, after_native["delegated"]))
if not ran:
    failures.append(
        "the native path never ran, so the comparison below is python against "
        "python. reasons: %r" % (after_native["reasons"],)
    )
failures.extend(
    "the %s statement was never taken: %r" % (shape, after_native["native"])
    for shape in ("update_rows.uniform", "update_rows.values")
    if not after_native["native"].get(shape)
)

armed = port.RustBackend.NATIVE
port.RustBackend.NATIVE = frozenset()
try:
    port.reset_stats()
    python_recs = seed()
    env.cr.commit()  # noqa: F821
    after_python = port.stats()
    python_rows = stored(python_recs)
finally:
    port.RustBackend.NATIVE = armed

if after_python["native"]:
    failures.append("the control leg routed natively: %r" % (after_python["native"],))

print("WRITE rows compared: %d" % len(native_rows))
if len(native_rows) != len(python_rows) or len(native_rows) != len(CASES):
    failures.append(
        "row counts: native %d python %d cases %d"
        % (len(native_rows), len(python_rows), len(CASES))
    )
for index, (got, want) in enumerate(zip(native_rows, python_rows, strict=False)):
    for col, a, b in zip((*COLUMNS, "function"), got, want, strict=True):
        if a != b:
            failures.append(
                "row %d column %s: native %r python %r" % (index, col, a, b)
            )

MERGE_MODEL = "mail.activity.type"
MERGE_FIELD = "summary"
if MERGE_MODEL not in env:  # noqa: F821
    print(
        "WRITE NOTE: %s is not installed here; the merge is unexercised" % MERGE_MODEL
    )
    failures.append("no translated model on this database to exercise the merge")
else:
    Merge = env[MERGE_MODEL]  # noqa: F821
    field = Merge._fields[MERGE_FIELD]
    if field.translate is not True:
        failures.append(
            "%s.%s is not whole-value translated here (translate=%r), so the "
            "merge is unexercised" % (MERGE_MODEL, MERGE_FIELD, field.translate)
        )
    Merge.search([("name", "=like", "writetest-%")]).unlink()
    env.cr.commit()  # noqa: F821
    rows = Merge.create(
        [
            {"name": "%s-merge-a" % RUN, MERGE_FIELD: "%s-a" % RUN},
            {"name": "%s-merge-b" % RUN, MERGE_FIELD: "%s-b" % RUN},
        ]
    )
    env.cr.flush()  # noqa: F821
    port.reset_stats()
    rows[0].with_context(lang="en_US").write({MERGE_FIELD: "%s-en-a" % RUN})
    env.cr.flush()  # noqa: F821
    rows.with_context(lang="en_US").write({MERGE_FIELD: "%s-en-both" % RUN})
    env.cr.flush()  # noqa: F821
    env.cr.commit()  # noqa: F821
    after_merge = port.stats()

    import psycopg

    with psycopg.connect(conninfo) as conn, conn.cursor() as cur:
        cur.execute(
            "SELECT id, %s FROM %s WHERE id = ANY(%%s) ORDER BY id"
            % (MERGE_FIELD, Merge._table),
            (list(rows.ids),),
        )
        merged = cur.fetchall()
    print("WRITE translated column after the merge: %r" % ([row[1] for row in merged],))
    print("WRITE the merge ran natively: %r" % (after_merge["native"],))
    if not after_merge["native"].get("update_rows.values"):
        failures.append(
            "the translated merge never routed: %r" % (after_merge["native"],)
        )
    if after_merge["native"].get("update_rows.uniform"):
        failures.append(
            "a translated column took the uniform statement with a non-null value, "
            "which _UNIFORM_UPDATE_TYPES should have prevented: %r"
            % (after_merge["native"],)
        )
    for id_, langs in merged:
        if not isinstance(langs, dict):
            failures.append("%s(%d) is not jsonb here: %r" % (MERGE_MODEL, id_, langs))
        elif langs.get("en_US") != "%s-en-both" % RUN:
            failures.append(
                "%s(%d) lost its en_US value: %r" % (MERGE_MODEL, id_, langs)
            )

    port.reset_stats()
    rows.with_context(lang="en_US").write({MERGE_FIELD: False})
    env.cr.flush()  # noqa: F821
    env.cr.commit()  # noqa: F821
    after_null = port.stats()
    print("WRITE clearing it ran natively: %r" % (after_null["native"],))
    if not after_null["native"].get("update_rows.uniform"):
        failures.append(
            "the uniform statement was never taken with a translated column: %r"
            % (after_null["native"],)
        )
    with psycopg.connect(conninfo) as conn, conn.cursor() as cur:
        cur.execute(
            "SELECT id, %s FROM %s WHERE id = ANY(%%s) ORDER BY id"
            % (MERGE_FIELD, Merge._table),
            (list(rows.ids),),
        )
        cleared = cur.fetchall()
    print("WRITE translated column after the clear: %r" % ([r[1] for r in cleared],))
    failures.extend(
        "%s(%d) should be NULL after the clear: %r" % (MERGE_MODEL, id_, langs)
        for id_, langs in cleared
        if langs is not None
    )


def create_leg(tag, count):
    model = env["res.partner"]  # noqa: F821
    vals = [
        dict(CASES[i % len(CASES)], name="%s-%s-%d" % (RUN, tag, i))
        for i in range(count)
    ]
    recs = model.create(vals)
    env.cr.flush()  # noqa: F821
    env.cr.commit()  # noqa: F821
    return vals, recs


def by_id(recs):
    import psycopg

    cols = ", ".join(COLUMNS)
    with psycopg.connect(conninfo) as conn, conn.cursor() as cur:
        cur.execute(
            "SELECT id, %s FROM res_partner WHERE id = ANY(%%s)" % cols,
            (list(recs.ids),),
        )
        return {row[0]: row[1:] for row in cur.fetchall()}


VERBATIM = ("name", "ref", "color", "is_company")


def check_created(label, vals, recs):
    stored = by_id(recs)
    if len(stored) != len(vals):
        failures.append(
            "%s: %d rows stored for %d created" % (label, len(stored), len(vals))
        )
        return
    for id_, want in zip(recs.ids, vals, strict=True):
        got = stored.get(id_)
        if got is None:
            failures.append("%s: id %d was returned and not stored" % (label, id_))
            continue
        got = dict(zip(COLUMNS, got, strict=True))
        failures.extend(
            "%s: id %d column %s holds %r, created from %r"
            % (label, id_, col, got[col], want[col])
            for col in VERBATIM
            if got[col] != want[col]
        )


from odoo.orm.runtime.backend import COPY_THRESHOLD

for small, strategy in (
    (min(5, COPY_THRESHOLD - 1), "INSERT"),
    (COPY_THRESHOLD + 2, "COPY"),
):
    port.reset_stats()
    vals, recs = create_leg("create%d" % small, small)
    after = port.stats()
    native = after["native"].get("create_rows", 0)
    print(
        "WRITE create of %d (%s): native create_rows %d, reasons %r"
        % (
            small,
            strategy,
            native,
            {k: v for k, v in after["reasons"].items() if "COPY" in k or "create" in k},
        )
    )
    check_created("create of %d" % small, vals, recs)
    if strategy == "INSERT" and not native:
        failures.append(
            "a create of %d rows never reached the kernel's INSERT: %r"
            % (small, after["reasons"])
        )
    if strategy == "COPY":
        if native:
            failures.append(
                "a create of %d rows was answered natively instead of by COPY" % small
            )
        if not after["reasons"].get("COPY strategy: the cursor owns it"):
            failures.append(
                "a create of %d rows did not report the COPY delegation: %r"
                % (small, after["reasons"])
            )

port.reset_stats()
model = env["res.partner"]  # noqa: F821
piped_vals = [
    dict(CASES[i % len(CASES)], name="%s-piped-%d" % (RUN, i))
    for i in range(COPY_THRESHOLD + 2)
]
with env.cr.pipeline():  # noqa: F821
    piped = model.create(piped_vals)
    entered_pipeline = env.cr.in_pipeline  # noqa: F821
env.cr.flush()  # noqa: F821
env.cr.commit()  # noqa: F821
after_piped = port.stats()
print(
    "WRITE create of %d in a pipeline (%s): native create_rows %d, reasons %r"
    % (
        len(piped_vals),
        "entered"
        if entered_pipeline
        else "not entered: the cursor has no pipeline mode",
        after_piped["native"].get("create_rows", 0),
        after_piped["reasons"],
    )
)
check_created("create of %d in a pipeline" % len(piped_vals), piped_vals, piped)
if entered_pipeline and not after_piped["native"].get("create_rows"):
    failures.append(
        "a create of %d rows in a pipeline did not reach the kernel's INSERT: %r"
        % (len(piped_vals), after_piped["reasons"])
    )
if not entered_pipeline and not after_piped["reasons"].get(
    "COPY strategy: the cursor owns it"
):
    failures.append(
        "a create of %d rows outside pipeline mode did not report the COPY delegation: %r"
        % (len(piped_vals), after_piped["reasons"])
    )

armed = port.RustBackend.NATIVE
port.RustBackend.NATIVE = frozenset()
try:
    port.reset_stats()
    control_vals, control_recs = create_leg("control", 5)
finally:
    port.RustBackend.NATIVE = armed
port.reset_stats()
native_vals, native_recs2 = create_leg("armed", 5)
if not port.stats()["native"].get("create_rows"):
    failures.append("the armed control leg never reached the kernel")
armed_rows = [by_id(native_recs2)[i] for i in native_recs2.ids]
control_rows = [by_id(control_recs)[i] for i in control_recs.ids]
for index, (a, b) in enumerate(zip(armed_rows, control_rows, strict=True)):
    for col, x, y in zip(COLUMNS, a, b, strict=True):
        if col == "name":
            x, y = x.replace("-armed-", "-"), y.replace("-control-", "-")
        if x != y:
            failures.append(
                "created row %d column %s: native %r python %r" % (index, col, x, y)
            )
print("WRITE created rows compared with the python control: %d" % len(armed_rows))

print("WRITE %s" % ("OK" if not failures else "FAILED (%d)" % len(failures)))
for line in failures:
    print("  WRITE MISMATCH %s" % line)
sys.exit(1 if failures else 0)
