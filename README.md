# next-download-manager

A small async download manager for Rust built on `reqwest` + `tokio`.  
It provides a simple high-level API to schedule downloads with a global concurrency limit, per-download progress and events, cooperative cancellation, retries with exponential backoff, and basic probing (HEAD) to discover remote metadata.

## Key features

- Global concurrency control across all downloads.
- Per-download handle that:
  - Implements `Future` returning a `DownloadResult`.
  - Exposes a progress stream and per-download events.
  - Allows cooperative cancellation.
- Global event broadcast for monitoring all downloads.
- Retry policy with exponential backoff for retryable network errors.
- Optional HEAD probe to surface remote metadata (content-length, ETag, etc.).
- Removal of partial files on cancellation / failure.

## Events & Progress

The crate emits typed `Event` values for lifecycle events:

- `Queued`, `Probed`, `Started`, `Retrying`, `Completed`, `Failed`, `Cancelled`.

You can subscribe to global events via `DownloadManager::subscribe()` (a `broadcast::Receiver`) or obtain a fallible-safe stream via `DownloadManager::events()` which drops lagged messages.

Per-download events are available from the `Download` handle via `download.events()`.

Progress sampling is implemented with a `Progress` struct exposing:
- `bytes_downloaded()`, `total_bytes()`
- `percent()` (Option)
- `instantaneous_bps`, `ema_bps` and `eta()` when available

Progress is emitted on a `watch` channel and is sampled to avoid flooding consumers.

## Cancellation

- Per-download: call `download.cancel()` on the returned handle or call the manager's `cancel(id)`.
- Global: `manager.cancel_all()` cancels all downloads.
- Shutdown: `manager.shutdown().await` will cancel in-flight work and wait for worker tasks to finish.

Cancellation is cooperative: it aborts the HTTP request and removes the partial file.

## Retry behavior

- Network errors are classified and retryable errors are retried with exponential backoff (base 1s, max 10s).
- The per-request retry limit defaults to 3 and is configurable via the `RequestBuilder` (`retries()`).
- Non-retryable errors (I/O, cancellation, some client errors) fail immediately.

## Logging / Tracing

The crate uses `tracing` for structured logs. Attach a subscriber in your binary (for example `tracing_subscriber::fmt::init()` or set up a more advanced subscriber) to see info/debug traces for lifecycle events, queueing, retries, and errors.

## License

This crate is GPL-3.0 (see `LICENSE`).
