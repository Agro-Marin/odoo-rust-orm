import json
import os
import pathlib
import random
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
import pathlib

from _env import base_env, out_path

OUT = out_path("fuzz_corpus.json", "RUSTORM_FUZZ_OUT")
SEED = int(os.environ.get("RUSTORM_FUZZ_SEED", "1"))
CASES = int(os.environ.get("RUSTORM_FUZZ_CASES", "1500"))
MAX_MODELS = int(os.environ.get("RUSTORM_FUZZ_MODELS", "120"))

TEXT_OPS = [
    "=",
    "!=",
    "in",
    "not in",
    "like",
    "not like",
    "ilike",
    "not ilike",
    "=like",
    "=ilike",
    "not =like",
    "not =ilike",
]
NUM_OPS = ["=", "!=", "<", "<=", ">", ">=", "in", "not in"]
BOOL_OPS = ["=", "!="]
M2O_OPS = [
    "=",
    "!=",
    "in",
    "not in",
    "ilike",
    "not ilike",
    "any",
    "not any",
    "child_of",
    "parent_of",
]
X2M_OPS = ["=", "!=", "in", "not in", "any", "not any"]
UNORDERABLE = (
    "one2many",
    "many2many",
    "html",
    "json",
    "text",
    "binary",
    "properties",
    "properties_definition",
)
GRANULARITIES = ["day", "week", "month", "quarter", "year", "day_of_week"]
AGGREGATES = ["sum", "avg", "min", "max"]
FAMILIES = [
    "granularity",
    "tz",
    "=?",
    "child_of",
    "nulls",
    "offset",
    "two_level_groupby",
    "count_distinct",
] + AGGREGATES


class Gen:
    def __init__(self, env, rng) -> None:
        self.env = env
        self.rng = rng
        self._samples = {}

    def samples(self, model, fname):
        key = (model._name, fname)
        if key not in self._samples:
            try:
                rows = (
                    model.sudo()
                    .with_context(active_test=False)
                    .search_read([(fname, "!=", False)], [fname], limit=5)
                )
                vals = []
                for r in rows:
                    v = r[fname]
                    if isinstance(v, tuple):
                        v = v[0]
                    if isinstance(v, list):
                        v = v[0] if v else None
                    if v is None:
                        continue
                    if hasattr(v, "strftime"):
                        v = v.strftime(
                            "%Y-%m-%d %H:%M:%S" if hasattr(v, "hour") else "%Y-%m-%d"
                        )
                    vals.append(v)
            except Exception:
                vals = []
            self._samples[key] = vals
        return self._samples[key]

    def value_for(self, model, field, operator):
        rng = self.rng
        sampled = self.samples(model, field.name)
        pick = rng.choice(sampled) if sampled else None

        edge = rng.random()
        if field.type in ("char", "text", "html", "selection"):
            if (
                pick is not None
                and isinstance(pick, str)
                and len(pick) > 2
                and edge < 0.4
            ):
                start = rng.randrange(0, len(pick) - 1)
                pick = pick[start : start + rng.randrange(1, 4)]
            base = pick if pick is not None else "zz"
            if operator in ("in", "not in"):
                return [base, False] if edge < 0.3 else [base]
            return False if edge < 0.12 else base
        if field.type in ("integer", "float", "monetary"):
            base = pick if pick is not None else 0
            if operator in ("in", "not in"):
                return [base, 0] if edge < 0.3 else [base]
            return False if edge < 0.1 else base
        if field.type == "boolean":
            return rng.choice([True, False])
        if field.type in ("date", "datetime"):
            base = pick if pick is not None else "2020-01-01"
            if isinstance(base, str) and field.type == "datetime" and len(base) == 10:
                base += " 00:00:00"
            if operator in ("in", "not in"):
                return [base]
            return False if edge < 0.1 else base
        if field.type in ("many2one", "one2many", "many2many", "many2one_reference"):
            base = pick if pick is not None else 1
            if operator in ("ilike", "not ilike"):
                return "a"
            if operator in ("in", "not in"):
                if edge < 0.12:
                    return [base, "a"] if edge < 0.06 else ["a"]
                return (
                    [base, 999999]
                    if edge < 0.25
                    else ([base, False] if edge < 0.4 else [base])
                )
            if operator in ("any", "not any"):
                return []
            return False if edge < 0.15 else base
        return False

    def scalar_leaf(self, model, field, operator, co=None):
        rng = self.rng
        target = co if co is not None else model
        value = self.value_for(target, field, operator)
        r = rng.random()
        if operator in ("in", "not in") and isinstance(value, list) and r < 0.15:
            value = rng.choice(value + [False, 0, ""])
        elif operator in ("=", "!=") and r < 0.1 and not isinstance(value, list):
            value = [value, False] if rng.random() < 0.5 else [value]
        elif (
            operator == "="
            and r < 0.15
            and field.type in ("char", "integer", "float", "many2one", "boolean")
        ):
            operator = "=?"
        return [field.name, operator, value]

    def dotted_leaf(self, model, field, depth):
        rng = self.rng
        co = model.env.get(field.comodel_name)
        if co is None:
            return None
        sub = [
            f
            for f in co._fields.values()
            if f.store and f.type in ("char", "integer", "boolean", "date", "many2one")
        ]
        if not sub:
            return None
        subfield = rng.choice(sub)
        name = f"{field.name}.{subfield.name}"
        if subfield.type == "many2one":
            deeper = self.dotted_leaf(co, subfield, depth + 1) if depth < 1 else None
            if deeper is None:
                operator = rng.choice(M2O_OPS[:6])
                return [name, operator, self.value_for(co, subfield, operator)]
            return [f"{field.name}.{deeper[0]}", deeper[1], deeper[2]]
        operator = rng.choice(
            TEXT_OPS
            if subfield.type == "char"
            else BOOL_OPS + ["in", "not in"]
            if subfield.type == "boolean"
            else NUM_OPS
        )
        leaf = self.scalar_leaf(co, subfield, operator)
        return [name, leaf[1], leaf[2]]

    def special_leaf(self, model):
        rng = self.rng
        r = rng.random()
        if r < 0.5:
            ids = self.samples(model, "id") or [1]
            operator = rng.choice(["=", "!=", "in", "not in", "<", ">", ">=", "<="])
            if model._parent_name in model._fields and rng.random() < 0.3:
                operator = rng.choice(["child_of", "parent_of"])
                return ["id", operator, rng.choice(ids)]
            value = rng.choice(ids)
            if operator in ("in", "not in"):
                value = [value, 999999] if rng.random() < 0.5 else [value]
            return ["id", operator, value]
        operator = rng.choice(TEXT_OPS)
        value = rng.choice(["a", "", "zz", False])
        if operator in ("in", "not in"):
            value = [value, False] if rng.random() < 0.5 else [value]
        return ["display_name", operator, value]

    def leaf(self, model, depth=0):
        rng = self.rng
        if rng.random() < 0.06:
            return self.special_leaf(model)
        candidates = [
            f
            for f in model._fields.values()
            if f.store and f.name != "id" and f.type != "binary"
        ]
        if not candidates:
            return None
        field = rng.choice(candidates)
        if field.type in ("char", "text", "html", "selection"):
            ops = TEXT_OPS
        elif field.type in ("integer", "float", "monetary"):
            ops = NUM_OPS
        elif field.type == "boolean":
            ops = BOOL_OPS + ["in", "not in"]
        elif field.type == "many2one":
            ops = M2O_OPS
        elif field.type in ("one2many", "many2many"):
            ops = X2M_OPS
        elif field.type in ("date", "datetime"):
            ops = NUM_OPS
        else:
            ops = ["=", "!="]
        operator = rng.choice(ops)

        if field.type in ("many2one", "one2many", "many2many"):
            if operator in ("any", "not any"):
                co = model.env.get(field.comodel_name)
                if co is not None and depth < 2 and rng.random() < 0.7:
                    return [field.name, operator, self.domain(co, depth + 1)]
                return [field.name, operator, []]
            if operator in ("child_of", "parent_of"):
                return [field.name, operator, self.value_for(model, field, "=")]
            if depth < 2 and rng.random() < 0.4:
                dotted = self.dotted_leaf(model, field, depth)
                if dotted is not None:
                    return dotted
            return [field.name, operator, self.value_for(model, field, operator)]
        return self.scalar_leaf(model, field, operator)

    def tree(self, model, depth, budget):
        rng = self.rng
        r = rng.random()
        if budget[0] <= 1 or depth >= 3 or r < 0.45:
            budget[0] -= 1
            leaf = self.leaf(model, depth)
            return [leaf] if leaf else []
        if r < 0.6:
            inner = self.tree(model, depth + 1, budget)
            return ["!"] + inner if inner else []
        a = self.tree(model, depth + 1, budget)
        b = self.tree(model, depth + 1, budget)
        if not a or not b:
            return a or b
        return [rng.choice(["&", "|"])] + a + b

    def domain(self, model, depth=0):
        budget = [self.rng.choice([1, 1, 2, 2, 3, 4])]
        return self.tree(model, depth, budget)


def domain_families(domain, found) -> None:
    for item in domain or []:
        if not isinstance(item, list):
            continue
        if len(item) == 3 and isinstance(item[0], str):
            if item[1] == "=?":
                found.add("=?")
            if item[1] in ("child_of", "parent_of"):
                found.add("child_of")
            if item[1] in ("any", "not any"):
                domain_families(item[2], found)
        else:
            domain_families(item, found)


def families(case):
    found = set()
    domain_families(case.get("domain"), found)
    if "tz" in case:
        found.add("tz")
    if case.get("offset"):
        found.add("offset")
    order = case.get("order") or ""
    if "nulls first" in order or "nulls last" in order:
        found.add("nulls")
    gb = case.get("groupby") or []
    if any(":" in g for g in gb):
        found.add("granularity")
    if len(gb) > 1:
        found.add("two_level_groupby")
    for agg in case.get("aggregates") or []:
        if ":" in agg:
            found.add(agg.split(":")[1])
    return found


def main(env) -> None:
    rng = random.Random(SEED)
    gen = Gen(env, rng)
    base = base_env(env)
    models = [
        n
        for n in sorted(base.registry)
        if (m := base.get(n)) is not None
        and not m._abstract
        and not m._transient
        and m._auto
    ]
    rng.shuffle(models)
    models = models[:MAX_MODELS]

    langs = base["res.lang"].sudo().search([("active", "=", True)]).mapped("code") or [
        "en_US"
    ]
    companies = base["res.users"].sudo().browse(2).company_ids.ids
    company_choices = [[c] for c in companies] + (
        [companies] if len(companies) > 1 else []
    )
    probe = (
        base["res.users"]
        .sudo()
        .search([("login", "=", "rustorm_sweep_probe")], limit=1)
    )
    identities = [None] + ([probe.id] if probe else [])

    def build_case(name, want=None):
        model = base.get(name)
        if model is None:
            return None
        fields = model._fields
        if want == "child_of":
            if model._parent_name not in fields:
                return None
            ids = gen.samples(model, "id") or [1]
            dom = [["id", rng.choice(["child_of", "parent_of"]), rng.choice(ids)]]
        elif want == "=?":
            scalars = [
                f
                for f in fields.values()
                if f.store
                and f.type in ("char", "integer", "float", "many2one", "boolean")
            ]
            if not scalars:
                return None
            field = rng.choice(scalars)
            dom = [[field.name, "=?", gen.value_for(model, field, "=")]]
        else:
            dom = gen.domain(model)
        uid = rng.choice(identities)
        method = rng.choice(
            ["search_read", "search_read", "search_count", "read_group"]
        )
        if want in ("nulls", "offset"):
            method = "search_read"
        elif want in (
            "granularity",
            "tz",
            "two_level_groupby",
            "count_distinct",
        ) + tuple(AGGREGATES):
            method = "read_group"
        case = {"model": name, "domain": dom}
        if method == "search_read":
            names = sorted(
                f.name
                for f in fields.values()
                if f.store
                and f.type not in ("binary", "properties", "properties_definition")
            )
            orderable = sorted(
                f.name for f in fields.values() if f.store and f.type not in UNORDERABLE
            )
            if not names:
                return None
            case["method"] = "search_read"
            case["fields"] = rng.sample(names, min(len(names), rng.randrange(1, 4)))
            case["limit"] = rng.choice([1, 5, 20])
            if want == "offset" or rng.random() < 0.15:
                case["offset"] = rng.choice([1, 3, 50])
            if orderable and (want == "nulls" or rng.random() < 0.2):
                directions = ["", " desc", " asc nulls first", " desc nulls last"]
                if want == "nulls":
                    directions = directions[2:]
                case["order"] = rng.choice(orderable) + rng.choice(directions) + ", id"
            if "order" not in case:
                terms = [
                    t.strip().split()[0] for t in model._order.split(",") if t.strip()
                ]
                case["order"] = model._order if "id" in terms else model._order + ", id"
        elif method == "search_count":
            case["method"] = "search_count"
            if rng.random() < 0.3:
                case["limit"] = rng.choice([1, 10])
        else:
            gb = [
                f.name
                for f in fields.values()
                if f.store
                and f.type
                in ("many2one", "selection", "boolean", "date", "datetime", "char")
            ]
            dates = sorted(
                f.name
                for f in fields.values()
                if f.store and f.type in ("date", "datetime")
            )
            if not gb:
                return None
            case["method"] = "read_group"
            groupby = rng.choice(sorted(gb))
            if want in ("granularity", "tz"):
                if not dates:
                    return None
                groupby = rng.choice(dates)
            if fields[groupby].type in ("date", "datetime") and (
                want in ("granularity", "tz") or rng.random() < 0.6
            ):
                groupby += ":" + rng.choice(GRANULARITIES)
                case["lang"] = rng.choice(langs)
                if want == "tz" or rng.random() < 0.5:
                    case["tz"] = "America/Mexico_City"
            groupbys = [groupby]
            others = sorted(set(gb) - {groupby.split(":")[0]})
            if want == "two_level_groupby" and not others:
                return None
            if others and (want == "two_level_groupby" or rng.random() < 0.3):
                groupbys.append(rng.choice(others))
            case["groupby"] = groupbys
            aggregates = ["__count"]
            nums = sorted(
                f.name
                for f in fields.values()
                if f.store and f.type in ("integer", "float", "monetary")
            )
            if want in AGGREGATES and not nums:
                return None
            if nums and (want in AGGREGATES or rng.random() < 0.4):
                agg = want if want in AGGREGATES else rng.choice(AGGREGATES)
                aggregates.append(f"{rng.choice(nums)}:{agg}")
            m2os = sorted(
                f.name for f in fields.values() if f.store and f.type == "many2one"
            )
            if want == "count_distinct" and not m2os:
                return None
            if m2os and (want == "count_distinct" or rng.random() < 0.2):
                aggregates.append(f"{rng.choice(m2os)}:count_distinct")
            case["aggregates"] = aggregates
            if rng.random() < 0.3:
                term = rng.choice(aggregates + [g.split(":")[0] for g in groupbys])
                tail = ", ".join(g for g in groupbys if g != term)
                case["order"] = (
                    term + rng.choice(["", " desc"]) + (", " + tail if tail else "")
                )
        if uid is not None:
            case["uid"] = uid
        if "lang" not in case and rng.random() < 0.3:
            case["lang"] = rng.choice(langs)
        if len(company_choices) > 1 and rng.random() < 0.25:
            case["allowed_company_ids"] = rng.choice(company_choices)
        if rng.random() < 0.15:
            case["su"] = True
        if rng.random() < 0.15:
            case["active_test"] = False
        return case

    cases = []
    while len(cases) < CASES and models:
        case = build_case(rng.choice(models))
        if case is not None:
            cases.append(case)

    emitted = set()
    for case in cases:
        emitted |= families(case)
    parents = [
        n
        for n in models
        if (m := base.get(n)) is not None and m._parent_name in m._fields
    ]
    for family in FAMILIES:
        if family in emitted:
            continue
        pool = parents if family == "child_of" else models
        for _ in range(200):
            if not pool:
                break
            case = build_case(rng.choice(pool), want=family)
            if case is not None and family in families(case):
                cases.append(case)
                emitted.add(family)
                break
    for i, case in enumerate(cases):
        case["id"] = "f%05d" % i
    cases = [
        dict(id=c["id"], **{k: v for k, v in c.items() if k != "id"}) for c in cases
    ]

    with pathlib.Path(OUT).open("w", encoding="utf-8") as f:
        json.dump(cases, f, indent=1)
    print(
        "wrote %d fuzz cases to %s (seed=%d, %d models, identities=%s)"
        % (len(cases), OUT, SEED, len(models), identities)
    )
    missing = [f for f in FAMILIES if f not in emitted]
    counts = {f: sum(f in families(c) for c in cases) for f in FAMILIES}
    print("families: %s" % " ".join("%s=%d" % kv for kv in counts.items()))
    if missing:
        print("FUZZ GENERATOR FAILED: no case exercises %s" % missing)
        sys.exit(1)


main(env)  # noqa: F821
