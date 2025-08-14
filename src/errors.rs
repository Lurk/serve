use thiserror::Error;
use tracing_appender::rolling::InitError;

#[derive(Error, Debug)]
pub enum ServeError {
    #[error("Notify errors")]
    Notify(#[from] notify::Error),
    #[error("IO errors")]
    Io(#[from] std::io::Error),
    #[error("Not a directory: {0}")]
    NotADirectory(String),
    #[error("Log initialization error: {0}")]
    LogInit(#[from] InitError),
}
