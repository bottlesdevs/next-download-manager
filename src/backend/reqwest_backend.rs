use std::{future::Future, pin::Pin};

use bytes::Bytes;
use futures_util::StreamExt as _;
use http::HeaderMap;
use reqwest::{Client, Method};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};
use url::Url;

use super::{BackendError, BackendResponse, DownloadBackend};
use crate::download::RemoteInfo;

impl From<reqwest::Error> for BackendError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            BackendError::Timeout(e.to_string())
        } else if e.is_connect() {
            BackendError::Connect(e.to_string())
        } else if e.is_request() {
            BackendError::Request(e.to_string())
        } else if let Some(status) = e.status() {
            if status.is_server_error() {
                BackendError::ServerError {
                    status: status.as_u16(),
                    message: e.to_string(),
                }
            } else {
                BackendError::Other(e.to_string())
            }
        } else {
            BackendError::Other(e.to_string())
        }
    }
}

/// A [`DownloadBackend`] backed by [`reqwest`].
///
/// Uses a shared [`reqwest::Client`] internally for connection pooling and
/// keep-alive reuse across downloads.
///
/// # Construction
///
/// | Method | When to use |
/// |---|---|
/// | [`ReqwestBackend::default()`] | Quick start — plain client, no extra config. |
/// | [`ReqwestBackend::from_client`] | Reuse an existing client with custom TLS, proxy, or timeout settings. |
///
/// # Example
///
/// ```rust,ignore
/// use reqwest::ClientBuilder;
/// use std::time::Duration;
/// use bottles_download_manager::{DownloadManager, backend::ReqwestBackend};
///
/// let client = ClientBuilder::new()
///     .timeout(Duration::from_secs(30))
///     .user_agent("my-app/1.0")
///     .build()
///     .unwrap();
///
/// let manager = DownloadManager::with_backend(ReqwestBackend::from_client(client));
/// ```
pub struct ReqwestBackend {
    client: Client,
}

impl Default for ReqwestBackend {
    fn default() -> Self {
        ReqwestBackend {
            client: Client::new(),
        }
    }
}

impl ReqwestBackend {
    /// Wrap an existing [`reqwest::Client`].
    ///
    /// Prefer this over [`Default`] when you need custom TLS roots, a proxy,
    /// connection timeouts, or a specific `User-Agent`.
    pub fn from_client(client: Client) -> Self {
        ReqwestBackend { client }
    }

    /// Return a reference to the underlying [`reqwest::Client`].
    pub fn client(&self) -> &Client {
        &self.client
    }
}

impl std::fmt::Debug for ReqwestBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReqwestBackend").finish_non_exhaustive()
    }
}

impl DownloadBackend for ReqwestBackend {
    /// Send an HTTP `HEAD` request and surface the response metadata.
    ///
    /// Cancellation is handled cooperatively: if the token fires before the
    /// server responds, the future returns `None` immediately and the
    /// in-flight request is dropped.
    fn probe_head<'a>(
        &'a self,
        url: &'a Url,
        headers: &'a HeaderMap,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Option<RemoteInfo>> + Send + 'a>> {
        Box::pin(async move {
            use reqwest::header as rh;

            debug!(%url, "ReqwestBackend: HEAD probe");

            let req = self
                .client
                .request(Method::HEAD, url.as_str())
                .headers(headers.clone())
                .send();

            let resp = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    debug!(%url, "HEAD probe cancelled before response");
                    return None;
                }
                result = req => match result {
                    Ok(r) => match r.error_for_status() {
                        Ok(r) => r,
                        Err(e) => {
                            debug!(%url, error = %e, "HEAD probe returned error status");
                            return None;
                        }
                    },
                    Err(e) => {
                        debug!(%url, error = %e, "HEAD probe request failed");
                        return None;
                    }
                },
            };

            let content_length = resp.content_length();
            trace!(%url, content_length = ?content_length, "HEAD response received");

            let h = resp.headers();

            Some(RemoteInfo {
                content_length,
                accept_ranges: h
                    .get(rh::ACCEPT_RANGES)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
                etag: h
                    .get(rh::ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
                last_modified: h
                    .get(rh::LAST_MODIFIED)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
                content_type: h
                    .get(rh::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
            })
        })
    }

    /// Send an HTTP `GET` request and return a streaming [`BackendResponse`].
    ///
    /// - If the cancellation token fires before the response headers arrive,
    ///   returns `Err(BackendError::Cancelled)`.
    /// - The returned `stream` yields `Ok(Bytes)` chunks as they arrive from
    ///   the server, or `Err(BackendError)` if the connection drops mid-stream.
    /// - Chunk-level cancellation during streaming is handled externally by
    ///   the worker via `tokio::select!`.
    fn fetch<'a>(
        &'a self,
        url: &'a Url,
        headers: &'a HeaderMap,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<BackendResponse, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            debug!(%url, "ReqwestBackend: GET fetch");

            let req = self
                .client
                .request(Method::GET, url.as_str())
                .headers(headers.clone())
                .send();

            let resp = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    debug!(%url, "GET fetch cancelled before response headers");
                    return Err(BackendError::Cancelled);
                }
                result = req => {
                    result
                        .map_err(BackendError::from)?
                        .error_for_status()
                        .map_err(BackendError::from)?
                }
            };

            let content_length = resp.content_length();
            debug!(%url, content_length = ?content_length, "GET response headers received");

            // Map each reqwest chunk error to a BackendError so the stream
            // item type matches the trait's contract.
            let stream = resp
                .bytes_stream()
                .map(|result: Result<Bytes, reqwest::Error>| result.map_err(BackendError::from));

            Ok(BackendResponse {
                content_length,
                stream: Box::pin(stream),
            })
        })
    }
}
