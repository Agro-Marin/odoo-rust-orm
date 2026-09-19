"""One leg of the write differential: create, write, unlink, read back raw.

Run under `odoo-bin shell` with `env` injected (see verify.sh's shell_script
and python_script), once with the engine armed and once without. Each leg
writes the same generated rows to the same models in one transaction, reads
every stored scalar column of the rows it created back with raw SQL -- the
cache cannot mask what the row holds -- dumps them as JSON and rolls back.
`write_diff_compare.py` pairs the two dumps by creation order.

    RUSTORM_WRITE_DIFF_OUT     where the JSON goes (required)
    RUSTORM_WRITE_DIFF_LEG     "armed": install the port and route in this
                               process (exit 3 when it cannot); anything
                               else is the control, which arms nothing
    RUSTORM_WRITE_DIFF_MODELS  comma-separated model names; default: every
                               concrete model the generator can create
    RUSTORM_WRITE_DIFF_LIMIT   at most this many models (default 40)
    RUSTORM_WRITE_DIFF_FAULT   "1": the positive control -- the armed leg's
                               port writes the first char column's repr
"""

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
    """The i-th value for a field: plain, empty, NULL, unicode with control
    characters, an extreme, a float that does not round-trip through text.
    Strings carry the field's name and the row, so a unique column gets a
    distinct value per row and two char columns never collide."""
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
            # stored as jsonb keyed by company or language: another comparison,
            # and a unique rule over one key (res.partner.barcode) collides on
            # the empty string the scalar cases carry
            continue
        if f.type == "selection" and (callable(f.selection) or not f.selection):
            continue
        out.append(f)
    return out


def create_rows(model, fields, vals_list):
    """The six creates, with two retries for what the first attempt cannot
    know: a unique rule over a defaulted char the comparison excludes (the
    six rows share the default), answered by a distinct value per row; and
    a create override written for one record, answered one row at a time."""
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
    """Row i's values for the required fields the comparison does not cover:
    an existing row for a many2one, a first choice for a selection, a per-row
    string for a scalar `writable` excluded (computed, readonly, translated,
    company-dependent). None when a required field has no fillable value,
    which skips the model."""
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
            target = (
                model.env[f.comodel_name]
                .sudo()
                .with_context(active_test=False)
                .search([], limit=1)
            )
            if not target:
                return None
            vals[f.name] = target.id
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


def candidates(env):
    return ONLY or sorted(
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

    # The per-row string writes are the LAST write of every char column, so
    # a fault on the create alone is overwritten before the dump reads it and
    # the control passes on nothing. The fault has to ride the final write.
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
    """The armed leg: the port on the class and routing on for this process,
    as write_path.py does -- the addon arms routing, not the port, in a shell."""
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
        sys.exit(3)  # skipped, not passed
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
                            # per row: a required char is often the unique one
                            v[f.name] = (
                                True
                                if f.type == "boolean"
                                else "%s req %d" % (f.name, i)
                                if f.type in ("char", "text", "html")
                                else case(f, 0)
                            )
                recs = create_rows(model, fields, vals_list)
                env.flush_all()
                # second values: one field at a time on the first record, all at once on the rest
                for f in fields:
                    recs[0].write({f.name: case(f, 3)})
                if len(recs) > 1:
                    # one value onto many rows (`update_rows.uniform`) for the
                    # types a unique constraint cannot catch; strings get a
                    # value per row, since a shared one collides on a unique column
                    uniform = [
                        f for f in fields if f.type not in ("char", "text", "html")
                    ]
                    if uniform:
                        recs[1:].write({f.name: case(f, 4) for f in uniform})
                    for k, rec in enumerate(recs[1:]):
                        rec.write(
                            {
                                f.name: case(f, 4)
                                if k == 0
                                else "%s row %d" % (f.name, k)
                                for f in fields
                                if f.type in ("char", "text", "html")
                            }
                        )
                env.flush_all()
                ids = list(recs.ids)
                recs[1::2].unlink()
                env.flush_all()
                cols = ["id", *(f.name for f in fields)]
                env.cr.execute(
                    "SELECT %s FROM %s WHERE id = ANY(%%s) ORDER BY id"
                    % (", ".join('"%s"' % c for c in cols), model._table),
                    (ids,),
                )
                rows = {r[0]: [dump(v) for v in r[1:]] for r in env.cr.fetchall()}
                result["models"][name] = {
                    "columns": cols[1:],
                    "types": [f.type for f in fields],
                    "rows": [rows.get(i) for i in ids],
                    "unlinked": [k for k in range(len(ids)) if k % 2 == 1],
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
