"""The port's `update_rows`, differentially against the backend it replaces.

`kernel/tests/pure.rs` and `test_shims.py` pin the statement TEXT from both
sides, which is the strong half of this. What they cannot see is the row: a
statement that reads correctly and binds its parameters in the wrong order
writes the wrong value without erroring, and a translated column merged the
wrong way loses a language rather than raising.

So this writes the same values twice on one database -- once through the
kernel's statement and once through the fork's -- and compares what PostgreSQL
stored, read back by an INDEPENDENT connection that neither path touched.

It also proves the path RAN. A run where the port delegated every call would
compare two identical Python writes and pass while exercising nothing, which
is the failure the copy encoder's stream counter exists to catch as well.
"""

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
    sys.exit(0)

import engine_py

dbname = env.cr.dbname  # noqa: F821
conninfo = dsn_for(dbname)
RUN = "writetest-%d" % os.getpid()

# Chosen for where a length-ordered parameter list goes wrong. `function` is
# written to every record of a leg so its group takes the UNIFORM statement,
# and everything in CASES differs per row so those take the VALUES join.
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
    """Create the records bare, then WRITE every column.

    Creating them with the values would exercise `create_rows`, which this
    port does not implement; what is under test is the update.
    """
    model = env["res.partner"]  # noqa: F821
    recs = model.create([{"name": "%s-seed-%d" % (RUN, i)} for i in range(len(CASES))])
    env.cr.flush()  # noqa: F821
    for rec, case in zip(recs, CASES, strict=True):
        rec.write(dict(case))
    # Flushed on its own, because the uniform statement is only chosen when
    # every row of a column-group carries the SAME values: leaving these in
    # the same flush as the per-row writes above makes one non-uniform group
    # and the uniform statement is then never composed at all.
    env.cr.flush()  # noqa: F821
    recs.write(UNIFORM)
    env.cr.flush()  # noqa: F821
    return recs


def stored(recs):
    """Read the rows back on a connection neither write path touched."""
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
failures = []

if port.installed() is None:
    print("WRITE SKIP: the port is not installed in this process")
    sys.exit(0)

backend = env.cr.transaction.backend  # noqa: F821
if type(backend).__name__ != "RustBackend":
    print(
        "WRITE SKIP: env.backend is %s, so this database is not armed"
        % type(backend).__name__
    )
    sys.exit(0)

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
# Both statements have to have RUN. They are composed differently and bind
# their parameters in different orders, so a leg that only ever took one of
# them leaves the other unverified while still reporting a clean comparison.
failures.extend(
    "the %s statement was never taken: %r" % (shape, after_native["native"])
    for shape in ("update_rows.uniform", "update_rows.values")
    if not after_native["native"].get(shape)
)

# The same writes with the port disarmed, so the fork's own backend composes.
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

# The whole-value translated merge, which no comparison above reaches:
# res.partner has no such column, so this runs on a model that has one. The
# assignment MERGES into the languages already stored rather than replacing
# them, and a wrong merge loses one silently.
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
    # `summary` rather than `name`: the clearing step below needs a column
    # that can be NULL, and `name` on this model cannot.
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
    # The VALUES statement only, and that is not an oversight. A whole-value
    # translated column's update value is a `PsycopgJson` wrapper, which
    # `_UNIFORM_UPDATE_TYPES` does not list -- so a group holding one is never
    # uniform unless its value is NULL on every row. That case is below, and
    # it is the ONLY path on which the merge's three-times-bound parameter
    # runs, so the harness has to say which of the two it exercised.
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

    # NULL on every row is where a translated column DOES take the uniform
    # statement, so it is the one place the three-parameter binding is
    # exercised. Built for one occurrence instead, the id array lands inside
    # the CASE and PostgreSQL rejects it -- in production rather than here.
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

# --------------------------------------------------------------------------
# create_rows
#
# The same comparison for creates, and here the ids matter as much as the
# values: `create()` pairs the ids `INSERT ... RETURNING "id"` hands back with
# the values it sent, in order, and fills the cache from that pairing. An id
# list in the wrong order gives every record its neighbour's values in the
# cache while the table is right, which no read-back by id range would see.
# So each record is read back BY ITS OWN ID and compared with the case it was
# created from.
#
# Five rows take the INSERT strategy, which the kernel composes. Twelve take
# COPY, which the cursor owns: that leg must DELEGATE, and says so, because a
# create_rows counted native for a COPY would mean the strategy split moved.
# --------------------------------------------------------------------------


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


# The columns a create stores exactly as given. `comment` is Html, so the
# sanitizer wraps it, and `partner_latitude` is a rounded numeric that reads
# back as a Decimal: both are converted by the ORM before either strategy sees
# them, so comparing them to the INPUT tests the converter, not the write.
# They are compared against the python control below instead, where both legs
# went through the same conversion. `name` is unique per record, which is what
# makes this the check on the id pairing.
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


for small, strategy in ((5, "INSERT"), (12, "COPY")):
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

# Twelve rows INSIDE a pipeline: the fork takes the INSERT strategy there
# whatever the row count, because COPY cannot run in pipeline mode. It is the
# one create above the threshold the kernel composes, and the one place the
# strategy split is decided by the cursor's state rather than the row count.
port.reset_stats()
model = env["res.partner"]  # noqa: F821
piped_vals = [
    dict(CASES[i % len(CASES)], name="%s-piped-%d" % (RUN, i)) for i in range(12)
]
with env.cr.pipeline():  # noqa: F821
    piped = model.create(piped_vals)
env.cr.flush()  # noqa: F821
env.cr.commit()  # noqa: F821
after_piped = port.stats()
print(
    "WRITE create of 12 in a pipeline: native create_rows %d, reasons %r"
    % (after_piped["native"].get("create_rows", 0), after_piped["reasons"])
)
check_created("create of 12 in a pipeline", piped_vals, piped)
if not after_piped["native"].get("create_rows"):
    failures.append(
        "a create of 12 rows in a pipeline did not reach the kernel's INSERT: %r"
        % (after_piped["reasons"],)
    )

# the same creates with the port disarmed, compared row for row with the armed
# ones -- the control for anything the by-id check above cannot see, such as a
# column the ORM fills in on its own
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
