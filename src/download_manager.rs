pub mod backend;
mod context;
mod download;
mod error;
mod events;
mod request;
mod scheduler;
mod worker;

pub mod prelude {
    pub use crate::{
        backend::{BackendError, DownloadBackend},
        context::DownloadID,
        download::{Download, DownloadResult},
        error::DownloadError,
        events::{Event, Progress},
        request::Request,
    };

    #[cfg(feature = "reqwest")]
    pub use crate::backend::ReqwestBackend;
}

use crate::{
    backend::DownloadBackend,
    context::Context,
    request::RequestBuilder,
    scheduler::{Scheduler, SchedulerCmd},
};
use derive_builder::Builder;
use futures_core::Stream;
use prelude::*;
use std::{
    path::Path,
    sync::{Arc, atomic::Ordering},
};
use tokio::sync::{broadcast, mpsc};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::{debug, info, instrument, trace, warn};
use url::Url;

/// Entry point for scheduling, observing, and cancelling downloads.
///
/// # Behavior
/// - Enforces a global concurrency limit across all downloads.
/// - Publishes global [`Event`] notifications and exposes per-download streams
///   via [`Download`].
///
/// # Backend injection
///
/// By default (when the `reqwest` Cargo feature is enabled) the manager uses
/// [`ReqwestBackend`].  To supply a custom backend implement [`DownloadBackend`]
/// and use [`DownloadManager::with_backend`] or [`DownloadManager::new`].
///
/// ```rust,ignore
/// use bottles_download_manager::{DownloadManager, DownloadManagerConfig};
/// use my_crate::MyBackend;
///
/// // Custom backend, default config
/// let manager = DownloadManager::with_backend(MyBackend::new());
///
/// // Custom backend + custom config
/// let config = DownloadManagerConfig::default();
/// let manager = DownloadManager::new(config, MyBackend::new());
/// ```
///
/// # Notes
/// - Events are delivered over a broadcast channel with a bounded buffer; slow
///   consumers can miss events.
/// - Use [`DownloadManager::events`] to get a stream that drops lagged messages
///   gracefully.
/// - Use [`DownloadManager::shutdown`] for a graceful stop: it cancels all work
///   and waits for workers to finish.
pub struct DownloadManager {
    scheduler_tx: mpsc::Sender<SchedulerCmd>,
    ctx: Arc<Context>,
    tracker: TaskTracker,
    shutdown_token: CancellationToken,
}

/// `Default` is only available when the `reqwest` feature is enabled, because
/// it requires a concrete backend to be chosen automatically.
#[cfg(feature = "reqwest")]
impl Default for DownloadManager {
    #[instrument(level = "debug")]
    fn default() -> Self {
        DownloadManager::with_config(DownloadManagerConfig::default())
    }
}

impl DownloadManager {
    /// Create a manager with a custom [`DownloadManagerConfig`] and the default
    /// [`ReqwestBackend`].
    ///
    /// Only available when the `reqwest` Cargo feature is enabled (it is on by
    /// default).  For a fully custom backend use [`DownloadManager::new`].
    #[cfg(feature = "reqwest")]
    #[instrument(level = "info", skip(config))]
    pub fn with_config(config: DownloadManagerConfig) -> DownloadManager {
        use crate::backend::ReqwestBackend;
        DownloadManager::new(config, ReqwestBackend::default())
    }

    /// Create a manager with the default [`DownloadManagerConfig`] and a custom
    /// backend.
    ///
    /// This is a convenience wrapper around [`DownloadManager::new`] for when
    /// you only need to override the backend.
    ///
    /// ```rust,ignore
    /// let manager = DownloadManager::with_backend(MyBackend::new());
    /// ```
    #[instrument(level = "info", skip(backend))]
    pub fn with_backend<B: DownloadBackend>(backend: B) -> DownloadManager {
        DownloadManager::new(DownloadManagerConfig::default(), backend)
    }

    /// Create a manager with both a custom [`DownloadManagerConfig`] and a
    /// custom backend.
    ///
    /// This is the most general constructor; all other constructors delegate
    /// here.
    ///
    /// # Panics
    ///
    /// Does not panic.  The config builder can return an error only if
    /// `max_concurrent` is 0; [`DownloadManagerConfig::default`] sets it to 3.
    #[instrument(level = "info", skip(config, backend))]
    pub fn new<B: DownloadBackend>(config: DownloadManagerConfig, backend: B) -> DownloadManager {
        let (cmd_tx, cmd_rx) = mpsc::channel(1024);
        let tracker = TaskTracker::new();
        let shutdown_token = CancellationToken::new();
        let ctx = Context::new(config, shutdown_token.child_token(), Arc::new(backend));
        let scheduler =
            Scheduler::new(shutdown_token.clone(), ctx.clone(), tracker.clone(), cmd_rx);

        let manager = DownloadManager {
            scheduler_tx: cmd_tx,
            ctx: ctx.clone(),
            tracker: tracker.clone(),
            shutdown_token,
        };

        tracker.spawn(async move { scheduler.run().await });

        let max = ctx.max_concurrent.load(Ordering::Relaxed);
        info!(
            max_concurrent = max,
            "DownloadManager initialized and scheduler started"
        );

        manager
    }

    /// Start a download with default request settings.
    ///
    /// - Returns a [`Download`] handle which is also a `Future` yielding
    ///   [`DownloadResult`] or [`DownloadError`].
    /// - You can stream progress and per-download events from the returned
    ///   handle.
    /// - Cancellation: call [`Download::cancel`] on the handle or
    ///   [`DownloadManager::cancel`] with its ID.
    #[instrument(level = "info", skip(self, destination), fields(url = %url))]
    pub fn download(&self, url: Url, destination: impl AsRef<Path>) -> anyhow::Result<Download> {
        self.download_builder()
            .url(url)
            .destination(destination.as_ref())
            .start()
    }

    /// Create a [`RequestBuilder`] to customise a download (headers, retries,
    /// overwrite, callbacks).
    ///
    /// Use this if you need non-default behaviour or want to hook into
    /// progress / event callbacks before `start()`.
    #[instrument(level = "debug", skip(self))]
    pub fn download_builder(&self) -> RequestBuilder {
        Request::builder(self)
    }

    /// Best-effort attempt to request cancellation for a download by ID.
    ///
    /// - No-op if the job is already finished or missing.
    /// - Returns an error if the internal command channel is unavailable or the
    ///   buffer is full.
    #[instrument(level = "info", skip(self), fields(id = id))]
    pub fn try_cancel(&self, id: DownloadID) -> anyhow::Result<()> {
        match self.scheduler_tx.try_send(SchedulerCmd::Cancel { id }) {
            Ok(_) => {
                debug!(%id, "Cancel command enqueued (try_cancel)");
                Ok(())
            }
            Err(e) => {
                warn!(%id, error = %e, "Failed to send cancel command with try_send");
                Err(anyhow::anyhow!("Failed to send cancel command: {}", e))
            }
        }
    }

    /// Request cancellation for a download by ID.
    ///
    /// - No-op if the job is already finished or missing.
    /// - Returns an error only if the internal command channel is unavailable.
    #[instrument(level = "info", skip(self), fields(id = id))]
    pub async fn cancel(&self, id: DownloadID) -> anyhow::Result<()> {
        match self.scheduler_tx.send(SchedulerCmd::Cancel { id }).await {
            Ok(_) => {
                info!(%id, "Cancel command sent");
                Ok(())
            }
            Err(e) => {
                warn!(%id, error = %e, "Failed to send cancel command");
                Err(anyhow::anyhow!("Failed to send cancel command: {}", e))
            }
        }
    }

    /// Number of currently active (running) downloads.
    ///
    /// Does not include queued or delayed retries. Reflects active semaphore
    /// permits.
    #[instrument(level = "trace", skip(self))]
    pub fn active_downloads(&self) -> usize {
        let n = self.ctx.active.load(Ordering::Relaxed);
        trace!(active = n, "Active downloads");
        n
    }

    /// Cancel all queued and in-flight downloads managed by this instance.
    ///
    /// This triggers cooperative cancellation for workers and removes partial
    /// files.
    #[instrument(level = "info", skip(self))]
    pub fn cancel_all(&self) {
        info!("Cancelling all downloads");
        self.ctx.cancel_all();
    }

    /// Return a child [`CancellationToken`] tied to the manager's root token.
    #[instrument(level = "trace", skip(self))]
    pub fn child_token(&self) -> CancellationToken {
        self.ctx.child_token()
    }

    /// Subscribe to all [`Event`] notifications across the manager.
    ///
    /// The underlying broadcast channel has a bounded buffer (1 024). Slow
    /// consumers may lag and miss events. Consider using
    /// [`DownloadManager::events`] for a stream that skips lagged messages
    /// gracefully.
    #[instrument(level = "debug", skip(self))]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.ctx.events.subscribe()
    }

    /// A fallible-safe stream of global [`Event`] values.
    ///
    /// Internally wraps the broadcast receiver and filters out lagged / closed
    /// errors.
    #[instrument(level = "debug", skip(self))]
    pub fn events(&self) -> impl Stream<Item = Event> + 'static {
        self.ctx.events.events()
    }

    /// Gracefully stop the manager.
    ///
    /// - Cancels all in-flight work ([`DownloadManager::cancel_all`]).
    /// - Prevents new tasks from being scheduled and waits for all worker tasks
    ///   to finish.
    ///
    /// Call this before dropping the manager if you need deterministic
    /// teardown.
    #[instrument(level = "info", skip(self))]
    pub async fn shutdown(&self) {
        info!("Shutting down DownloadManager");
        self.shutdown_token.cancel();
        self.tracker.close();
        self.tracker.wait().await;
        info!("DownloadManager shutdown complete");
    }
}

#[derive(Builder)]
pub struct DownloadManagerConfig {
    /// Maximum number of downloads that may run concurrently.
    ///
    /// Must be greater than 0. Defaults to 3.
    #[builder(default = 3, setter(custom))]
    max_concurrent: usize,
}

impl Default for DownloadManagerConfig {
    #[instrument(level = "debug")]
    fn default() -> Self {
        DownloadManagerConfigBuilder::default().build().unwrap()
    }
}

impl DownloadManagerConfigBuilder {
    /// Set the maximum concurrent downloads.
    ///
    /// Returns an error if `value` is 0.
    #[instrument(level = "debug", skip(self))]
    fn max_concurrent(&mut self, value: usize) -> anyhow::Result<&mut Self> {
        let value = (value != 0)
            .then_some(value)
            .ok_or_else(|| anyhow::anyhow!("Max concurrent downloads must be greater than 0"))?;

        self.max_concurrent = Some(value);
        Ok(self)
    }
}
