"""Run in odoo-bin shell: verify live, old and malformed field-policy exports."""

import copy
import json
import logging

from _env import dsn_for

_logger = logging.getLogger("odoo.rust_kernel.registry_policy_probe")


def live_classes(env, export):
    from odoo.libs.field_class_names import is_anonymous

    served = python = 0
    for model_name, model in export["models"].items():
        fields = env.registry[model_name]._fields
        for name, exported in model["fields"].items():
            classes = fields[name].access_classes
            if classes is None or not (classes.read or classes.read_alternative):
                assert exported["python_read_access"] is False, (model_name, name)
                continue
            groups_only = (
                not classes.read_alternative
                and len(classes.read) == 1
                and is_anonymous(classes.read[0])
            )
            assert exported["python_read_access"] is not groups_only, (
                model_name,
                name,
                classes,
            )
            if groups_only:
                assert exported["groups"] == fields[name].groups, (model_name, name)
                served += 1
            else:
                python += 1
    assert served and python, (served, python)
    print(
        f"REGISTRY POLICY live: {served} groups= fields evaluated natively, "
        f"{python} class-policy fields left to Python"
    )


def group_spec_verdicts(env, engine_py, db, conn, export):
    kernel = engine_py.RustKernel.build(db, json.dumps(export))
    user = env.ref("base.group_user").id
    manager = env.ref("base.group_erp_manager").id
    request = {
        "model": "res.partner",
        "method": "search",
        "domain": [["signup_type", "=", "ZZ"]],
        "registry_sequence": env.registry.registry_sequence,
        "uid": 2,
        "su": False,
    }
    for label, held, expected in (
        ("without the group", {user: None}, engine_py.KernelAccessDenied),
        ("with the group", {user: None, manager: None}, None),
        (
            "with the group in one company",
            {user: None, manager: [env.company.id]},
            "masks it per record",
        ),
    ):
        request["principal_groups"] = held
        error = None
        try:
            kernel.search_where(conn, json.dumps(request), offline=False)
        except engine_py.KernelRefused as exc:
            error = exc
        _logger.debug("group spec %s: error=%r", label, error)
        if expected is None:
            assert error is None, (label, error)
        elif isinstance(expected, str):
            assert error is not None and expected in str(error), (label, error)
        else:
            assert isinstance(error, expected), (label, error)
    print("REGISTRY POLICY groups= verdicts: denied, served, per record refused")


def run(env):
    import engine_py

    shim = engine_py.install_shims()[1]
    export = json.loads(engine_py.export_registry(env.registry))
    field = export["models"]["res.country"]["fields"]["code"]
    assert field["python_read_access"] is False
    live_classes(env, export)
    principal = env(user=2, su=False)
    request = {
        "model": "res.country",
        "method": "search",
        "domain": [["code", "=", "ZZ"]],
        "registry_sequence": env.registry.registry_sequence,
        "uid": principal.uid,
        "principal_groups": shim._principal_groups(principal),
    }
    db = engine_py.RustDb(dsn_for(env.cr.dbname))
    conn = db.connect()
    try:
        group_spec_verdicts(env, engine_py, db, conn, export)
        for label, marker in (
            ("fresh", False),
            ("class policy", True),
            ("old export", "missing"),
            ("null", None),
            ("malformed", "false"),
        ):
            candidate = copy.deepcopy(export)
            field = candidate["models"]["res.country"]["fields"]["code"]
            if marker == "missing":
                del field["python_read_access"]
            else:
                field["python_read_access"] = marker
            kernel = engine_py.RustKernel.build(db, json.dumps(candidate))
            for su in (False, True):
                request["su"] = su
                expected = (
                    None
                    if su or marker is False
                    else "Python read-access class policy"
                    if marker is True
                    else "unknown read-access policy"
                )
                error = None
                answer = None
                try:
                    answer = kernel.search_where(
                        conn, json.dumps(request), offline=False
                    )
                except engine_py.KernelRefused as exc:
                    error = str(exc)
                _logger.debug(
                    "export=%s su=%s answer=%r error=%r", label, su, answer, error
                )
                if expected is None:
                    assert error is None and answer is not None, (label, su, error)
                else:
                    assert error is not None and expected in error, (label, su, error)
    finally:
        conn.rollback()
        conn.close()
    print("REGISTRY POLICY OK (5 metadata states, user and superuser)")


if __name__ == "__main__":
    run(env)  # noqa: F821 - supplied by odoo-bin shell
