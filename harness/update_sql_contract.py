#!/usr/bin/env python3
"""Compose `update_rows`' statement with the FORK's backend, for the contract.

`update_sql_contract.json` is read by two tests -- `test_shims.py` here and
`kernel/tests/pure.rs` in the kernel -- so that the statement the kernel
composes and the statement Odoo composes are pinned to one literal rather than
to copies of each other. This module is the half that asks Odoo.

    harness/update_sql_contract.py            print what the fork composes now
    harness/update_sql_contract.py --update   rewrite the contract file

`--update` is for an INTENDED change to the fork's composition: it makes the
Python test green again and the Rust one red until the kernel is taught the
same statement, which is the order the two should move in.
"""

import argparse
import json
import os
import pathlib
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _env

CONTRACT = pathlib.Path(_env.harness_dir()) / "update_sql_contract.json"


def load():
    with CONTRACT.open() as fh:
        return json.load(fh)


class _Cursor:
    def __init__(self):
        self.seen = []

    def execute(self, sql):
        self.seen.append((sql.code, sql.params))


class _Env:
    def __init__(self):
        self.cr = _Cursor()


class _Field:
    """Only the attributes `_update_assignments` reads.

    A stub rather than a real field because the contract is about the SHAPE of
    a column -- its declared cast and how it is translated -- and building a
    real registry for four shapes would tie the contract to whichever database
    happened to be around.
    """

    def __init__(self, name, cast, translate):
        self.name = name
        self.column_type = (cast.lower(), cast)
        self.is_column = True
        self.company_dependent = False
        # `translate is True` is the whole-value case the assignment merges;
        # a callable is the term-translated one it replaces whole.
        self.translate = (
            True
            if translate == "whole"
            else (str.strip if translate == "term" else False)
        )

    def __repr__(self):
        return "Field(%s)" % self.name


class _Model:
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--update", action="store_true", help="rewrite the contract file"
    )
    args = parser.parse_args()

    sys.path.insert(0, str(pathlib.Path(_env.workspace()) / "odoo"))
    contract = load()
    drift = []
    for case in contract["cases"]:
        got = compose(contract, case)
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
    print("CONTRACT OK (%d cases)" % len(contract["cases"]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
