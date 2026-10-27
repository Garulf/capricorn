use std::io;
use std::path::PathBuf;

pub const EXIT_FAILURE: u8 = 1;
pub const EXIT_LOCKED: u8 = 2;
pub const EXIT_UNREACHABLE: u8 = 3;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Failed(String),
    #[error("{0}")]
    Locked(String),
    #[error("cannot reach capricornd at {}: {source} (is the daemon running?)", path.display())]
    Unreachable { path: PathBuf, source: io::Error },
}

pub type Result<T> = std::result::Result<T, CliError>;

impl CliError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }

    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Failed(_) => EXIT_FAILURE,
            Self::Locked(_) => EXIT_LOCKED,
            Self::Unreachable { .. } => EXIT_UNREACHABLE,
        }
    }
}

impl From<io::Error> for CliError {
    fn from(err: io::Error) -> Self {
        Self::Failed(err.to_string())
    }
}

impl From<capricorn_hypr::Error> for CliError {
    fn from(err: capricorn_hypr::Error) -> Self {
        Self::Failed(err.to_string())
    }
}
