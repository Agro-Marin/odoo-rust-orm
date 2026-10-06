from collections.abc import Callable
from typing import TYPE_CHECKING, Literal, cast

import rust_orm_shim

from odoo.exceptions import AccessError, UserError

if TYPE_CHECKING:
    from odoo.api import Environment


def run_read_and_rollback[T](
    env: Environment, mode: Literal["on", "off"], call: Callable[[], T]
) -> tuple[T | str, bool]:
    """Run one comparison read, roll back, and report whether the kernel served it."""
    rust_orm_shim.set_mode(mode)
    routed = cast("int", rust_orm_shim.STATS["kernel"])
    answer: T | str
    try:
        answer = call()
    except (AccessError, UserError) as exc:
        answer = type(exc).__name__
    finally:
        env.cr.rollback()
    return answer, cast("int", rust_orm_shim.STATS["kernel"]) > routed
