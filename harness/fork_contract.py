#!/usr/bin/env python3
import importlib
import os
import pathlib
import re
import sys

ROOT = pathlib.Path(pathlib.Path(pathlib.Path(__file__).resolve()).parent).parent
ODOO = os.environ.get("RUSTORM_ODOO_ROOT") or os.path.join(
    pathlib.Path(ROOT).parent, "odoo"
)

SOURCES = [
    os.path.join(ROOT, "addons", "rust_engine", "__init__.py"),
    os.path.join(ROOT, "engine-py", "python", "rust_db_shim.py"),
    os.path.join(ROOT, "engine-py", "python", "rust_orm_shim.py"),
    os.path.join(ROOT, "engine-py", "src", "export.rs"),
    os.path.join(ROOT, "addons", "rust_engine", "capture.py"),
    os.path.join(ROOT, "harness", "replay.py"),
    os.path.join(ROOT, "harness", "copy_path.py"),
    os.path.join(ROOT, "harness", "load_into_odoo.py"),
    os.path.join(ROOT, "harness", "bench_python.py"),
    os.path.join(ROOT, "harness", "gen_expected.py"),
    os.path.join(ROOT, "harness", "sweep_corpus.py"),
    os.path.join(ROOT, "harness", "fuzz_corpus.py"),
]

SEAMS = [
    ("odoo.db.pool", "ConnectionPool", ["borrow", "give_back"]),
    ("odoo.orm.runtime.registry", "Registry", ["new", "cursor"]),
    (
        "odoo.orm.models.base",
        "BaseModel",
        [
            "_search",
            "read",
            "search_read",
            "_read_group",
            "search_count",
            "_compute_display_name",
            "_search_display_name",
            "create",
            "write",
            "unlink",
        ],
    ),
    (
        "odoo.orm.fields",
        "Field",
        [
            "get_company_dependent_fallback",
            "related",
            "store",
            "context",
            "company_dependent",
            "comodel_name",
            "search",
            "groups",
            "index",
        ],
    ),
    ("odoo.fields", "Many2many", ["relation", "column1", "column2"]),
    ("odoo.fields", "One2many", ["inverse_name"]),
    ("odoo.fields", "Many2oneReference", ["model_field"]),
    ("odoo.orm.runtime.backend", None, ["COPY_THRESHOLD"]),
    (
        "odoo.orm.runtime._search_flush",
        "_DependencyCollector",
        ["collect_domain", "collect_order", "collect_field"],
    ),
    (
        "odoo.orm.models.base",
        "BaseModel",
        [
            "_get_display_name_visible_ids",
            "_field_to_sql",
            "_order_to_sql",
            "_order_field_to_sql",
            "_read_group_select",
            "_read_group_groupby",
            "_read_group_orderby",
            "_read_group_having",
            "_read_group_postprocess_aggregate",
            "_read_group_postprocess_groupby",
            "search_fetch",
            "fetch",
            "_fetch_query",
            "_active_name",
        ],
    ),
    ("odoo.orm.fields", "Many2one", ["bypass_search_access"]),
    ("odoo.db", None, ["get_connection_info_for_database"]),
]

FROM_RE = re.compile(r"^\s*from\s+(odoo[\w.]*)\s+import\s+([\w, ]+)", re.MULTILINE)
IMPORT_RE = re.compile(r"^\s*import\s+(odoo[\w.]*)", re.MULTILINE)


def collect():
    wanted = {}
    for path in SOURCES:
        if not pathlib.Path(path).exists():
            continue
        text = pathlib.Path(path).read_text(encoding="utf-8")
        rel = os.path.relpath(path, ROOT)
        for mod, names in FROM_RE.findall(text):
            for name in names.split(","):
                name = name.strip().split(" as ")[0].strip()
                if name:
                    wanted.setdefault((mod, name), rel)
        for mod in IMPORT_RE.findall(text):
            wanted.setdefault((mod, None), rel)
    return wanted


def main() -> int:
    sys.path.insert(0, ODOO)
    try:
        importlib.import_module("odoo")
        import odoo.addons

        odoo.addons.__path__.append(os.path.join(ODOO, "addons"))
    except ImportError as exc:
        print("FORK SKIP: odoo is not importable from %s (%s)" % (ODOO, exc))
        return 3

    failures = []
    wanted = collect()
    for (mod, name), rel in sorted(
        wanted.items(), key=lambda kv: (kv[0][0], kv[0][1] or "")
    ):
        try:
            module = importlib.import_module(mod)
        except Exception as exc:
            failures.append("%s: import %s -> %s" % (rel, mod, exc))
            continue
        if name and not hasattr(module, name):
            try:
                importlib.import_module(mod + "." + name)
            except ImportError:
                failures.append("%s: from %s import %s -> absent" % (rel, mod, name))

    for mod, holder, attrs in SEAMS:
        try:
            target = importlib.import_module(mod)
            if holder:
                target = getattr(target, holder)
        except Exception as exc:
            failures.append("seam %s.%s -> %s" % (mod, holder or "", exc))
            continue
        failures.extend(
            "seam %s.%s.%s -> absent" % (mod, holder or "", attr)
            for attr in attrs
            if not hasattr(target, attr)
        )

    for failure in failures:
        print("  FAIL %s" % failure)
    print(
        "FORK %s (%d imports, %d seams)"
        % (
            "OK" if not failures else "FAILED (%d)" % len(failures),
            len(wanted),
            sum(len(a) for _, _, a in SEAMS),
        )
    )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
