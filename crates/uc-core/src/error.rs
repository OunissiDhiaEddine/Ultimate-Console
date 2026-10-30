use std::path::PathBuf;

/// Errors produced by `uc-core`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid config file {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },

    #[error("config file {path} has schema {found}, this build supports up to {supported}")]
    ConfigSchemaTooNew {
        path: PathBuf,
        found: u32,
        supported: u32,
    },

    #[error("invalid config: {0}")]
    ConfigInvalid(String),

    #[error("could not serialise config: {0}")]
    ConfigSerialize(#[from] toml::ser::Error),

    #[error("invalid state file {path}: {source}")]
    StateParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid hotkey {input:?}: {reason}")]
    Hotkey { input: String, reason: String },

    #[error("invalid IPC message: {0}")]
    Ipc(#[from] serde_json::Error),

    #[error("environment variable {0} is not set")]
    MissingEnv(&'static str),

    #[error("logging setup failed: {0}")]
    Logging(String),
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
