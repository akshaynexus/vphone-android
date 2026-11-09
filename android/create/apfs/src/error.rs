use std::fmt;

/// Every failure in this crate carries a human-readable reason; a corrupt or
/// unsupported image is a normal error, never a panic.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// The image is not valid APFS, or a structure inside it is inconsistent.
    Corrupt(String),
    /// The image is valid but needs something this implementation refuses.
    Unsupported(String),
    /// A named path or record does not exist.
    NotFound(String),
    /// The operation does not apply (not a directory, already exists, ...).
    Invalid(String),
    /// The volume or container has no room left.
    NoSpace(String),
    /// The write engine refused or failed an operation.
    Write(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Corrupt(m) => write!(f, "corrupt image: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
            Error::Invalid(m) => write!(f, "invalid operation: {m}"),
            Error::NoSpace(m) => write!(f, "no space: {m}"),
            Error::Write(m) => write!(f, "write failed: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn corrupt<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Corrupt(msg.into()))
}
