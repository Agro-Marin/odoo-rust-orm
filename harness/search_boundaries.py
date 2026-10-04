"""Live Odoo parity for boundary values, including actual native fallback."""

import collections
import logging

_logger = logging.getLogger("odoo.rust_kernel.search_boundaries")


def run(env):
    import engine_py
    import rust_orm_shim

    port = engine_py.install_backend()
    assert type(env.cr.transaction.backend).__name__ == "RustBackend"
    original = port.RustBackend.NATIVE
    original_mode = rust_orm_shim.MODE
    armed = original | {"search", "search_raw"}
    disarmed = original - {"search", "search_raw"}
    cases = [
        ("res.country", [["code", "=", "USA"]]),
        ("res.country", [["code", "=", 1e-6]]),
        ("res.country", [["code", "<", 1e-6]]),
        ("res.country", [["code", "like", 1e-6]]),
        ("res.partner", [["id", "child_of", 4_294_967_297]]),
        ("res.partner", [["id", "parent_of", 4_294_967_297]]),
        ("res.partner", [["color", "=", "NaN"]]),
        ("res.partner", [["color", "=", "1_000"]]),
        ("res.partner", [["color", "=", "١٢"]]),
        ("res.partner", [["missing_audit_field", "=?", False]]),
        ("res.partner", [["missing_audit_field", "in", []]]),
        ("res.partner", [[2, "=", 2]]),
        ("res.partner", [[-1, "=", "not a number"]]),
    ]
    identities = [
        (1, True),
        (2, False),
        (env.ref("base.public_user").id, False),
    ]
    counts = collections.Counter()
    reasons = collections.Counter()
    try:
        rust_orm_shim.set_mode("on")
        for uid, su in identities:
            cenv = env(user=uid, su=su, context={"lang": "en_US"})
            for model_name, domain in cases:
                outcomes = []
                for enabled in (False, True):
                    port.RustBackend.NATIVE = armed if enabled else disarmed
                    port.reset_stats()
                    try:
                        with cenv.cr.savepoint():
                            query = cenv[model_name]._search(domain, order="id")
                            outcome = ("ids", tuple(query.get_result_ids()))
                    except Exception as exc:
                        outcome = (
                            "error",
                            type(exc).__module__,
                            type(exc).__name__,
                            str(exc),
                        )
                    outcomes.append(outcome)
                    stats = port.stats()
                    _logger.debug(
                        "uid=%s su=%s model=%s domain=%r enabled=%s outcome=%r stats=%r",
                        uid,
                        su,
                        model_name,
                        domain,
                        enabled,
                        outcome,
                        stats,
                    )
                    if enabled:
                        counts["native"] += sum(
                            stats["native"].get(method, 0)
                            for method in ("search", "search_raw")
                        )
                        for method in ("search", "search_raw"):
                            reasons.update(stats["reasons_by_method"].get(method, {}))
                assert outcomes[0] == outcomes[1], (
                    uid,
                    su,
                    model_name,
                    domain,
                    outcomes,
                )
                counts[outcomes[0][0]] += 1
        assert counts["native"] > 0, counts
        assert any("Python's formatting" in reason for reason in reasons), reasons
        print(f"SEARCH BOUNDARIES OK {dict(counts)}; delegations={dict(reasons)}")
    finally:
        port.RustBackend.NATIVE = original
        rust_orm_shim.set_mode(original_mode)


if __name__ == "__main__":
    run(env)  # noqa: F821 - supplied by odoo-bin shell
