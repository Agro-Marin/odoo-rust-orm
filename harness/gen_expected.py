import json
import os
import pathlib
import sys

_HERE = (
    pathlib.Path(pathlib.Path(__file__).resolve()).parent
    if "__file__" in globals()
    else None
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
from _env import base_env, engine_python_dir, harness_dir, out_path

HARNESS = harness_dir()
CORPUS = os.environ.get("RUSTORM_CORPUS") or os.path.join(HARNESS, "corpus.json")
EXPECTED = out_path("expected.json", "RUSTORM_EXPECTED")

sys.path.insert(0, engine_python_dir())
from cases import run_case
from wire import ser as _ser

FINGERPRINT_SQL = """
SELECT coalesce(sum(n_tup_ins + n_tup_upd + n_tup_del), 0)
  FROM pg_stat_user_tables
 WHERE schemaname = current_schema AND relname = ANY(%s)
"""


def data_fingerprint(env, tables):
    try:
        env.cr.execute("SELECT pg_stat_force_next_flush()")
        env.cr.execute(FINGERPRINT_SQL, (sorted(tables),))
        return int(env.cr.fetchone()[0])
    except Exception:
        return None


def corpus_tables(env, corpus):
    out = set()
    for case in corpus:
        M = env.get(case["model"])
        if M is not None and getattr(M, "_table", None):
            out.add(M._table)
    return out


def domain_paths(domain, prefix, paths) -> None:
    for item in domain or []:
        if not isinstance(item, (list, tuple)):
            continue
        if len(item) == 3 and isinstance(item[0], str):
            head, operator, value = item
            paths.add(prefix + head)
            if operator in ("any", "not any") and isinstance(value, (list, tuple)):
                domain_paths(value, prefix + head + ".", paths)
        else:
            domain_paths(item, prefix, paths)


def referenced_paths(case):
    paths = set(case.get("fields") or [])
    gb = case.get("groupby") or []
    paths.update(g.split(":")[0] for g in ([gb] if isinstance(gb, str) else gb))
    for agg in case.get("aggregates") or []:
        if agg != "__count":
            paths.add(agg.split(":")[0])
    domain_paths(case.get("domain"), "", paths)
    for term in (case.get("order") or "").split(","):
        # a read_group order may name an aggregate: `amount:sum desc`, `__count`
        head = term.split()[0].split(":")[0] if term.strip() else ""
        if head and head != "__count":
            paths.add(head)
    return paths


CASE_KEYS = frozenset(
    {
        "id",
        "model",
        "method",
        "domain",
        "fields",
        "limit",
        "offset",
        "order",
        "groupby",
        "aggregates",
        "uid",
        "su",
        "lang",
        "allowed_company_ids",
        "groupby_labels",
        "active_test",
        "x2many_active_test",
        "tz",
        "note",
    }
)


def inapplicable(env, case) -> str | None:
    unknown = sorted(set(case) - CASE_KEYS)
    if unknown:
        raise ValueError("case %s carries unknown keys %s" % (case.get("id"), unknown))
    if case["model"] not in env.registry:
        return "model %s is not installed" % case["model"]
    uid = case.get("uid")
    if isinstance(uid, int) and not env["res.users"].sudo().browse(uid).exists():
        return "uid %s does not exist on this database" % uid
    companies = env["res.company"].sudo().browse(case.get("allowed_company_ids") or [])
    missing = set(companies.ids) - set(companies.exists().ids)
    if missing:
        return "company %s does not exist on this database" % sorted(missing)
    M = env[case["model"]]
    for path in referenced_paths(case):
        model = M
        segments = path.split(".")
        for i, name in enumerate(segments):
            if name in ("id", "display_name"):
                break
            field = model._fields.get(name)
            if field is None:
                return "field %s.%s does not exist" % (model._name, name)
            comodel = getattr(field, "comodel_name", None)
            if comodel in env.registry:
                model = env[comodel]
            elif i + 1 < len(segments):
                return "field %s.%s is not relational" % (model._name, name)
    return None


def main(env) -> None:
    env = base_env(env)
    corpus = json.loads(pathlib.Path(CORPUS).read_text(encoding="utf-8"))
    results = []
    for case in corpus:
        why = inapplicable(env, case)
        if why:
            results.append({"id": case["id"], "ok": False, "skipped": why})
            continue
        try:
            res = run_case(env, case)
            results.append({"id": case["id"], "ok": True, "result": _ser(res)})
        except Exception as e:
            results.append(
                {
                    "id": case["id"],
                    "ok": False,
                    "error": str(e),
                    "error_type": type(e).__name__,
                }
            )
        env.cr.rollback()
        env.invalidate_all()
    payload = {
        "db": env.cr.dbname,
        "data_fingerprint": data_fingerprint(env, corpus_tables(env, corpus)),
        "has_unaccent": bool(env.registry.has_unaccent),
        "cases": results,
    }
    with pathlib.Path(EXPECTED).open("w", encoding="utf-8") as f:
        json.dump(payload, f, indent=1, default=str)
    print("wrote %s (%d cases, db=%s)" % (EXPECTED, len(results), payload["db"]))


main(env)  # noqa: F821
