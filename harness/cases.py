SYMBOLIC_LOGINS = {"grouped": "rustorm_sweep_grouped", "debug": "rustorm_sweep_debug"}


def resolve_uid(base, uid):
    if not isinstance(uid, str):
        return uid if uid is not None else 2
    su = base(user=1, su=True)
    if uid == "other":
        u = su["res.users"].search(
            [("id", "not in", [1, 2]), ("active", "=", True)], order="id", limit=1
        )
        if not u:
            raise ValueError('no alternate identity on this database (uid "other")')
        return u.id
    login = SYMBOLIC_LOGINS.get(uid)
    if login is None:
        raise ValueError("unknown uid symbol %r" % uid)
    u = su["res.users"].search([("login", "=", login), ("active", "=", True)], limit=1)
    if not u:
        raise ValueError(
            "no seeded identity %r on this database (uid %r)" % (login, uid)
        )
    return u.id


def case_env(base, case):
    ctx = dict(base.context)
    ctx["lang"] = case.get("lang") or "en_US"
    if case.get("allowed_company_ids"):
        ctx["allowed_company_ids"] = case["allowed_company_ids"]
    if "active_test" in case:
        ctx["active_test"] = case["active_test"]
    if case.get("tz"):
        ctx["tz"] = case["tz"]
    env = base(user=resolve_uid(base, case.get("uid")), su=bool(case.get("su")))
    return env(context=ctx)


def run_case(base, case, resolve_labels=True):
    env = case_env(base, case)
    M = env[case["model"]]
    method = case["method"]
    domain = case.get("domain") or []
    unsupported = {
        "search_read": (),
        "search_count": ("fields", "offset", "order", "groupby", "aggregates"),
        "read_group": ("fields", "limit", "offset"),
    }.get(method, ())
    for key in unsupported:
        if case.get(key):
            raise ValueError(
                "case %r passes %r, which run_case does not apply to %s"
                % (case.get("id"), key, method)
            )
    if method == "search_read":
        return M.search_read(
            domain,
            case["fields"],
            offset=case.get("offset") or 0,
            limit=case.get("limit"),
            order=case.get("order"),
        )
    if method == "search_count":
        return M.search_count(domain, limit=case.get("limit"))
    if method == "read_group":
        gb = case["groupby"]
        gb = [gb] if isinstance(gb, str) else gb
        rows = M._read_group(
            domain,
            groupby=gb,
            aggregates=case.get("aggregates") or ["__count"],
            order=case.get("order"),
        )
        out = []
        for row in rows:
            item = []
            for gv in row[: len(gb)]:
                if hasattr(gv, "_name"):
                    gv = (
                        ([gv.id, gv.display_name] if gv else False)
                        if resolve_labels
                        else (gv.id if gv else False)
                    )
                item.append(gv)
            item.extend(row[len(gb) :])
            out.append(item)
        return out
    raise ValueError("unknown method %s" % method)
