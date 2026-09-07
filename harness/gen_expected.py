import json
import sys

import os

HARNESS = os.environ.get("RUSTPOC_HARNESS", "/home/marin/Odoo/odoo-rust-orm/harness")
CORPUS = os.environ.get("RUSTPOC_CORPUS", os.path.join(HARNESS, "corpus.json"))
EXPECTED = os.environ.get("RUSTPOC_EXPECTED", os.path.join(HARNESS, "expected.json"))


sys.path.insert(0, os.path.join(os.path.dirname(HARNESS), "engine-py", "python"))
from wire import ser as _ser  # noqa: E402

sys.path.insert(0, HARNESS)
from cases import run_case  # noqa: E402


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




def main(env):
    env = env(user=2, su=False)
    env = env(context=dict(env.context, lang="en_US"))
    corpus = json.load(open(CORPUS))
    results = []
    for case in corpus:
        try:
            res = run_case(env, case)
            results.append({"id": case["id"], "ok": True, "result": _ser(res)})
        except Exception as e:  # noqa: BLE001
            results.append({
                "id": case["id"],
                "ok": False,
                "error": str(e),
                "error_type": type(e).__name__,
            })
        env.cr.rollback()
        env.invalidate_all()
    payload = {
        "db": env.cr.dbname,
        "data_fingerprint": data_fingerprint(env, corpus_tables(env, corpus)),
        "has_unaccent": bool(env.registry.has_unaccent),
        "cases": results,
    }
    with open(EXPECTED, "w") as f:
        json.dump(payload, f, indent=1, default=str)
    print("wrote %s (%d cases, db=%s)" % (EXPECTED, len(results), payload["db"]))


main(env)  # noqa: F821  (env comes from the odoo shell namespace)
