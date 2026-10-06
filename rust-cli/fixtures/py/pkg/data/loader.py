from pkg.shapes import plain
from pkg.util.log import log


def load() -> int:
    log("load")
    return plain(1)
