use crate::util::log;

pub fn step_one() -> u32 {
    step_two() + 1
}
pub fn step_two() -> u32 {
    step_three() + 1
}
pub fn step_three() -> u32 {
    step_four() + 1
}
pub fn step_four() -> u32 {
    log("four");
    step_five() + 1
}
pub fn step_five() -> u32 {
    5
}
