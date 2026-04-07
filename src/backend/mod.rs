mod error;
#[cfg(feature = "reqwest")]
mod reqwest_backend;

pub use error::BackendError;
#[cfg(feature = "reqwest")]
pub use reqwest_backend::ReqwestBackend;

use crate::download::RemoteInfo;
use bytes::Bytes;
use futures_core::Stream;
use http::HeaderMap;
use std::{future::Future, pin::Pin};
use tokio_util::sync::CancellationToken;
use url::Url;

/// The response yielded by a successful [`DownloadBackend::fetch`] call.
///
/// Carries the optional total size advertised by the server and a pinned
/// stream of raw byte chunks.
pub struct BackendResponse {
    /// Total byte count as reported by the server's `Content-Length` header,
    /// or `None` when the server did not include one.
    pub content_length: Option<u64>,

    /// Stream of raw byte chunks arriving from the server.
    ///
    /// Each item is `Ok(Bytes)` on success or `Err(BackendError)` when the
    /// underlying transport encounters an error mid-stream.
    pub stream: Pin<Box<dyn Stream<Item = Result<Bytes, BackendError>> + Send + 'static>>,
}

/// Abstraction over the transport layer used to perform downloads.
///
/// Implement this trait to provide a custom HTTP (or non-HTTP) backend.
/// The [`ReqwestBackend`] provided behind the `reqwest` Cargo feature is the
/// built-in default.
///
/// # Object safety
///
/// The trait is object-safe when called through `Arc<dyn DownloadBackend>`.
/// Both methods return boxed futures so that the trait can be used as a
/// dynamic dispatch target without requiring `async_trait`.
///
/// # Cancellation
///
/// Both methods receive a [`CancellationToken`] for the in-flight download.
/// Implementations should honour cancellation as early as possible (e.g.
/// aborting the TCP connection rather than waiting for a response). When a
/// probe is cancelled `probe_head` should return `None`; when a fetch is
/// cancelled before the response headers arrive `fetch` should return
/// `Err(BackendError::Cancelled)`.  Chunk-level cancellation during streaming
/// is handled by the worker via `tokio::select!` so backends are not required
/// to poll the token inside the returned stream.
///
/// # Example — minimal backend stub
///
/// ```rust,ignore
/// use std::{future::Future, pin::Pin};
/// use bytes::Bytes;
/// use http::HeaderMap;
/// use tokio_util::sync::CancellationToken;
/// use url::Url;
/// use bottles_download_manager::backend::{
///     BackendError, BackendResponse, DownloadBackend,
/// };
/// use bottles_download_manager::download::RemoteInfo;
///
/// struct MyBackend;
///
/// impl DownloadBackend for MyBackend {
///     fn probe_head<'a>(
///         &'a self,
///         _url: &'a Url,
///         _headers: &'a HeaderMap,
///         _cancel: &'a CancellationToken,
///     ) -> Pin<Box<dyn Future<Output = Option<RemoteInfo>> + Send + 'a>> {
///         Box::pin(async { None })
///     }
///
///     fn fetch<'a>(
///         &'a self,
///         _url: &'a Url,
///         _headers: &'a HeaderMap,
///         _cancel: &'a CancellationToken,
///     ) -> Pin<Box<dyn Future<Output = Result<BackendResponse, BackendError>> + Send + 'a>> {
///         Box::pin(async {
///             Err(BackendError::Other("not implemented".into()))
///         })
///     }
/// }
/// ```
pub trait DownloadBackend: Send + Sync + 'static {
    /// Perform a best-effort `HEAD` probe and return metadata about the remote
    /// resource.
    ///
    /// Implementations should return `None` when:
    /// - the server does not support `HEAD`,
    /// - the response indicates an error,
    /// - or the cancellation token is triggered before a response arrives.
    ///
    /// Returning `None` is safe; the download will proceed without pre-flight
    /// metadata and no [`crate::events::Event::Probed`] event will be emitted.
    fn probe_head<'a>(
        &'a self,
        url: &'a Url,
        headers: &'a HeaderMap,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Option<RemoteInfo>> + Send + 'a>>;

    /// Issue a `GET` request and return a streaming response.
    ///
    /// The implementation is responsible for:
    /// - establishing the connection,
    /// - reading and surfacing the `Content-Length` header (if present),
    /// - returning a `Stream` of `Bytes` chunks.
    ///
    /// Returns `Err(BackendError::Cancelled)` if the cancellation token fires
    /// before the response headers are received.  Mid-stream cancellation is
    /// managed by the caller.
    fn fetch<'a>(
        &'a self,
        url: &'a Url,
        headers: &'a HeaderMap,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<BackendResponse, BackendError>> + Send + 'a>>;
}
