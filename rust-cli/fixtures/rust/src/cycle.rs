pub fn is_even(n: u32) -> bool {
    if n == 0 { true } else { is_odd(n - 1) }
}

pub fn is_odd(n: u32) -> bool {
    if n == 0 { false } else { is_even(n - 1) }
}

pub fn factorial(n: u64) -> u64 {
    if n <= 1 { 1 } else { n * factorial(n - 1) }
}
