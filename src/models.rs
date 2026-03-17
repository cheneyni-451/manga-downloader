use std::fmt::Display;

#[derive(Debug, Clone)]
pub struct Chapter {
    pub url: String,
    pub title: String,
}

impl Display for Chapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.title)
    }
}
