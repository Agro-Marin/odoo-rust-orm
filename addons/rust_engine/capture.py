import json
import logging

_logger = logging.getLogger(__name__)

CAPTURE_METHODS = frozenset({
    "search_read", "web_search_read", "search_count", "read", "web_read",
    "read_group", "web_read_group", "name_search", "web_name_search",
})


def arm(path, db_name):
    from odoo.service import model as service_model

    orig_call_kw = service_model.call_kw

    def call_kw(model, name, args, kwargs):
        if name in CAPTURE_METHODS and model.env.cr.dbname == db_name:
            try:
                record = {
                    "model": model._name,
                    "method": name,
                    "args": list(args),
                    "kwargs": dict(kwargs),
                    "uid": model.env.uid,
                    "context": dict(model.env.context),
                }
                with open(path, "a") as fh:
                    fh.write(json.dumps(record, default=str) + "\n")
            except Exception:  # noqa: BLE001
                _logger.exception("rust_engine: could not capture %s.%s", model._name, name)
        return orig_call_kw(model, name, args, kwargs)

    service_model.call_kw = call_kw
    try:
        from odoo.addons.web.controllers import dataset

        dataset.call_kw = call_kw
    except ImportError:
        pass
    _logger.info("rust_engine: capturing read calls on %s to %s", db_name, path)
