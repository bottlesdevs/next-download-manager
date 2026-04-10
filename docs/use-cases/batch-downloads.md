# Batch Downloads

This document covers downloading multiple files with concurrency control - a common use case for bulk downloads.

## The Problem

When downloading multiple files:
1. **Don't** start 100 downloads simultaneously (you'll overwhelm the server/your connection)
2. **Do** limit concurrency to a reasonable number
3. **Do** track progress for each file

The download manager handles this automatically via its semaphore.

## Concurrency Control

The key feature is the **semaphore-based concurrency limit**:

```rust
let manager = DownloadManager::with_config(
    DownloadManagerConfig::builder()
        .max_concurrent(3)  // Only 3 downloads at a time
        .build()
);
```

With `max_concurrent = 3`:
- Downloads 1-3 start immediately
- Downloads 4-6 wait in the queue
- When one completes, the next starts

## Simple Batch Download

```rust
use next_download_manager::prelude::*;
use futures::join;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let manager = DownloadManager::default();  // Max 3 concurrent
    
    let files = vec![
        ("https://example.com/1.zip", "/tmp/1.zip"),
        ("https://example.com/2.zip", "/tmp/2.zip"),
        ("https://example.com/3.zip", "/tmp/3.zip"),
        ("https://example.com/4.zip", "/tmp/4.zip"),
        ("https://example.com/5.zip", "/tmp/5.zip"),
    ];
    
    // Start all downloads - they'll be queued automatically
    let downloads: Vec<_> = files
        .iter()
        .map(|(url, dest)| {
            let url = url.parse().unwrap();
            manager.download(url, dest).unwrap()
        })
        .collect();
    
    // Wait for all to complete using futures::join_all
    for download in downloads {
        match download.await {
            Ok(result) => {
                println!("Downloaded: {:?}", result.path);
            }
            Err(e) => {
                println!("Failed: {}", e);
            }
        }
    }
    
    manager.shutdown().await
}
```

## Using Futures for Concurrency

### join_all

Wait for all downloads to complete:

```rust
use futures::future::join_all;

let downloads: Vec<_> = urls.iter()
    .map(|url| manager.download(url.clone(), dest.clone()))
    .collect::<Result<Vec<_>, _>>()?;

let results = join_all(downloads).await;

// Each result is Result<DownloadResult, DownloadError>
for result in results {
    match result {
        Ok(r) => println!("Downloaded: {:?}", r.path),
        Err(e) => eprintln!("Failed: {}", e),
    }
}
```

### for_each_concurrent

Process results as they complete:

```rust
use futures::stream::StreamExt;

let download_stream = futures::stream::iter(downloads)
    .map(|d| async move {
        match d.await {
            Ok(r) => Some(r),
            Err(_) => None,
        }
    });

download_stream
    .for_each_concurrent(3, |result| async move {
        if let Some(r) = result {
            println!("Completed: {:?}", r.path);
        }
    })
    .await;
```

## Monitoring All Downloads

Use the global event stream to track all downloads:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let manager = DownloadManager::default();
    
    // Subscribe to global events
    let mut events = manager.subscribe();
    
    // Start downloads in background
    let downloads = start_batch_downloads(&manager).await?;
    
    // Process events while downloads run
    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(Event::Completed { id, path, bytes_downloaded }) => {
                        println!("[{}] Completed: {} bytes", id, bytes_downloaded);
                    }
                    Ok(Event::Failed { id, error }) => {
                        println!("[{}] Failed: {}", id, error);
                    }
                    Ok(Event::Started { id, total_bytes, .. }) => {
                        println!("[{}] Started: {:?} bytes", id, total_bytes);
                    }
                    _ => {}
                }
            }
            _ = check_all_done(&downloads) => {
                break;
            }
        }
    }
    
    manager.shutdown().await
}
```

## Batch with Progress

Track progress for each download individually:

```rust
use std::collections::HashMap;
use tokio::sync::mpsc;

struct DownloadProgress {
    id: DownloadID,
    bytes: u64,
    total: Option<u64>,
}

async fn download_with_progress(
    manager: &DownloadManager,
    url: Url,
    path: &str,
) -> anyhow::Result<Download> {
    let download = manager.download(url, path)?;
    
    // Spawn task to track progress
    let d = download.clone();
    tokio::spawn(async move {
        let mut progress_stream = d.progress();
        while let Some(p) = progress_stream.next().await {
            // Could send to a channel, log, etc.
            let percent = p.percent().unwrap_or(0.0);
            eprint!("\rDownload: {:.1}%", percent);
        }
    });
    
    Ok(download)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let manager = DownloadManager::default();
    
    let downloads = vec![
        ("https://example.com/1.zip", "/tmp/1.zip"),
        ("https://example.com/2.zip", "/tmp/2.zip"),
        ("https://example.com/3.zip", "/tmp/3.zip"),
    ];
    
    let handles: Vec<_> = downloads
        .iter()
        .map(|(url, dest)| {
            download_with_progress(
                &manager,
                url.parse().unwrap(),
                dest,
            )
        })
        .collect();
    
    // Wait for all
    for handle in handles {
        handle.await??;
    }
    
    manager.shutdown().await
}
```

## Cancellation

Cancel all downloads:

```rust
// Cancel specific download
manager.cancel(download_id).await;

// Cancel all
manager.cancel_all();

// Or from the download handle
download.cancel();
```

When cancelling:
- In-flight downloads stop immediately
- Partial files are deleted
- Queued downloads never start

## Error Recovery

For batch downloads, you might want to:

```rust
// Option 1: Fail fast - stop on first error
for url in urls {
    let download = manager.download(url, path)?;
    download.await?;  // Returns error on first failure
}

// Option 2: Continue on error - track failures
let mut errors = Vec::new();
for (url, path) in urls {
    let download = manager.download(url, path)?;
    match download.await {
        Ok(r) => println!("Downloaded: {:?}", r.path),
        Err(e) => {
            eprintln!("Failed to download {}: {}", url, e);
            errors.push((url, e));
        }
    }
}

// Option 3: Retry with custom logic
for (url, path) in urls {
    let mut retries = 3;
    while retries > 0 {
        let download = manager.download(url, path)?;
        match download.await {
            Ok(r) => break,
            Err(e) if retries > 1 => {
                retries -= 1;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => {
                eprintln!("Failed after retries: {}", e);
                break;
            }
        }
    }
}
```

## Summary

| Scenario | Approach |
|----------|----------|
| Simple batch | `join_all()` on all downloads |
| Process as complete | `for_each_concurrent` |
| Track all events | `manager.subscribe()` |
| Per-download progress | `download.progress()` |
| Cancel all | `manager.cancel_all()` |
| Fail fast | `?` on each await |
| Collect errors | Loop with error collection |

The semaphore automatically handles queuing - just create downloads and let the manager coordinate!
