use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("io at {path}: {source}")]
    IoAt {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("config: {0}")]
    Config(String),

    #[error("manifest at {path}: {message}")]
    Manifest { path: PathBuf, message: String },

    #[error("preset at {path}: {message}")]
    Preset { path: PathBuf, message: String },

    #[error("applied state at {path}: {message}")]
    Applied { path: PathBuf, message: String },

    #[error("template `{template}`: {message}")]
    Template { template: String, message: String },

    #[error("git: {0}")]
    Git(String),

    #[error("merge: {0}")]
    Merge(String),

    #[error("ai backend `{agent}` not available: {reason}")]
    AiUnavailable { agent: String, reason: String },

    #[error("ai backend `{agent}` failed (exit={code:?}): {stderr}")]
    AiFailed {
        agent: String,
        code: Option<i32>,
        stderr: String,
    },

    #[error("project not registered: {0}")]
    PjUnknown(String),

    #[error("user cancelled")]
    Cancelled,

    #[error(transparent)]
    Tera(#[from] teravars::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    pub fn manifest(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Manifest {
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn preset(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Preset {
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn applied(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Applied {
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn template(template: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Template {
            template: template.into(),
            message: message.into(),
        }
    }

    pub fn io_at(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::IoAt {
            path: path.into(),
            source,
        }
    }
}
