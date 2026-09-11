import datetime


def ser(v):
    if isinstance(v, (tuple, list)):
        return [ser(x) for x in v]
    if isinstance(v, dict):
        return {k: ser(x) for k, x in v.items()}
    if isinstance(v, datetime.datetime):
        return v.strftime(
            "%Y-%m-%d %H:%M:%S.%f" if v.microsecond else "%Y-%m-%d %H:%M:%S"
        )
    if isinstance(v, datetime.date):
        return v.strftime("%Y-%m-%d")
    return v
