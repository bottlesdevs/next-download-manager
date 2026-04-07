use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{DownloadManagerConfig, backend::DownloadBackend, events::EventBus};

/// Unique identifier for a download; monotonically increasing u64.
pub type DownloadID = u64;

/// Shared runtime context for coordinating downloads. Internal to the crate.
///
/// Holds the concurrency semaphore, root cancellation token, pluggable download
/// backend, atomic counters, and the global [`Event`](crate::events::Event)
/// broadcast sender. Cloned (via [`Arc`]) and shared across the scheduler and
/// every spawned worker task.
pub(crate) struct Context {
    /// Semaphore limiting the number of concurrently active downloads.
    pub semaphore: Arc<Semaphore>,

    /// Root cancellation token. Cancelling it cascades to all child tokens,
    /// cooperatively stopping every in-flight download.
    pub cancel_root: CancellationToken,

    /// Pluggable transport backend (e.g. `ReqwestBackend`).
    ///
    /// Wrapped in an [`Arc`] so it can be shared cheaply across tasks without
    /// requiring the backend to be `Clone`.
    pub backend: Arc<dyn DownloadBackend>,

    /// Monotonic counter used to generate [`DownloadID`] values. Starts at 1.
    pub id_counter: AtomicU64,

    /// Number of currently active (running, not queued) downloads.
    pub active: AtomicUsize,

    /// Configured maximum concurrency. Informational; the semaphore is the
    /// true enforcement mechanism.
    pub max_concurrent: AtomicUsize,

    /// Global [`Event`](crate::events::Event) broadcaster (bounded buffer, 1 024
    /// slots). Slow subscribers may miss events; use
    /// [`EventBus::events`](crate::events::EventBus::events) for a
    /// lagged-message-safe stream.
    pub events: EventBus,
}

// Manual Debug impl because `Arc<dyn DownloadBackend>` is not Debug by default
// (the trait does not require it) and we don't want to force that bound on
// every implementor.
impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field(
                "max_concurrent",
                &self.max_concurrent.load(Ordering::Relaxed),
            )
            .field("active", &self.active.load(Ordering::Relaxed))
            .field("id_counter", &self.id_counter.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Context {
    /// Create a new shared [`Context`].
    ///
    /// - Initialises the semaphore with `config.max_concurrent` permits.
    /// - Stores the provided `cancel_root` token (the manager's shutdown token
    ///   child).
    /// - Wraps `backend` in an [`Arc`] for cheap sharing across tasks.
    /// - Creates a fresh broadcast channel for global events (capacity 1 024).
    pub fn new(
        config: DownloadManagerConfig,
        cancel_root: CancellationToken,
        backend: Arc<dyn DownloadBackend>,
    ) -> Arc<Self> {
        let ctx = Arc::new(Self {
            semaphore: Arc::new(Semaphore::new(config.max_concurrent)),
            max_concurrent: AtomicUsize::new(config.max_concurrent),
            cancel_root,
            active: AtomicUsize::new(0),
            id_counter: AtomicU64::new(1),
            backend,
            events: EventBus::new(),
        });

        info!(
            max_concurrent = config.max_concurrent,
            "Context initialized"
        );

        ctx
    }

    /// Atomically generate the next [`DownloadID`] (relaxed ordering).
    ///
    /// IDs are unique within the lifetime of this [`Context`] and start at 1.
    #[inline]
    pub fn next_id(&self) -> DownloadID {
        self.id_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Create a child [`CancellationToken`] tied to the manager's root token.
    ///
    /// Cancelling the root (e.g. on [`DownloadManager::shutdown`]) cascades to
    /// all children.
    #[inline]
    pub fn child_token(&self) -> CancellationToken {
        self.cancel_root.child_token()
    }

    /// Cancel the root token, cooperatively stopping all in-flight downloads.
    pub fn cancel_all(&self) {
        self.cancel_root.cancel();
    }
}
