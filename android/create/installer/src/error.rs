use std::fmt::Display;
use std::path::Path;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },
    #[error("{0}")]
    Msg(String),
    #[error("cancelled")]
    Cancelled,
    #[error("ssh: {0}")]
    Ssh(String),
    #[error("sftp: {0}")]
    Sftp(String),
    #[error("http: {0}")]
    Http(String),
    #[error("apfs: {0}")]
    Apfs(String),
    #[error("signing: {0}")]
    Sign(String),
    #[error("plist: {0}")]
    Plist(#[from] plist::Error),
    #[error("{0}: not supported by this crate: {1}")]
    Unsupported(String, String),
}

impl Error {
    pub fn msg(text: impl Into<String>) -> Self {
        Error::Msg(text.into())
    }

    pub fn io(context: impl Display, source: std::io::Error) -> Self {
        Error::Io {
            context: context.to_string(),
            source,
        }
    }

    pub fn path(path: &Path, source: std::io::Error) -> Self {
        Error::io(path.display(), source)
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Error::Io {
            context: "io".into(),
            source,
        }
    }
}

/// Bail with a formatted message.
#[macro_export]
macro_rules! bail {
    ($($t:tt)*) => { return Err($crate::error::Error::msg(format!($($t)*))) };
}
