import datetime as dt


def ser(v):
    if isinstance(v, (tuple, list)):
        return [ser(x) for x in v]
    if isinstance(v, dict):
        return {k: ser(x) for k, x in v.items()}
    if isinstance(v, dt.datetime):
        return v.strftime(
            "%Y-%m-%d %H:%M:%S.%f" if v.microsecond else "%Y-%m-%d %H:%M:%S"
        )
    if isinstance(v, dt.date):
        return v.strftime("%Y-%m-%d")
    return v
