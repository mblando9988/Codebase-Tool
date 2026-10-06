import functools

from pkg.util.log import log


class Chart:
    """A chart. The documentation here is deliberately long so that anything which clips long
    documentation has something to clip: it keeps going well past the limit that the server
    applies to a single text field, and it repeats itself to be sure of that. It keeps going
    well past the limit that the server applies to a single text field, and it repeats itself
    to be sure of that."""

    def render(self) -> str:
        log("chart")
        return "chart"


class Table:
    def render(self) -> str:
        log("table")
        return "table"


@functools.lru_cache
def decorated(n: int) -> int:
    return n + 1


def plain(n: int) -> int:
    return n * 2
