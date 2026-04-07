use std::path::PathBuf;
use thiserror::Error;
use tracing::instrument;

use crate::backend::BackendError;

#[derive(Debug, Error)]
pub enum DownloadError {
    /// A transport-level error reported by the active [`crate::backend::DownloadBackend`].
    ///
    /// Retryability is determined by [`BackendError::is_retryable`] so the
    /// scheduler does not need to know which backend is in use.
    #[error("Network error: {0}")]
    Network(BackendError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Download was cancelled")]
    Cancelled,

    #[error("Retry limit exceeded: {last_error}")]
    RetriesExhausted { last_error: Box<DownloadError> },

    #[error("Download manager has been shut down")]
    ManagerShutdown,

    #[error("File already exists: {path}")]
    FileExists { path: PathBuf },

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Unknown error: {0}")]
    Unknown(String),
}

impl DownloadError {
    /// Classify whether this error should be retried by the scheduler.
    ///
    /// For [`DownloadError::Network`] variants the decision is delegated to
    /// [`BackendError::is_retryable`], keeping retry logic co-located with the
    /// error classification that backends are responsible for.
    ///
    /// [`DownloadError::Cancelled`] and [`DownloadError::Io`] are never
    /// retried; all other variants are treated as terminal by default.
    #[instrument(level = "trace", skip(self))]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Network(backend_err) => backend_err.is_retryable(),
            Self::Cancelled | Self::Io(_) => false,
            _ => false,
        }
    }
}
