"""What the rust cursor refuses to send, refused as psycopg refuses it.

Set RUSTORM_TEST_DSN to a disposable database.
"""

import os

import pytest
from _env import usable_engine

NUL_IN_TEXT = "PostgreSQL text fields cannot contain NUL (0x00) bytes"


@pytest.fixture
def conn():
    dsn = os.environ.get("RUSTORM_TEST_DSN")
    if not dsn:
        pytest.skip("RUSTORM_TEST_DSN must name a disposable database")
    engine, unusable = usable_engine()
    if unusable:
        pytest.skip(unusable)
    held = engine.RustDb(dsn).connect()
    yield held
    held.close()


@pytest.mark.parametrize(
    ("query", "params"),
    [
        ("SELECT %s", ["a\x00b"]),
        ("SELECT %s::text[]", [["ok", "a\x00b"]]),
        ("SELECT %(name)s", {"name": "\x00"}),
        ("SELECT %s::int", ["1\x00"]),
    ],
)
def test_a_nul_in_a_text_parameter_is_psycopgs_data_error(conn, query, params) -> None:
    import psycopg

    with pytest.raises(psycopg.DataError) as raised:
        conn.execute(query, params)
    assert type(raised.value) is psycopg.DataError
    assert str(raised.value) == NUL_IN_TEXT
    assert conn.execute("SELECT %s", ["a\\x00b"]).rows == [("a\\x00b",)], (
        "refused before sending: the transaction goes on"
    )
