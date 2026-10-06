from pkg.shapes import Chart, Table, decorated, plain
from pkg.util.log import log


def draw() -> str:
    c = Chart()
    t = Table()
    log("draw")
    return c.render() + t.render() + str(decorated(1)) + str(plain(2))


def other() -> int:
    return plain(3)
