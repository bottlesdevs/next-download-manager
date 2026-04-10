# Why Are Core Types Mutable?

One of the first things you'll notice when working with this codebase is that many types are mutable (`&mut self` or `mut self`). This might seem unusual if you're used to immutable data structures. This document explains why.

## The Short Answer

The types are mutable because they need to:
1. **Send messages** on channels
2. **Spawn background tasks**
3. **Track changing state** (counters, queues)
4. **Coordinate between async tasks**

Let's break this down.

## What is Mutability in Rust?

In Rust, data can be **immutable** (read-only) or **mutable** (can be modified):

```rust
// Immutable - can't change x
let x = 5;

// Mutable - can change x  
let mut x = 5;
x = 10;
```

When you see `&mut self` in method signatures, it means the method needs to modify the object.

## Why DownloadManager Needs Mutability

```rust
pub struct DownloadManager {
    scheduler_tx: mpsc::Sender<SchedulerCmd>,
    ctx: Arc<Context>,
    tracker: TaskTracker,
    shutdown_token: CancellationToken,
}
```

### 1. Channel Senders Need Mutability

```rust
// From download_manager.rs
pub fn download(&self, url: Url, destination: impl AsRef<Path>) -> anyhow::Result<Download> {
    self.download_builder()
        .url(url)
        .destination(destination.as_ref())
        .start()
}
```

The `scheduler_tx` is a channel **sender**. In Rust, sending on a channel requires `&mut self`:

```rust
// tokio's mpsc sender signature
pub async fn send(&mut self, value: T) -> ...
```

This is why methods like `download()`, `cancel()`, and others need to mutate the manager.

### 2. Background Tasks Need Coordination

When you call `download()`, the manager:
1. Creates channels for progress/events
2. Generates a unique ID
3. Spawns the scheduler (if not already running)
4. Sends the request to the scheduler

All of these modify internal state.

### 3. State Changes Over Time

The manager maintains state that changes:
- Active download count (increments/decrements)
- Generated IDs (monotonically increasing)
- Event subscribers (comes and goes)

This is inherently mutable state.

## Why Download Handle Needs Mutability

```rust
pub struct Download {
    id: DownloadID,
    progress: watch::Receiver<Progress>,
    events: broadcast::Receiver<Event>,
    result: oneshot::Receiver<Result<DownloadResult, DownloadError>>,
    cancel_token: CancellationToken,
}
```

### 1. It's a Future

`Download` implements the `Future` trait:

```rust
impl Future for Download {
    type Output = Result<DownloadResult, DownloadError>;
    
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Need &mut self to poll the result receiver
        match Pin::new(&mut self.result).poll(cx) { ... }
    }
}
```

Rust's async system requires futures to be pollable, which needs mutable access.

### 2. Channel Receivers Need Mutability

To receive values from channels:

```rust
// watch channel
progress: watch::Receiver<Progress>,

// Need &mut self to call .recv() or .changed()
```

## Why RequestBuilder is Mutable

The builder pattern requires mutability:

```rust
pub fn retries(mut self, retries: u32) -> Self {
    self.config = self.config.retries(retries);
    self
}
```

Each method call modifies the builder's internal state:
- Sets URL
- Sets destination
- Modifies headers
- Sets retry count

This is the classic builder pattern and is completely normal.

## Once Built, It's Immutable

Here's the key insight: **configuration is immutable, runtime is mutable**.

```rust
// RequestBuilder is mutable during construction
let builder = manager.download_builder();
let download = builder
    .url(url)              // mutate
    .destination(path)     // mutate  
    .retries(5)            // mutate
    .start()               // consume (ownership moves)
    -> Download (immutable reference via Arc);
```

After `.start()`, the request is **wrapped in an `Arc`** and shared:

```rust
// From scheduler.rs
self.schedule(Job {
    request: Arc::new(request),  // shared, read-only
    result: Some(result_tx),
    attempt: 0,
});
```

### Why Arc?

```rust
pub struct Scheduler {
    // The request is shared between:
    // - The scheduler (which created it)
    // - The worker (which executes it)
    request: Arc<Request>,
}
```

Both tasks need to access the same request:
- Scheduler reads it to check config
- Worker reads it to get URL, headers, destination

Using `Arc` (atomic reference counted) allows safe sharing without locking.

## The Design Principle

```
┌─────────────────────────────────────────────────────────────┐
│                    DESIGN PRINCIPLE                         │
├─────────────────────────────────────────────────────────────┤
│  Configuration = Immutable (built once, shared via Arc)   │
│  Runtime State  = Mutable (channels, queues, counters)    │
└─────────────────────────────────────────────────────────────┘
```

| Phase | Mutability | Reason |
|-------|------------|--------|
| Building request | Mutable (`RequestBuilder`) | Accumulate options |
| Built request | Immutable (`Arc<Request>`) | Shared read-only |
| Download handle | Mutable (`Future`) | Poll for completion |
| Manager | Mutable | Channel sends, spawning |

## How Other Languages Handle This

In languages like Python or JavaScript:

```python
# Everything is mutable by default
manager = DownloadManager()
download = manager.download(url, path)  # manager changes internally
```

In Rust, the **compiler enforces** this distinction:
- The type system makes mutability explicit
- The borrow checker prevents data races
- You can't accidentally share mutable state

## Summary

The types are mutable because:

1. **DownloadManager** - Channel senders need `&mut self`
2. **Download** - Futures need `Pin<&mut self>` to poll
3. **RequestBuilder** - Builder pattern requires accumulating state
4. **Request (built)** - Wrapped in `Arc`, shared immutably

This design isn't arbitrary - it's what allows safe concurrent access without locks:
- **Read-only data** is shared via `Arc` (no synchronization needed)
- **Mutable data** is kept within single-threaded contexts (channels handle coordination)

This is Rust's approach to concurrency: make illegal states unrepresentable at compile time.
