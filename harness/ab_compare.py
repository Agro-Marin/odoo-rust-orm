"""Compare two databases table by table after the same committed workload.

    ab_compare.py DB_A DB_B [--ignore-tables t1,t2] [--json OUT]

Every base table in `public` of A is read in both, ordered by `id` (or by
every column when there is none), with the volatile columns left out:
`create_date`, `write_date`, every timestamp column (a workload's clock is
not the same on two runs), and the tables that record a process rather than
a business fact. Exit 0 when every remaining cell agrees.
"""

import argparse
import json
import pathlib
import sys

import psycopg

SKIP_TABLES = {
    "ir_logging",
    "bus_bus",
    "ir_sessions",
    "ir_cron",
    "ir_cron_trigger",
    "ir_attachment",
    "ir_asset",
    "ir_config_parameter",
    "res_users_apikeys",
    "mail_notification",
    "mail_mail",
    "web_editor_converter_test",
}
SKIP_COLUMNS = {
    "create_date",
    "write_date",
    "login_date",
    "last_update",
    # random per run, measured by the python-vs-python control
    "message_id",
    "access_token",
    "html_field_history",
}


def columns(cur, table):
    cur.execute(
        "SELECT column_name, data_type FROM information_schema.columns "
        "WHERE table_schema = 'public' AND table_name = %s ORDER BY ordinal_position",
        (table,),
    )
    return [
        c
        for c, t in cur.fetchall()
        if c not in SKIP_COLUMNS and not t.startswith("timestamp")
    ]


def rows(cur, table, cols, has_id):
    order = '"id"' if has_id else ", ".join('"%s"' % c for c in cols)
    cur.execute(
        "SELECT %s FROM %s ORDER BY %s"
        % (", ".join('"%s"' % c for c in cols), '"%s"' % table, order)
    )
    return cur.fetchall()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("db_a")
    ap.add_argument("db_b")
    ap.add_argument("--ignore-tables", default="")
    ap.add_argument("--json", default="")
    args = ap.parse_args()
    skip = SKIP_TABLES | set(filter(None, args.ignore_tables.split(",")))
    dsn = "host=/var/run/postgresql user=marin dbname=%s"
    report = {"tables": 0, "rows": 0, "cells": 0, "differences": []}
    with (
        psycopg.connect(dsn % args.db_a) as a,
        psycopg.connect(dsn % args.db_b) as b,
        a.cursor() as ca,
        b.cursor() as cb,
    ):
        # a comparison of two snapshots nothing was committed to reads as a
        # perfect agreement; the workload's own rows must be in both
        for cur, name in ((ca, args.db_a), (cb, args.db_b)):
            cur.execute(
                "SELECT count(*) FROM res_partner WHERE name LIKE 'AB Partner%%'"
            )
            if not cur.fetchone()[0]:
                print("AB COMPARE VACUOUS: no workload rows in %s" % name)
                return 1
        ca.execute(
            "SELECT table_name FROM information_schema.tables "
            "WHERE table_schema = 'public' AND table_type = 'BASE TABLE' ORDER BY 1"
        )
        tables = [t for (t,) in ca.fetchall() if t not in skip]
        for table in tables:
            cols = columns(ca, table)
            if not cols or columns(cb, table) != cols:
                report["differences"].append(
                    {"table": table, "kind": "columns differ or table absent"}
                )
                continue
            has_id = "id" in cols
            ra = rows(ca, table, cols, has_id)
            rb = rows(cb, table, cols, has_id)
            report["tables"] += 1
            if len(ra) != len(rb):
                report["differences"].append(
                    {"table": table, "kind": "row count", "a": len(ra), "b": len(rb)}
                )
            shown = 0
            for x, y in zip(ra, rb, strict=False):
                report["rows"] += 1
                report["cells"] += len(cols)
                if x != y:
                    diff = [
                        (c, str(u)[:60], str(v)[:60])
                        for c, u, v in zip(cols, x, y, strict=True)
                        if u != v
                    ]
                    if shown < 3:
                        report["differences"].append(
                            {
                                "table": table,
                                "kind": "cells",
                                "id": x[cols.index("id")] if has_id else None,
                                "diff": diff,
                            }
                        )
                    shown += 1
            if shown > 3:
                report["differences"].append(
                    {"table": table, "kind": "more rows differ", "count": shown - 3}
                )
    if args.json:
        with pathlib.Path(args.json).open("w", encoding="utf-8") as fh:
            json.dump(report, fh, indent=1, default=str)
    for d in report["differences"][:40]:
        print("  DIFF %s" % json.dumps(d, default=str)[:300])
    verdict = "OK" if not report["differences"] else "FAILED"
    print(
        "AB COMPARE %s: %d tables, %d rows, %d cells, %d differences"
        % (
            verdict,
            report["tables"],
            report["rows"],
            report["cells"],
            len(report["differences"]),
        )
    )
    return 0 if verdict == "OK" else 1


if __name__ == "__main__":
    sys.exit(main())
