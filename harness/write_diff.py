import base64
import datetime
import decimal
import json
import os
import pathlib
import sys

OUT = os.environ["RUSTORM_WRITE_DIFF_OUT"]
ONLY = [m for m in os.environ.get("RUSTORM_WRITE_DIFF_MODELS", "").split(",") if m]
LIMIT = int(os.environ.get("RUSTORM_WRITE_DIFF_LIMIT", "40"))
FAULT = os.environ.get("RUSTORM_WRITE_DIFF_FAULT") == "1"
ARMED = os.environ.get("RUSTORM_WRITE_DIFF_LEG") == "armed"
ROWS = 6

SCALAR = (
    "char",
    "text",
    "integer",
    "float",
    "monetary",
    "boolean",
    "selection",
    "date",
    "datetime",
    "html",
)


def case(field, i):
    t = field.type
    if t in ("char", "text", "html"):
        shapes = [
            "plain %s %d" % (field.name, i),
            "",
            False,
            "\u00fc\u65e5\u672c\u8a9e \U0001f30d %s\nnl\ttab" % field.name,
            ("%s-" % field.name) * 40,
            'it\'s "q" \\ back %s' % field.name,
        ]
        value = shapes[i % 6]
        if field.size and isinstance(value, str):
            value = value[: field.size]
        return value
    if t == "integer":
        return [i, 0, False, -7, 2147483647, 1][i % 6]
    if t in ("float", "monetary"):
        return [1.5, 0.0, False, -89.999999, 0.1 + 0.2, 1e6][i % 6]
    if t == "boolean":
        return [True, False, False, True, True, False][i % 6]
    if t == "selection":
        opts = [
            k for k, _ in (field.selection if isinstance(field.selection, list) else [])
        ]
        return opts[i % len(opts)] if opts else None
    if t == "date":
        return [
            datetime.date(2026, 1, 31),
            datetime.date(1970, 1, 1),
            False,
            datetime.date(2000, 2, 29),
            datetime.date(2099, 12, 31),
            datetime.date(2026, 9, 18),
        ][i % 6]
    if t == "datetime":
        return [
            datetime.datetime(2026, 1, 31, 12, 34, 56),
            datetime.datetime(1970, 1, 1),
            False,
            datetime.datetime(2000, 2, 29, 23, 59, 59),
            datetime.datetime(2026, 9, 18, 0, 0, 0),
            datetime.datetime(2026, 6, 30, 6, 6, 6),
        ][i % 6]
    return None


def writable(model):
    out = []
    for f in model._fields.values():
        if (
            not f.store
            or f.related
            or f.name in ("id", "create_date", "write_date", "create_uid", "write_uid")
        ):
            continue
        if f.compute and not f.inverse:
            continue
        if f.readonly and not f.inverse:
            continue
        if f.type not in SCALAR:
            continue
        if getattr(f, "company_dependent", False) or getattr(f, "translate", False):
            continue
        if f.type == "selection" and (callable(f.selection) or not f.selection):
            continue
        out.append(f)
    return out


def _constrained():
    import psycopg

    from odoo.exceptions import UserError, ValidationError

    return (
        ValidationError,
        UserError,
        ValueError,
        KeyError,
        psycopg.errors.CheckViolation,
        psycopg.errors.NotNullViolation,
        psycopg.errors.NumericValueOutOfRange,
        psycopg.errors.UniqueViolation,
    )


_CONSTRAINED = _constrained()


def write_or_drop(env, recs, vals, dropped):
    if not vals:
        return
    try:
        with env.cr.savepoint():
            recs.write(vals)
            env.flush_all()
        return
    except _CONSTRAINED:
        pass
    for name, value in vals.items():
        try:
            with env.cr.savepoint():
                recs.write({name: value})
                env.flush_all()
        except _CONSTRAINED:
            if name not in dropped:
                dropped.append(name)


def create_rows(model, fields, vals_list):
    env = model.env
    try:
        with env.cr.savepoint():
            return model.create(vals_list)
    except Exception as e:
        first = e
    if type(first).__name__ == "UniqueViolation":
        covered = {f.name for f in fields}
        defaulted = [
            f
            for f in model._fields.values()
            if f.store
            and f.type in ("char", "text")
            and not f.compute
            and not getattr(f, "translate", False)
            and f.name not in covered
        ]
        retry = [
            dict(v, **{f.name: "%s uniq %d" % (f.name, i) for f in defaulted})
            for i, v in enumerate(vals_list)
        ]
        with env.cr.savepoint():
            return model.create(retry)
    if isinstance(first, ValueError) and "singleton" in str(first):
        with env.cr.savepoint():
            recs = model.browse()
            for v in vals_list:
                recs |= model.create([v])
            return recs
    raise first


def required_fill(model, fields, i):
    covered = {f.name for f in fields}
    vals = {}
    for f in model._fields.values():
        if (
            not f.required
            or not f.store
            or f.name in covered
            or f.name == "id"
            or f.default is not None
        ):
            continue
        if f.type == "selection":
            choices = f.selection if not callable(f.selection) else None
            if not choices:
                return None
            vals[f.name] = choices[0][0]
        elif f.type in SCALAR:
            vals[f.name] = (
                True
                if f.type == "boolean"
                else "%s req %d" % (f.name, i)
                if f.type in ("char", "text", "html")
                else case(f, 0)
            )
        elif f.type == "many2one":
            targets = (
                model.env[f.comodel_name]
                .sudo()
                .with_context(active_test=False)
                .search([], limit=ROWS, order="id")
            )
            if not targets:
                return None
            vals[f.name] = targets.ids[i % len(targets)]
        else:
            return None
    return vals


def dump(v):
    if isinstance(v, (datetime.date, datetime.datetime)):
        return v.isoformat()
    if isinstance(v, decimal.Decimal):
        return str(v)
    if isinstance(v, (bytes, memoryview)):
        return "b64:" + base64.b64encode(bytes(v)).decode()
    return v


def readback(env, model, ids, fields):
    cols = ["id", *(f.name for f in fields)]
    env.cr.execute(
        "SELECT %s FROM %s WHERE id = ANY(%%s) ORDER BY id"
        % (", ".join('"%s"' % c for c in cols), model._table),
        (ids,),
    )
    rows = {r[0]: [dump(v) for v in r[1:]] for r in env.cr.fetchall()}
    return cols[1:], [f.type for f in fields], [rows.get(i) for i in ids]


def scenario_balanced_entry(env):
    Move = env["account.move"].sudo()
    journal = env["account.journal"].sudo().search([("type", "=", "general")], limit=1)
    accounts = env["account.account"].sudo().search([], limit=2)
    if not journal or len(accounts) < 2:
        return None
    lines = [
        (0, 0, {"account_id": accounts[0].id, "name": "wd debit", "debit": 100.25}),
        (0, 0, {"account_id": accounts[1].id, "name": "wd credit", "credit": 100.25}),
    ]
    move = Move.create({"journal_id": journal.id, "line_ids": lines})
    env.flush_all()
    debit, credit = move.line_ids.sorted("id")
    move.write(
        {
            "line_ids": [
                (1, debit.id, {"debit": 250.5, "name": "wd debit w"}),
                (1, credit.id, {"credit": 250.5, "name": "wd credit w"}),
            ]
        }
    )
    env.flush_all()
    move.write({"ref": "wd ref \u00fc\u65e5\u672c"})
    env.flush_all()
    Line = env["account.move.line"]
    fields = [
        Line._fields[n]
        for n in (
            "name",
            "debit",
            "credit",
            "balance",
            "amount_currency",
            "display_type",
        )
    ]
    return Line, list(move.line_ids.sorted("id").ids), fields


def scenario_bulk_copy(env, model_name):
    from odoo.orm.runtime.backend import COPY_THRESHOLD

    if model_name not in env.registry:
        raise ValueError("model not installed")
    model = env[model_name].sudo()
    fields = [f for f in writable(model) if not f.required]
    if not fields:
        raise ValueError("no writable scalar field")
    n = COPY_THRESHOLD + 2
    bases = [required_fill(model, fields, i) for i in range(n)]
    if any(b is None for b in bases):
        raise ValueError("a required field the generator cannot fill")
    vals_list = [
        dict(bases[i], **{f.name: case(f, i) for f in fields}) for i in range(n)
    ]
    for i, v in enumerate(vals_list):
        for f in model._fields.values():
            if (
                f.name in v
                and v[f.name] in (False, None, "")
                and (f.required or f.name == "name")
            ):
                v[f.name] = (
                    "%s req %d" % (f.name, i)
                    if f.type in ("char", "text", "html")
                    else case(f, 0)
                )
    recs = create_rows(model, fields, vals_list)
    env.flush_all()
    return model, list(recs.ids), fields


def scenario_customer_invoice(env):
    """A posted customer invoice: two product lines, a price rewritten in
    draft, then posting -- so the receivable and tax lines the fork computes
    are written through the port, and every line's amounts plus the move's
    totals are read back raw. `name` is left out: the sequence is not rolled
    back with the transaction, so the two legs draw different numbers."""
    Move = env["account.move"].sudo()
    partner = env["res.partner"].sudo().search([("is_company", "=", True)], limit=1)
    products = env["product.product"].sudo().search([("sale_ok", "=", True)], limit=2)
    if not partner or len(products) < 2:
        return None
    invoice = Move.create(
        {
            "move_type": "out_invoice",
            "partner_id": partner.id,
            "invoice_line_ids": [
                (
                    0,
                    0,
                    {"product_id": products[0].id, "quantity": 3, "price_unit": 100.0},
                ),
                (
                    0,
                    0,
                    {"product_id": products[1].id, "quantity": 1, "price_unit": 49.99},
                ),
            ],
        }
    )
    env.flush_all()
    first = invoice.invoice_line_ids.sorted("id")[0]
    invoice.write(
        {"invoice_line_ids": [(1, first.id, {"price_unit": 125.5, "quantity": 2})]}
    )
    env.flush_all()
    invoice.action_post()
    env.flush_all()
    Line = env["account.move.line"]
    fields = [
        Line._fields[n]
        for n in (
            "debit",
            "credit",
            "balance",
            "amount_currency",
            "amount_residual",
            "quantity",
            "price_unit",
            "price_subtotal",
            "price_total",
            "display_type",
            "reconciled",
        )
    ]
    return Line, list(invoice.line_ids.sorted("id").ids), fields


def scenario_invoice_totals(env):
    """The same invoice, read at the move: totals and state after posting."""
    built = scenario_customer_invoice(env)
    if built is None:
        return None
    Line, ids, _ = built
    move = Line.browse(ids[0]).move_id
    Move = env["account.move"]
    fields = [
        Move._fields[n]
        for n in (
            "amount_untaxed",
            "amount_tax",
            "amount_total",
            "amount_residual",
            "amount_untaxed_signed",
            "amount_total_signed",
            "state",
            "move_type",
            "payment_state",
        )
    ]
    return Move, [move.id], fields


def run_scenarios(env, result, stats):
    scenarios = [
        ("balanced entry", "entry", lambda: scenario_balanced_entry(env)),
        ("customer invoice lines", "entry", lambda: scenario_customer_invoice(env)),
        ("customer invoice totals", "entry", lambda: scenario_invoice_totals(env)),
    ]
    scenarios += [
        ("bulk copy %s" % m, "copy", (lambda m=m: scenario_bulk_copy(env, m)))
        for m in ("res.partner", "crm.lead", "product.template")
    ]
    for label, kind, build in scenarios:
        before = dict(stats["native"]) if stats else {}
        try:
            with env.cr.savepoint():
                built = build()
                if built is None:
                    result["skipped"]["scenario: " + label] = (
                        "not applicable on this database"
                    )
                    raise _Rollback
                model, ids, fields = built
                columns, types, rows = readback(env, model, ids, fields)
                result["models"]["scenario: " + label] = {
                    "columns": columns,
                    "types": types,
                    "rows": rows,
                    "unlinked": [],
                    "dropped": [],
                    "scenario": kind,
                    "native": {
                        k: v - before.get(k, 0)
                        for k, v in (stats["native"].items() if stats else [])
                        if v - before.get(k, 0)
                    },
                }
                raise _Rollback
        except _Rollback:
            pass
        except Exception as e:
            result["skipped"]["scenario: " + label] = "%s: %s" % (
                type(e).__name__,
                str(e)[:120],
            )


def candidates(env):
    if ONLY:
        return [n for n in ONLY if not n.startswith("scenario")]
    return sorted(
        n
        for n, m in ((n, env[n]) for n in env.registry)
        if not m._abstract
        and not m._transient
        and m._auto
        and getattr(m, "_table", None)
    )


def arm_fault():
    import rust_backend

    original = rust_backend.RustBackend.create_rows

    def faulted(self, model, stored_list, columns, col_fields):
        for f in col_fields:
            if f.type == "char":
                for vals in stored_list:
                    if isinstance(vals.get(f.name), str):
                        vals[f.name] = repr(vals[f.name])
                break
        return original(self, model, stored_list, columns, col_fields)

    rust_backend.RustBackend.create_rows = faulted
    original_update = rust_backend.RustBackend.update_rows

    def faulted_update(self, model, fnames, rows):
        fnames = list(fnames)
        char_cols = [
            i for i, name in enumerate(fnames) if model._fields[name].type == "char"
        ]
        if char_cols:
            i = char_cols[0] + 1
            rows = [
                tuple(
                    repr(v) if j == i and isinstance(v, str) else v
                    for j, v in enumerate(row)
                )
                for row in rows
            ]
        return original_update(self, model, fnames, rows)

    rust_backend.RustBackend.update_rows = faulted_update


def arm_port():
    try:
        import engine_py
    except ImportError:
        return None
    engine_py.install_shims()
    port = engine_py.install_backend()
    import rust_orm_shim

    if port.installed() is None:
        port.install()
    rust_orm_shim.set_mode("on")
    return port


def main(env):
    port = arm_port() if ARMED else None
    if ARMED and (port is None or port.installed() is None):
        print("WRITE DIFF SKIP: the port could not be installed in this process")
        sys.exit(3)
    if FAULT:
        arm_fault()
    stats = port.STATS if port is not None else None
    if stats is not None:
        stats["native"].clear()
    result = {"models": {}, "skipped": {}, "fault": FAULT, "armed": ARMED}
    done = 0
    for name in candidates(env):
        if done >= LIMIT:
            break
        model = env[name].sudo()
        fields = writable(model)
        if not fields:
            result["skipped"][name] = "no writable scalar field"
            continue
        bases = [required_fill(model, fields, i) for i in range(ROWS)]
        if any(b is None for b in bases):
            result["skipped"][name] = "a required field the generator cannot fill"
            continue
        before = dict(stats["native"]) if stats else {}
        try:
            with env.cr.savepoint():
                vals_list = [
                    dict(bases[i], **{f.name: case(f, i) for f in fields})
                    for i in range(ROWS)
                ]
                for i, v in enumerate(vals_list):
                    for f in fields:
                        if f.required and v.get(f.name) in (False, None, ""):
                            v[f.name] = (
                                True
                                if f.type == "boolean"
                                else "%s req %d" % (f.name, i)
                                if f.type in ("char", "text", "html")
                                else case(f, 0)
                            )
                try:
                    recs = create_rows(model, fields, vals_list)
                except _CONSTRAINED as first:
                    minimal = [
                        dict(
                            bases[i],
                            **{
                                f.name: v[f.name]
                                for f in fields
                                if f.required and f.name in v
                            },
                        )
                        for i, v in enumerate(vals_list)
                    ]
                    try:
                        recs = create_rows(model, fields, minimal)
                    except Exception:
                        raise first from None
                env.flush_all()
                dropped = []
                for f in fields:
                    try:
                        with env.cr.savepoint():
                            recs[0].write({f.name: case(f, 3)})
                            env.flush_all()
                    except _CONSTRAINED:
                        dropped.append(f.name)
                if dropped:
                    fields = [f for f in fields if f.name not in dropped]
                if len(recs) > 1:
                    uniform = [
                        f for f in fields if f.type not in ("char", "text", "html")
                    ]
                    if uniform:
                        write_or_drop(
                            env,
                            recs[1:],
                            {f.name: case(f, 4) for f in uniform},
                            dropped,
                        )
                    for k, rec in enumerate(recs[1:]):
                        write_or_drop(
                            env,
                            rec,
                            {
                                f.name: case(f, 4)
                                if k == 0
                                else "%s row %d" % (f.name, k)
                                for f in fields
                                if f.type in ("char", "text", "html")
                            },
                            dropped,
                        )
                    fields = [f for f in fields if f.name not in dropped]
                env.flush_all()
                ids = list(recs.ids)
                recs[1::2].unlink()
                env.flush_all()
                columns, types, rows = readback(env, model, ids, fields)
                result["models"][name] = {
                    "columns": columns,
                    "types": types,
                    "rows": rows,
                    "unlinked": [k for k in range(len(ids)) if k % 2 == 1],
                    "dropped": sorted(dropped),
                    "native": {
                        k: v - before.get(k, 0)
                        for k, v in (stats["native"].items() if stats else [])
                        if v - before.get(k, 0)
                    },
                }
                raise _Rollback
        except _Rollback:
            done += 1
        except Exception as e:
            result["skipped"][name] = "%s: %s" % (type(e).__name__, str(e)[:120])
    if not ONLY or any(o.startswith("scenario") for o in ONLY):
        run_scenarios(env, result, stats)
    env.cr.rollback()
    with pathlib.Path(OUT).open("w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=1, default=str)
    print(
        "WRITE DIFF leg: %d models, %d skipped, fault=%s -> %s"
        % (len(result["models"]), len(result["skipped"]), FAULT, OUT)
    )


class _Rollback(Exception):
    pass


main(env)  # noqa: F821  injected by odoo-bin shell
