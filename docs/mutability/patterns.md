# Patterns & Best Practices

This document covers patterns for working with mutable state in the download manager, including common patterns and pitfalls to avoid.

## Common Patterns

### Single Manager Instance

Create one `DownloadManager` and reuse it:

```rust
// Good: Single instance
let manager = DownloadManager::default();

// Use for many downloads
let d1 = manager.download(url1, path1)?;
let d2 = manager.download(url2, path2)?;
let d3 = manager.download(url3, path3)?;

// Bad: Creating new managers
let m1 = DownloadManager::default();
let m2 = DownloadManager::default();  // Separate concurrency limits!
```

Why? The manager enforces a **global concurrency limit**. Multiple managers = multiple limits.

### Sharing the Manager

The manager is typically shared behind an `Arc`:

```rust
use std::sync::Arc;

// Create once
let manager = Arc::new(DownloadManager::default());

// Clone for multiple consumers
let m1 = manager.clone();
let m2 = manager.clone();

// Use in different tasks
let handle = tokio::spawn(async move {
    m1.download(url, path).await
});
```

Since `DownloadManager` is mutable, you need `Arc` for shared ownership:

```rust
// Without Arc, you'd need &mut manager everywhere
fn download(manager: &mut DownloadManager, url: Url, path: &Path) -> Result<Download>;

// With Arc, you get shared access
fn download(manager: Arc<DownloadManager>, url: Url, path: &Path) -> Result<Download>;
```

### Awaiting Downloads

The `Download` handle is a `Future`, so you can await it directly:

```rust
// Simple await
let result = download.await?;

// With timeout using tokio::select
tokio::select! {
    result = download => {
        // Handle result
    }
    _ = tokio::time::sleep(Duration::from_secs(60)) => {
        // Timeout - cancel and handle
        download.cancel();
    }
}
```

### Progress + Events Combo

Monitor both progress and events:

```rust
let download = manager.download(url, path)?;

let progress = download.progress();
let events = download.events();

tokio::pin!(progress);
tokio::pin!(events);

loop {
    tokio::select! {
        Some(p) = progress.next() => {
            println!("Progress: {}%", p.percent().unwrap_or(0.0));
        }
        Some(e) = events.next() => {
            println!("Event: {}", e);
        }
        result = download => {
            break;  // Download finished
        }
    }
}
```

## Pitfalls to Avoid

### 1. Dropping the Manager Too Early

```rust
// Bad: Manager dropped before download completes
fn download_file(url: Url, path: &Path) -> Download {
    let manager = DownloadManager::default();  // Dropped at end of function!
    manager.download(url, path).unwrap()
}

// The download handle needs the manager to be alive!
let download = download_file(url, path);
let result = download.await;  // Manager is already dropped!
```

**Solution**: Keep the manager alive as long as downloads are running:

```rust
// Good: Manager lives as long as needed
async fn download_file(manager: &DownloadManager, url: Url, path: &Path) -> Result<DownloadResult> {
    let download = manager.download(url, path)?;
    download.await.map_err(|e| e.into())
}

#[tokio::main]
async fn main() {
    let manager = DownloadManager::default();
    
    // Manager stays alive throughout
    let result = download_file(&manager, url, path).await;
    
    manager.shutdown().await;
}
```

### 2. Not Handling Shutdown

```rust
// Bad: Manager dropped without shutdown
fn main() {
    let manager = DownloadManager::default();
    // ... downloads running ...
    // Function ends, manager dropped, workers orphaned!
}
```

**Solution**: Always call `shutdown()`:

```rust
// Good: Proper cleanup
#[tokio::main]
async fn main() {
    let manager = DownloadManager::default();
    
    // Do downloads
    let downloads = /* ... */;
    
    // Wait for all to complete
    for d in downloads {
        let _ = d.await;
    }
    
    // Clean shutdown
    manager.shutdown().await;
}
```

### 3. Ignoring Errors on Channels

```rust
// Warning: Channel operations can fail
let download = manager.download_builder()
    .url(url)
    .destination(path)
    .start()?;  // This can fail if scheduler channel is full

download.cancel()?;  // Can fail if channel is closed
```

**Solution**: Handle the `anyhow::Result`:

```rust
match manager.try_cancel(id) {
    Ok(()) => println!("Cancel sent"),
    Err(e) => eprintln!("Failed to cancel: {}", e),
}
```

### 4. Creating Too Many Managers

```rust
// Bad: Each has its own concurrency limit
for _ in 0..10 {
    let m = DownloadManager::default();  // Each limits to 3!
    // Downloads here are limited to 3 per manager, not 3 total
}
```

**Solution**: One manager, multiple downloads:

```rust
// Good: Single manager, shared limit
let manager = DownloadManager::default();

for _ in 0..10 {
    let d = manager.download(url, path)?;  // All share the limit
}
```

### 5. Not Consuming the Download

```rust
// Warning: Download starts but you don't await it
manager.download(url, path)?;

// Download is running in background, but you lost the handle!
// Can't cancel it, can't check progress, can't get result!
```

**Solution**: Always keep the handle:

```rust
// Good: Keep the handle
let download = manager.download(url, path)?;

// Either await it
let result = download.await;

// Or spawn it and track the handle
tokio::spawn(async move {
    let result = download.await;
    // Handle result
});
```

## Working With Async

### Spawning Downloads

```rust
use tokio::spawn;
use futures::join;

// Spawn multiple downloads concurrently
let d1 = manager.download(url1, path1)?;
let d2 = manager.download(url2, path2)?;
let d3 = manager.download(url3, path3)?;

let (r1, r2, r3) = join!(d1, d2, d3);

// All three run concurrently (up to semaphore limit)
```

### Cancellation Patterns

```rust
use tokio::select;
use tokio::time::timeout;

// With timeout
let result = timeout(Duration::from_secs(60), download).await;

match result {
    Ok(Ok(r)) => println!("Downloaded: {:?}", r.path),
    Ok(Err(e)) => println!("Failed: {}", e),
    Err(_) => {
        println!("Timed out, cancelling");
        download.cancel();
    }
}

// With user cancellation
let cancel_flag = Arc::new(AtomicBool::new(false));
let cancel_clone = cancel_flag.clone();

let d = manager.download(url, path)?;

tokio::select! {
    result = d => {
        // Download finished
    }
    _ = async {
        // Wait for user to signal cancellation
        while !cancel_clone.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    } => {
        d.cancel();
    }
}
```

### Error Handling

```rust
match download.await {
    Ok(result) => {
        println!("Downloaded {} bytes to {:?}", 
            result.bytes_downloaded, result.path);
    }
    Err(DownloadError::Cancelled) => {
        println!("Download was cancelled");
    }
    Err(DownloadError::Network(e)) => {
        println!("Network error: {}", e);
    }
    Err(DownloadError::Io(e)) => {
        println!("File error: {}", e);
    }
    Err(e) => {
        println!("Other error: {}", e);
    }
}
```

## Summary

| Pattern | Do | Don't |
|---------|-----|-------|
| Manager instance | One per application | Multiple managers |
| Manager lifetime | Keep alive until shutdown | Drop before downloads finish |
| Sharing | Use `Arc<&mut DownloadManager>` or `Arc<DownloadManager>` | Copy without Arc |
| Download handle | Always keep/retain | Drop without awaiting |
| Shutdown | Always call `shutdown().await` | Let it drop |
| Errors | Handle `anyhow::Result` | Ignore channel errors |

The key insight: **the manager orchestrates everything**, so keep it alive and use it properly!
