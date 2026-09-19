import collections
import json
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
from _env import base_env, harness_dir

if not os.environ.get("PYTHONPATH"):
    print("SEARCH SKIP: no PYTHONPATH; engine_py must be importable")
    sys.exit(3)

import engine_py
from cases import case_env

CORPUS = os.environ.get("RUSTORM_SWEEP") or os.path.join(harness_dir(), "corpus.json")
MIN_NATIVE = int(os.environ.get("RUSTORM_MIN_NATIVE_SEARCH", "1000"))

port = engine_py.install_backend()
import rust_orm_shim

rust_orm_shim.set_mode("on")
backend_name = type(env.cr.transaction.backend).__name__  # noqa: F821
if port.installed() is None or backend_name != "RustBackend":
    print("SEARCH SKIP: the port is not installed for this database")
    sys.exit(3)

ORIGINAL = port.RustBackend.NATIVE
ARMED = ORIGINAL | {"search", "search_raw"}
DISARMED = port.RustBackend.NATIVE - {"search", "search_raw"}


def leg(model, domain, order, limit, *, armed):
    port.RustBackend.NATIVE = ARMED if armed else DISARMED
    port.reset_stats()
    query = model._search(domain, limit=limit, order=order)
    native = port.stats()["native"].get("search", 0) + port.stats()["native"].get(
        "search_raw", 0
    )
    reasons = port.stats()["reasons_by_method"].get("search", {})
    ids = list(query.get_result_ids())
    count = model._search(domain).count_matching() if limit is None else None
    return query, ids, count, native, reasons


def flush_names(query):
    return {"%s.%s" % (f.model_name, f.name) for f in query.where_clause.to_flush}


with pathlib.Path(CORPUS).open(encoding="utf-8") as fh:
    corpus = json.load(fh)

base = base_env(env)  # noqa: F821
seen = set()
tally = collections.Counter()
delegations = collections.Counter()
failures = []

for case in corpus:
    key = json.dumps(
        [
            case["model"],
            case.get("domain"),
            case.get("order"),
            case.get("limit"),
            case.get("uid"),
            case.get("su"),
            case.get("lang"),
            case.get("active_test"),
            case.get("allowed_company_ids"),
            case.get("tz"),
        ],
        sort_keys=True,
    )
    if key in seen:
        continue
    seen.add(key)
    try:
        cenv = case_env(base, case)
    except ValueError:
        tally["skipped (identity not seeded)"] += 1
        continue
    if case["model"] not in cenv:
        tally["skipped (model not installed)"] += 1
        continue
    model = cenv[case["model"]]
    domain = case.get("domain") or []
    order = case.get("order")
    limit = case.get("limit")

    outcomes = {}
    for armed in (True, False):
        try:
            with cenv.cr.savepoint():
                outcomes[armed] = ("ok", leg(model, domain, order, limit, armed=armed))
        except Exception as exc:
            outcomes[armed] = (
                "raised",
                "%s: %s" % (type(exc).__name__, str(exc)[:120]),
            )
    port.RustBackend.NATIVE = ORIGINAL

    (nk, native_out), (pk, python_out) = outcomes[True], outcomes[False]
    if nk == "raised" and pk == "raised":
        tally["both raised"] += 1
        continue
    if nk != pk:
        failures.append(
            "%s %s: native %s, python %s"
            % (
                case["id"],
                case["model"],
                native_out if nk == "raised" else "answered",
                python_out if pk == "raised" else "answered",
            )
        )
        continue

    n_query, n_ids, n_count, ran, reasons = native_out
    p_query, p_ids, p_count, _, _ = python_out
    if not ran:
        tally["delegated"] += 1
        for reason, n in reasons.items():
            delegations[reason] += n
        continue
    tally["native"] += 1

    same = n_ids == p_ids if order else sorted(n_ids) == sorted(p_ids)
    if order is None and limit is not None:
        same = len(n_ids) == len(p_ids)
    if not same and order:
        total = order + ", id"
        with cenv.cr.savepoint():
            _, n2, _, _, _ = leg(model, domain, total, limit, armed=True)
            _, p2, _, _, _ = leg(model, domain, total, limit, armed=False)
        if n2 == p2:
            tally["tie under the order"] += 1
            same = True
    if not same:
        failures.append(
            "%s %s %s order=%r limit=%r: native %d ids %s..., python %d ids %s..."
            % (
                case["id"],
                case["model"],
                json.dumps(domain)[:120],
                order,
                limit,
                len(n_ids),
                n_ids[:5],
                len(p_ids),
                p_ids[:5],
            )
        )
        continue
    if n_count != p_count:
        failures.append(
            "%s %s: count_matching native %r python %r"
            % (case["id"], case["model"], n_count, p_count)
        )
        continue

    missing = flush_names(p_query) - flush_names(n_query)
    if missing:
        failures.append(
            "%s %s: the native WHERE does not flush %s"
            % (case["id"], case["model"], sorted(missing))
        )
        continue

    try:
        with cenv.cr.savepoint():
            port.RustBackend.NATIVE = ARMED
            inner = model._search(domain)
            port.RustBackend.NATIVE = DISARMED
            outer = model.with_context(active_test=False)
            composed = sorted(outer._search([("id", "in", inner)]).get_result_ids())
            port.RustBackend.NATIVE = DISARMED
            reference = sorted(model._search(domain).get_result_ids())
    except Exception as exc:
        failures.append(
            "%s %s: composing the native query raised %s: %s"
            % (case["id"], case["model"], type(exc).__name__, exc)
        )
        continue
    if composed != reference:
        failures.append(
            "%s %s: as a sub-select the native query matched %d rows, python %d"
            % (case["id"], case["model"], len(composed), len(reference))
        )
        continue
    tally["matched"] += 1

Users = env["res.users"].sudo()  # noqa: F821
user = Users.search(
    [("share", "=", False), ("id", "not in", [1, 2]), ("active", "=", True)],
    order="id",
    limit=1,
)
if not user:
    failures.append(
        "no internal user besides the administrator to run the rule scenario as"
    )
else:
    partner_model = env["ir.model"].sudo()._get("res.partner")  # noqa: F821
    env["ir.rule"].sudo().create(  # noqa: F821
        {
            "name": "search_path: hides every partner, written in-transaction",
            "model_id": partner_model.id,
            "domain_force": "[('id', '=', 0)]",
            "perm_read": True,
        }
    )
    partners = env(user=user.id)["res.partner"]  # noqa: F821
    port.RustBackend.NATIVE = ARMED
    port.reset_stats()
    armed_ids = partners._search([]).get_result_ids()
    reasons = port.stats()["reasons_by_method"].get("search", {})
    port.RustBackend.NATIVE = DISARMED
    python_ids = partners._search([]).get_result_ids()
    port.RustBackend.NATIVE = ORIGINAL
    print(
        "SEARCH after an in-transaction ir.rule: native %d ids, python %d ids, reasons %r"
        % (len(armed_ids), len(python_ids), reasons)
    )
    if list(armed_ids) != list(python_ids):
        failures.append(
            "after an ir.rule written in this transaction the port answered %d ids "
            "where python answers %d" % (len(armed_ids), len(python_ids))
        )
    if not reasons.get("this transaction wrote a security model"):
        failures.append(
            "the port did not delegate after an in-transaction ir.rule: %r" % (reasons,)
        )
    env.cr.rollback()  # noqa: F821

port.RustBackend.NATIVE = ORIGINAL
print("SEARCH cases: %s" % dict(tally))
print("SEARCH delegation reasons (top 12):")
for reason, n in delegations.most_common(12):
    print("  %6d  %s" % (n, reason[:150]))
if tally["native"] < MIN_NATIVE:
    failures.append(
        "only %d cases reached the kernel's WHERE (floor %d): the comparison "
        "above is mostly python against python" % (tally["native"], MIN_NATIVE)
    )
kinds = collections.Counter(
    "flush"
    if "does not flush" in f
    else "sub-select"
    if "as a sub-select" in f
    else "count"
    if "count_matching" in f
    else "raised on one leg"
    if ": native " in f and "answered" in f
    else "ids"
    for f in failures
)
print("SEARCH failure kinds: %s" % dict(kinds))
dump = os.environ.get("RUSTORM_SEARCH_FAILURES")
if dump:
    with pathlib.Path(dump).open("w", encoding="utf-8") as fh:
        json.dump(failures, fh, indent=1)
print("SEARCH %s" % ("OK" if not failures else "FAILED (%d)" % len(failures)))
for line in failures[:40]:
    print("  SEARCH MISMATCH %s" % line)
if len(failures) > 40:
    print("  ... %d more" % (len(failures) - 40))
sys.exit(1 if failures else 0)
