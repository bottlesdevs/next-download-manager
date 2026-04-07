use bottles_download_manager::{DownloadManager, prelude::*};
use futures_util::{StreamExt, future::join_all};
use reqwest::Url;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

fn init_tracing() {
    if std::env::var_os("RUST_LOG").is_none() {
        unsafe {
            std::env::set_var("RUST_LOG", "info,bottles_download_manager=debug");
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let manager = DownloadManager::default();

    let items: Vec<(Url, Option<&str>)> = vec![
        (Url::parse("https://ash-speed.hetzner.com/100MB.bin")?, None),
        (
            Url::parse("https://ash-speed.hetzner.com/100MB.bin")?,
            Some("100MB-hel.bin"),
        ),
    ];

    let (handles, errors): (Vec<_>, Vec<_>) = manager
        .enqueue_many(items)
        .into_iter()
        .enumerate()
        .partition(|(_, r)| r.is_ok());

    for (i, err) in errors {
        warn!(index = i, error = %err.err().expect("partitioned as error"), "Failed to enqueue item");
    }

    let handles: Vec<Download> = handles
        .into_iter()
        .map(|(_, r)| r.expect("partitioned above"))
        .collect();

    if handles.is_empty() {
        error!("No downloads could be enqueued; exiting.");
        return Ok(());
    }

    info!(count = handles.len(), "Downloads enqueued");

    let mut global_events = manager.events();
    tokio::spawn(async move {
        while let Some(ev) = global_events.next().await {
            info!(event = %ev, "[global]");
        }
    });

    for dl in &handles {
        let id = dl.id();
        let mut progress = dl.progress();
        tokio::spawn(async move {
            while let Some(p) = progress.next().await {
                let pct = p
                    .percent()
                    .map(|v| format!("{v:.1}%"))
                    .unwrap_or_else(|| "?%".into());

                let eta = p
                    .eta()
                    .map(|d| format!("{:.0}s", d.as_secs_f64()))
                    .unwrap_or_else(|| "?".into());

                info!(
                    id,
                    bytes     = p.bytes_downloaded,
                    total     = p.total_bytes.map(|v| v as i64).unwrap_or(-1),
                    ema_bps   = p.ema_bps as u64,
                    percent   = %pct,
                    eta       = %eta,
                    "[progress]"
                );
            }
        });
    }

    let results = join_all(handles).await;

    let mut succeeded = 0usize;
    let mut failed = 0usize;

    for result in results {
        match result {
            Ok(r) => {
                succeeded += 1;
                info!(
                    path  = %r.path.display(),
                    bytes = r.bytes_downloaded,
                    "✓ completed"
                );
            }
            Err(e) => {
                failed += 1;
                error!(error = %e, "✗ failed");
            }
        }
    }

    info!(succeeded, failed, "batch finished");

    manager.shutdown().await;
    Ok(())
}
