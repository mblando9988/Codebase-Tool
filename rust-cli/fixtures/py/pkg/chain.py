from pkg.util.log import log


def step_one() -> int:
    return step_two() + 1


def step_two() -> int:
    return step_three() + 1


def step_three() -> int:
    return step_four() + 1


def step_four() -> int:
    log("four")
    return step_five() + 1


def step_five() -> int:
    return 5
