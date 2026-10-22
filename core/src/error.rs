use thiserror::Error;

#[derive(Error, Debug)]
pub enum JarvisError {
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Tool error: {0}")]
    Tool(String),
    #[error("Memory error: {0}")]
    Memory(String),
    #[error("IPC error: {0}")]
    Ipc(String),
    #[error("Model error: {0}")]
    Model(String),
    #[error("Permission denied: {0}")]
    Permission(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, JarvisError>;

impl From<reqwest::Error> for JarvisError {
    fn from(err: reqwest::Error) -> Self {
        JarvisError::Model(err.to_string())
    }
}
