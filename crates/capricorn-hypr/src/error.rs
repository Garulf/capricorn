use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("environment variable {0} is not set")]
    MissingEnv(&'static str),
    #[error("hyprland socket I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("hyprland did not answer within the timeout")]
    Timeout,
    #[error("unexpected hyprland reply: {0}")]
    Json(#[from] serde_json::Error),
    #[error("hyprland rejected the request: {0}")]
    Rejected(String),
}

pub type Result<T> = std::result::Result<T, Error>;
