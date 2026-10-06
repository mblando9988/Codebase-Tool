use crate::util::log;

/// Anything that can draw itself as text.
pub trait Render {
    fn render(&self) -> String;
}

pub struct Chart {
    pub title: String,
}

impl Render for Chart {
    fn render(&self) -> String {
        log("chart");
        self.title.clone()
    }
}

pub struct Table;

impl Render for Table {
    fn render(&self) -> String {
        log("table");
        String::from("table")
    }
}

pub enum Color {
    Red,
    Green,
}

pub struct Pair { pub left: u32,
    pub right: u32 }
