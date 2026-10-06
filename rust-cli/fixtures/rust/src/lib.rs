//! Fixture crate for the tests.
pub mod chain;
pub mod cycle;
pub mod shapes;
pub mod unicode;
pub mod util;
pub mod windows;

pub use shapes::{Chart, Color, Render, Table};

/// Runs everything once.
pub fn run() -> String {
    let chart = Chart { title: String::from("t") };
    let table = Table;
    util::log("run");
    format!("{}{}{}", chart.render(), table.render(), chain::step_one())
}

pub const LIMIT: usize = 3;
pub type Id = String;

macro_rules! twice {
    ($e:expr) => {
        $e + $e
    };
}

pub fn doubled() -> usize {
    twice!(LIMIT)
}
