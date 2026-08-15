use thiserror::Error;

/// Transport-agnostic error type returned by [`super::DownloadBackend`] implementations.
///
/// Backends are responsible for converting their native error types into this
/// enum so that the rest of the download pipeline can reason about
/// retryability without coupling itself to any particular HTTP library.
#[derive(Debug, Error)]
pub enum BackendError {
    /// TCP / socket-level connection to the remote host failed.
    #[error("Connection failed: {0}")]
    Connect(String),

    /// The request or response exceeded a time limit.
    #[error("Request timed out: {0}")]
    Timeout(String),

    /// A well-formed request was sent but could not be completed.
    #[error("Request error: {0}")]
    Request(String),

    /// The server returned an HTTP 5xx status code.
    #[error("HTTP server error {status}: {message}")]
    ServerError { status: u16, message: String },

    /// The operation was aborted via the download's [`tokio_util::sync::CancellationToken`].
    #[error("Cancelled")]
    Cancelled,

    /// Any other transport-layer error that does not fit a specific category.
    #[error("Network error: {0}")]
    Other(String),
}

impl BackendError {
    /// Returns `true` if the scheduler should schedule a retry after this error.
    ///
    /// The following variants are considered **retryable** (transient failures
    /// that have a reasonable chance of succeeding on a subsequent attempt):
    ///
    /// - [`BackendError::Connect`]
    /// - [`BackendError::Timeout`]
    /// - [`BackendError::Request`]
    /// - [`BackendError::ServerError`] (HTTP 5xx)
    ///
    /// [`BackendError::Cancelled`] and [`BackendError::Other`] are **not**
    /// retryable.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            BackendError::Connect(_)
                | BackendError::Timeout(_)
                | BackendError::Request(_)
                | BackendError::ServerError { .. }
        )
    }
}
