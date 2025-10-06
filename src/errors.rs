use thiserror::Error;
use tracing_appender::rolling::InitError;

#[derive(Error, Debug)]
pub enum ServeError {
    #[error("Notify errors {0}")]
    Notify(#[from] notify::Error),
    #[error("IO errors {0}")]
    Io(#[from] std::io::Error),
    #[error("Not a directory: {0}")]
    NotADirectory(String),
    #[error("Invalid path: {0}")]
    InvalidPath(String),
    #[error("Log initialization error: {0}")]
    LogInit(#[from] InitError),
    #[error("{0}\n\n{0:?}")]
    Toml(#[from] toml::de::Error),
    #[error("toml serialize error: {0}")]
    TomlEdit(#[from] toml::ser::Error),
    #[error("Error converting OsString( {0:?} ) to String")]
    OsStringConversionError(std::ffi::OsString),
    #[error("Config does not have '{0}' field")]
    GenerateConfig(String),
}

impl From<std::ffi::OsString> for ServeError {
    fn from(value: std::ffi::OsString) -> Self {
        ServeError::OsStringConversionError(value)
    }
}
