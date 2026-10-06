def is_even(n: int) -> bool:
    return True if n == 0 else is_odd(n - 1)


def is_odd(n: int) -> bool:
    return False if n == 0 else is_even(n - 1)


def factorial(n: int) -> int:
    return 1 if n <= 1 else n * factorial(n - 1)
