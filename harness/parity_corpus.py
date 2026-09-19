import json
import os
import pathlib

OUT = os.environ.get("RUSTORM_PARITY_OUT", "/tmp/rustorm_parity_corpus.json")
MAX_MODELS = int(os.environ.get("RUSTORM_PARITY_MODELS", "0"))


def stored(f):
    return f.store and f.name != "id"


def cases_for(model):
    fields = model._fields
    out = []

    def add(method, args, kwargs):
        out.append(
            {"model": model._name, "method": method, "args": args, "kwargs": kwargs}
        )

    scalars = sorted(
        f.name
        for f in fields.values()
        if stored(f)
        and f.type
        in (
            "char",
            "boolean",
            "integer",
            "date",
            "datetime",
            "selection",
            "float",
            "monetary",
        )
    )[:3]
    m2o = sorted(f.name for f in fields.values() if stored(f) and f.type == "many2one")[
        :1
    ]
    x2m = sorted(
        f.name
        for f in fields.values()
        if f.store
        and f.type in ("one2many", "many2many")
        and f.comodel_name in model.env.registry
    )[:1]

    order = "id asc"
    read_fields = scalars + m2o
    if read_fields:
        add(
            "search_read",
            [],
            {"domain": [], "fields": read_fields, "limit": 20, "order": order},
        )
    add("search_count", [], {"domain": []})

    spec = {name: {} for name in scalars}
    for name in m2o:
        spec[name] = {"fields": {"display_name": {}}}
    for name in x2m:
        spec[name] = {}
    if spec:
        add(
            "web_search_read",
            [],
            {
                "domain": [],
                "specification": spec,
                "limit": 20,
                "order": order,
                "count_limit": 10001,
            },
        )
        add(
            "web_search_read",
            [],
            {
                "domain": [],
                "specification": spec,
                "offset": 2,
                "limit": 5,
                "order": order,
            },
        )
    if m2o:
        add(
            "web_read_group",
            [],
            {"domain": [], "groupby": m2o, "aggregates": ["__count"]},
        )
    if model._rec_name:
        add("name_search", [], {"name": "a", "limit": 8})

    if spec:
        try:
            ids = model.search([], order="id", limit=3).ids
        except Exception:
            ids = []
        if ids:
            add("web_read", [ids], {"specification": spec})
    return out


def main(env):
    base = env(user=2, su=False)
    cases = []
    names = sorted(base.registry)
    if MAX_MODELS:
        names = names[:MAX_MODELS]
    for name in names:
        model = base.get(name)
        if model is None or model._abstract or model._transient or not model._auto:
            continue
        for case in cases_for(model):
            case["id"] = "p%05d" % len(cases)
            cases.append(case)
    with pathlib.Path(OUT).open("w", encoding="utf-8") as fh:
        json.dump(cases, fh, indent=1)
    print("PARITY CORPUS wrote %d cases to %s" % (len(cases), OUT))


main(env)  # noqa: F821  (env comes from the odoo shell namespace)
