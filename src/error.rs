use std::fmt;

#[derive(Debug)]
pub enum MagpieError {
    Network(String),
    Parse(String),
    Io(String),
    NoResults(String),
}

impl fmt::Display for MagpieError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(s) => write!(f, "magpie: network: {s}"),
            Self::Parse(s) => write!(f, "magpie: parse: {s}"),
            Self::Io(s) => write!(f, "magpie: io: {s}"),
            Self::NoResults(s) => write!(f, "magpie: no results: {s}"),
        }
    }
}

impl std::error::Error for MagpieError {}

pub type Result<T> = std::result::Result<T, MagpieError>;

pub fn eprint_next(msg: &str, next: &str) {
    eprintln!("magpie: {msg}");
    eprintln!("  next: {next}");
}
