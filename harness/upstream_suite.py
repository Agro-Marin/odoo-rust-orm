"""Run Odoo cases with its lifecycle, returning machine-readable outcomes."""

import unittest


def cases(suite):
    for item in suite:
        if isinstance(item, unittest.BaseTestSuite):
            yield from cases(item)
        else:
            yield item


def run(suite, *, only=(), full_traceback=False):
    import odoo.modules.module
    from odoo.tests.result import OdooTestResult
    from odoo.tests.suite import OdooSuite

    class Result(OdooTestResult):
        def __init__(self):
            super().__init__()
            self.problems = []
            self.outcomes = {}

        def startTest(self, test):
            self.outcomes[test.id()] = "ok"
            super().startTest(test)

        def addSuccess(self, test):
            self.outcomes[test.id()] = "ok"
            super().addSuccess(test)

        def addSkip(self, test, reason, infrastructure=False):
            self.outcomes[test.id()] = "skip"
            super().addSkip(test, reason, infrastructure=infrastructure)

        def logError(self, flavour, test, error):
            if not self._soft_fail:
                self.outcomes[test.id()] = "fail" if flavour == "FAIL" else "error"
                detail = self._exc_info_to_string(error, test).strip()
                self.problems.append(
                    (
                        test.id(),
                        detail if full_traceback else detail.splitlines()[-1][:180],
                    )
                )
            super().logError(flavour, test, error)

    selected = [
        case for case in cases(suite) if not only or case._testMethodName in only
    ]
    result = Result()
    result.outcomes = {test.id(): "not run" for test in selected}
    previous = odoo.modules.module.current_test
    try:
        OdooSuite(selected).run(result)
    finally:
        odoo.modules.module.current_test = previous
    return {
        "run": result.testsRun,
        "failures": result.failures_count,
        "errors": result.errors_count,
        "skipped": result.skipped,
        "infrastructure_skipped": result.infrastructure_skipped,
        "aborted": result.aborted,
        "problems": sorted({name for name, _detail in result.problems}),
        "detail": result.problems[:4],
        "outcomes": result.outcomes,
        "all_details": dict(result.problems),
    }
