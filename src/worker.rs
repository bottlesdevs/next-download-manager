use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use reqwest::{Client, Method};
use tokio::{fs::File, io::AsyncWriteExt, sync::mpsc};
use tracing::{debug, error, info, instrument, trace, warn};

use crate::{
    context::{Context, DownloadID},
    download::RemoteInfo,
    error::DownloadError,
    events::{Event, Progress},
    prelude::DownloadResult,
    request::Request,
};

pub(crate) enum WorkerMsg {
    Finish {
        id: DownloadID,
        result: Result<DownloadResult, DownloadError>,
    },
}

/// Returns a temporary file path in the same directory as `destination` so that
/// the final `rename` is guaranteed to be on the same filesystem (and therefore
/// atomic on POSIX systems).
///
/// Format: `<parent>/<filename>.<id>.part`
///
/// Using the download ID as part of the name avoids collisions when two
/// concurrent downloads target the same final path (an error handled elsewhere,
/// but defensive naming is still useful).
fn temp_path_for(destination: &Path, id: DownloadID) -> PathBuf {
    let file_name = destination
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("download"));

    let mut temp_name = file_name.to_os_string();
    temp_name.push(format!(".{id}.part"));

    destination
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(temp_name)
}

/// Remove a file at `path`, logging a warning on failure instead of propagating.
///
/// Used for best-effort cleanup in error/cancellation paths where there is
/// nothing sensible we can do if the removal itself fails.
async fn try_remove(path: &Path) {
    if let Err(e) = tokio::fs::remove_file(path).await {
        // Only warn if the file still exists; a missing file is fine.
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(path = ?path, error = %e, "Failed to remove temp file during cleanup");
        }
    }
}

#[instrument(level = "info", skip(request, ctx, worker_tx), fields(id = %request.id(), url = %request.url()))]
pub(crate) async fn run(
    request: Arc<Request>,
    ctx: Arc<Context>,
    worker_tx: mpsc::Sender<WorkerMsg>,
) {
    let result = attempt_download(request.as_ref(), ctx.client.clone()).await;
    if result.is_ok() {
        info!(id = %request.id(), "Download attempt finished successfully");
    } else {
        warn!(id = %request.id(), "Download attempt finished with error");
    }

    let _ = worker_tx
        .send(WorkerMsg::Finish {
            id: request.id(),
            result,
        })
        .await;
}

#[instrument(level = "debug", skip(request, client), fields(id = %request.id(), url = %request.url()))]
pub(crate) async fn probe_head(request: &Request, client: &Client) -> Option<RemoteInfo> {
    use reqwest::header;
    debug!("Probing remote with HTTP HEAD");
    let req = client
        .request(Method::HEAD, request.url().as_ref())
        .headers(request.config().headers().clone())
        .send();

    let resp = tokio::select! {
        resp = req => resp.ok()?.error_for_status().ok()?,
        _ = request.cancel_token.cancelled() => return None,
    };

    let headers = resp.headers();
    let content_length = resp.content_length();
    trace!(content_length = ?content_length, "Got HEAD response");

    let accept_ranges = headers
        .get(header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let etag = headers
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let last_modified = headers
        .get(header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    Some(RemoteInfo {
        content_length,
        accept_ranges,
        etag,
        last_modified,
        content_type,
    })
}

#[instrument(level = "info", skip(request, client), fields(id = %request.id(), url = %request.url(), destination = ?request.destination()))]
pub(crate) async fn attempt_download(
    request: &Request,
    client: Client,
) -> Result<DownloadResult, DownloadError> {
    if let Some(info) = probe_head(request, &client).await {
        request.emit(Event::Probed {
            id: request.id(),
            info,
        });
    }

    let destination = request.destination();

    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Check the *final* destination before we touch the filesystem so that
    // the `overwrite=false` guard is evaluated against the committed file, not
    // any stale `.part` file left from a previous interrupted attempt.
    if destination.exists() && !request.config().overwrite() {
        warn!(destination = ?destination, "Destination exists and overwrite=false; failing");
        return Err(DownloadError::FileExists {
            path: destination.to_path_buf(),
        });
    }

    let req = client
        .request(Method::GET, request.url().as_ref())
        .headers(request.config().headers().clone())
        .send();

    let mut response = tokio::select! {
        resp = req => Ok(resp?.error_for_status()?),
        _ = request.cancel_token.cancelled() => Err(DownloadError::Cancelled),
    }?;

    let total_bytes = response.content_length();
    debug!(total_bytes = ?total_bytes, "Server accepted download");

    // Write to a sibling `.part` file so that:
    //   1. The destination path never contains partial data while the download
    //      is in progress.
    //   2. The final rename(2) / MoveFileEx is atomic on POSIX (same
    //      filesystem) and near-atomic on Windows.
    let temp = temp_path_for(destination, request.id());
    debug!(temp = ?temp, destination = ?destination, "Writing to temp file");

    let mut file = File::create(&temp).await?;

    request.emit(Event::Started {
        id: request.id(),
        url: request.url().clone(),
        destination: destination.to_path_buf(),
        total_bytes,
    });

    let mut progress = Progress::new(total_bytes);

    let stream_result = stream_to_file(request, &mut response, &mut file, &mut progress).await;

    match stream_result {
        // Chunk-level error: discard the temp file and propagate.
        Err(e) => {
            error!(
                error = %e,
                temp = ?temp,
                "Error while streaming response; removing temp file"
            );
            drop(file);
            try_remove(&temp).await;
            return Err(e);
        }

        // Cancelled during streaming: discard the temp file and propagate.
        Ok(StreamOutcome::Cancelled) => {
            warn!(temp = ?temp, "Cancellation received; removing temp file");
            drop(file);
            try_remove(&temp).await;
            return Err(DownloadError::Cancelled);
        }

        Ok(StreamOutcome::Complete) => {}
    }

    progress.force_update();
    request.update_progress(progress);

    file.sync_all().await?;
    drop(file);

    // Atomic rename: replaces the destination if overwrite=true (already
    // checked above; if another writer raced us the last writer wins).
    debug!(temp = ?temp, destination = ?destination, "Renaming temp file to destination");
    tokio::fs::rename(&temp, destination).await.map_err(|e| {
        error!(error = %e, temp = ?temp, destination = ?destination, "Failed to rename temp file");
        e
    })?;

    info!(
        destination = ?destination,
        bytes = progress.bytes_downloaded(),
        "Download committed atomically"
    );

    Ok(DownloadResult {
        path: destination.to_path_buf(),
        bytes_downloaded: progress.bytes_downloaded(),
    })
}

enum StreamOutcome {
    Complete,
    Cancelled,
}

/// Drive the response body into `file`, updating `progress` on each chunk.
///
/// Returns:
/// - `Ok(StreamOutcome::Complete)` when all bytes have been written.
/// - `Ok(StreamOutcome::Cancelled)` when the cancel token fires mid-stream.
/// - `Err(DownloadError)` on an unrecoverable network or I/O error.
///
/// The caller is responsible for closing and removing the temp file on any
/// non-`Complete` outcome.
#[instrument(level = "debug", skip_all, fields(id = %request.id()))]
async fn stream_to_file(
    request: &Request,
    response: &mut reqwest::Response,
    file: &mut File,
    progress: &mut Progress,
) -> Result<StreamOutcome, DownloadError> {
    loop {
        tokio::select! {
            _ = request.cancel_token.cancelled() => {
                return Ok(StreamOutcome::Cancelled);
            }
            chunk = response.chunk() => {
                match chunk {
                    Ok(Some(chunk)) => {
                        file.write_all(&chunk).await?;
                        if progress.update(chunk.len() as u64) {
                            request.update_progress(*progress);
                        }
                    }
                    Ok(None) => return Ok(StreamOutcome::Complete),
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
}
