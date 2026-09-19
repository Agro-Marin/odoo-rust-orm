#!/usr/bin/env python3

import argparse
import json
import os
import pathlib
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _env

CONTRACT = pathlib.Path(_env.harness_dir()) / "write_sql_contract.json"


def load():
    with CONTRACT.open() as fh:
        return json.load(fh)


class _Cursor:
    in_pipeline = True

    def __init__(self):
        self.seen = []

    def execute(self, sql):
        self.seen.append((sql.code, sql.params))

    def fetchall(self):
        return []


class _Env:
    def __init__(self):
        self.cr = _Cursor()


class _Field:
    is_html = False

    def __init__(self, name, cast, translate):
        self.name = name
        self.column_type = (cast.lower(), cast)
        self.is_column = True
        self.company_dependent = False
        self.translate = (
            True
            if translate == "whole"
            else (str.strip if translate == "term" else False)
        )

    def __repr__(self):
        return "Field(%s)" % self.name

    def convert_to_column_insert(self, value, *_args, **_kwargs):
        return value


class _Model:
    _name = "res.partner"

    def __init__(self, table, fields):
        self._table = table
        self._fields = fields
        self.env = _Env()


def compose(contract, case):
    from odoo.orm.runtime.backend import PostgresBackend

    fields = {
        name: _Field(name, spec["cast"], spec["translate"])
        for name, spec in contract["fields"].items()
    }
    model = _Model(contract["table"], fields)
    fnames = tuple(case["fields"])
    backend = PostgresBackend()
    if case["shape"] == "uniform":
        backend._update_rows_uniform(
            model, fnames, list(range(1, case["rows"] + 1)), tuple("?" for _ in fnames)
        )
    else:
        rows = [tuple([i] + ["?" for _ in fnames]) for i in range(1, case["rows"] + 1)]
        backend._update_rows_values(model, fnames, rows)
    return model.env.cr.seen[-1][0]


def compose_insert(contract, case):
    from odoo.orm.runtime.backend import PostgresBackend

    fields = {
        name: _Field(name, spec["cast"], spec["translate"])
        for name, spec in contract["fields"].items()
    }
    model = _Model(contract["table"], fields)
    columns = list(case["columns"])
    col_fields = [fields[name] for name in columns]
    stored_list = [dict.fromkeys(columns, "?") for _ in range(case["rows"])]
    PostgresBackend().create_rows(model, stored_list, columns, col_fields)
    return model.env.cr.seen[-1][0]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--update", action="store_true", help="rewrite the contract file"
    )
    args = parser.parse_args()

    sys.path.insert(
        0,
        os.environ.get("RUSTORM_ODOO_ROOT")
        or str(pathlib.Path(_env.workspace()) / "odoo"),
    )
    contract = load()
    drift = []
    for key, composer in (("cases", compose), ("insert_cases", compose_insert)):
        for case in contract[key]:
            got = composer(contract, case)
            if got != case["sql"]:
                drift.append(case["name"])
            case["sql"] = got
    if args.update:
        with CONTRACT.open("w") as fh:
            json.dump(contract, fh, indent=2, ensure_ascii=False)
            fh.write("\n")
        print(
            "CONTRACT updated (%d case(s) moved: %s)"
            % (len(drift), ", ".join(drift) or "none")
        )
        return 0
    if drift:
        print("CONTRACT DRIFTED: %s" % ", ".join(drift))
        print(
            "the fork composes something else now; --update after deciding it is intended"
        )
        return 1
    print(
        "CONTRACT OK (%d cases)"
        % (len(contract["cases"]) + len(contract["insert_cases"]))
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
