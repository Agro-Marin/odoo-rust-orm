import json
import os
import random

OUT = os.environ.get("RUSTPOC_FUZZ_OUT", "/tmp/rustpoc_fuzz_corpus.json")
SEED = int(os.environ.get("RUSTPOC_FUZZ_SEED", "1"))
CASES = int(os.environ.get("RUSTPOC_FUZZ_CASES", "1500"))
MAX_MODELS = int(os.environ.get("RUSTPOC_FUZZ_MODELS", "120"))

TEXT_OPS = ["=", "!=", "in", "not in", "like", "not like", "ilike", "not ilike",
            "=like", "=ilike", "not =like", "not =ilike"]
NUM_OPS = ["=", "!=", "<", "<=", ">", ">=", "in", "not in"]
BOOL_OPS = ["=", "!="]
M2O_OPS = ["=", "!=", "in", "not in", "ilike", "not ilike", "any", "not any",
           "child_of", "parent_of"]
X2M_OPS = ["=", "!=", "in", "not in", "any", "not any"]


class Gen:
    def __init__(self, env, rng):
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
            if pick is not None and isinstance(pick, str) and len(pick) > 2 and edge < 0.4:
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
                return [base, 999999] if edge < 0.25 else ([base, False] if edge < 0.4 else [base])
            if operator in ("any", "not any"):
                return []
            return False if edge < 0.15 else base
        return False

    def leaf(self, model, depth=0):
        rng = self.rng
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
            ops = BOOL_OPS
        elif field.type == "many2one":
            ops = M2O_OPS
        elif field.type in ("one2many", "many2many"):
            ops = X2M_OPS
        elif field.type in ("date", "datetime"):
            ops = NUM_OPS
        else:
            ops = ["=", "!="]
        operator = rng.choice(ops)

        name = field.name
        if (
            field.type in ("many2one", "one2many", "many2many")
            and operator not in ("any", "not any", "child_of", "parent_of")
            and depth == 0
            and rng.random() < 0.35
        ):
            co = model.env.get(field.comodel_name)
            if co is not None:
                sub = [
                    f
                    for f in co._fields.values()
                    if f.store and f.type in ("char", "integer", "boolean", "date")
                ]
                if sub:
                    subfield = rng.choice(sub)
                    name = "%s.%s" % (field.name, subfield.name)
                    operator = rng.choice(
                        TEXT_OPS if subfield.type == "char" else NUM_OPS
                    )
                    return [name, operator, self.value_for(co, subfield, operator)]
        if operator in ("child_of", "parent_of"):
            return [name, operator, self.value_for(model, field, "=")]
        return [name, operator, self.value_for(model, field, operator)]

    def domain(self, model):
        rng = self.rng
        n = rng.choice([1, 1, 2, 2, 3])
        leaves = [self.leaf(model) for _ in range(n)]
        leaves = [x for x in leaves if x]
        if not leaves:
            return []
        out = []
        if len(leaves) >= 2 and rng.random() < 0.5:
            out.append(rng.choice(["&", "|"]))
            out.extend(leaves[:2])
            leaves = leaves[2:]
        elif rng.random() < 0.15:
            out.append("!")
            out.append(leaves.pop(0))
        out.extend(leaves)
        return out


def main(env):
    rng = random.Random(SEED)
    gen = Gen(env, rng)
    base = env(user=2, su=False)
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

    probe = base["res.users"].sudo().search([("login", "=", "rustpoc_sweep_probe")], limit=1)
    identities = [None] + ([probe.id] if probe else [])

    cases = []
    while len(cases) < CASES and models:
        name = rng.choice(models)
        model = base.get(name)
        if model is None:
            continue
        dom = gen.domain(model)
        uid = rng.choice(identities)
        method = rng.choice(["search_read", "search_read", "search_count", "read_group"])
        case = {"id": "f%05d" % len(cases), "model": name, "domain": dom}
        if method == "search_read":
            names = sorted(
                f.name
                for f in model._fields.values()
                if f.store and f.type not in ("binary", "properties", "properties_definition")
            )
            if not names:
                continue
            case["method"] = "search_read"
            case["fields"] = rng.sample(names, min(len(names), rng.randrange(1, 4)))
            case["limit"] = rng.choice([1, 5, 20])
            if rng.random() < 0.2:
                case["order"] = rng.choice(names) + rng.choice(["", " desc"])
        elif method == "search_count":
            case["method"] = "search_count"
            if rng.random() < 0.3:
                case["limit"] = rng.choice([1, 10])
        else:
            gb = [
                f.name
                for f in model._fields.values()
                if f.store and f.type in ("many2one", "selection", "boolean", "date", "char")
            ]
            if not gb:
                continue
            case["method"] = "read_group"
            case["groupby"] = [rng.choice(sorted(gb))]
            case["aggregates"] = ["__count"]
        if uid is not None:
            case["uid"] = uid
        if rng.random() < 0.15:
            case["su"] = True
        if rng.random() < 0.15:
            case["active_test"] = False
        cases.append(case)

    with open(OUT, "w") as f:
        json.dump(cases, f, indent=1)
    print(
        "wrote %d fuzz cases to %s (seed=%d, %d models, identities=%s)"
        % (len(cases), OUT, SEED, len(models), identities)
    )


main(env)  # noqa: F821  (env comes from the odoo shell namespace)
