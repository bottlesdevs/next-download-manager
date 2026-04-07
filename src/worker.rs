use std::sync::Arc;

use futures_util::StreamExt as _;
use tokio::{fs::File, io::AsyncWriteExt, sync::mpsc};
use tracing::{debug, error, info, instrument, warn};

use crate::{
    backend::BackendError,
    context::{Context, DownloadID},
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

#[instrument(level = "info", skip(request, ctx, worker_tx), fields(id = %request.id(), url = %request.url()))]
pub(crate) async fn run(
    request: Arc<Request>,
    ctx: Arc<Context>,
    worker_tx: mpsc::Sender<WorkerMsg>,
) {
    let result = attempt_download(request.as_ref(), &ctx).await;

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

#[instrument(
    level = "info",
    skip(request, ctx),
    fields(id = %request.id(), url = %request.url(), destination = ?request.destination())
)]
pub(crate) async fn attempt_download(
    request: &Request,
    ctx: &Context,
) -> Result<DownloadResult, DownloadError> {
    // The backend handles cancellation internally: if the token fires before
    // the server responds it returns None, and no Probed event is emitted.
    if let Some(info) = ctx
        .backend
        .probe_head(
            request.url(),
            request.config().headers(),
            &request.cancel_token,
        )
        .await
    {
        request.emit(Event::Probed {
            id: request.id(),
            info,
        });
    }

    if let Some(parent) = request.destination().parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if request.destination().exists() && !request.config().overwrite() {
        warn!(
            destination = ?request.destination(),
            "Destination exists and overwrite=false; failing"
        );
        return Err(DownloadError::FileExists {
            path: request.destination().to_path_buf(),
        });
    }

    //
    // If the cancellation token fires before response headers are received the
    // backend returns Err(BackendError::Cancelled), which we surface as
    // DownloadError::Cancelled rather than DownloadError::Network so the
    // scheduler does not retry it.
    let backend_resp = ctx
        .backend
        .fetch(
            request.url(),
            request.config().headers(),
            &request.cancel_token,
        )
        .await
        .map_err(|e| match e {
            BackendError::Cancelled => DownloadError::Cancelled,
            other => DownloadError::Network(other),
        })?;

    let total_bytes = backend_resp.content_length;
    debug!(total_bytes = ?total_bytes, "Server accepted GET request");

    let mut stream = backend_resp.stream;
    let mut file = File::create(request.destination()).await?;

    request.emit(Event::Started {
        id: request.id(),
        url: request.url().clone(),
        destination: request.destination().to_path_buf(),
        total_bytes,
    });

    let mut progress = Progress::new(total_bytes);

    loop {
        tokio::select! {
            _ = request.cancel_token.cancelled() => {
                warn!(
                    destination = ?request.destination(),
                    "Cancellation received during streaming; removing partial file"
                );
                drop(file);
                tokio::fs::remove_file(request.destination()).await?;
                return Err(DownloadError::Cancelled);
            }
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        file.write_all(&bytes).await?;
                        if progress.update(bytes.len() as u64) {
                            request.update_progress(progress);
                        }
                    }
                    Some(Err(e)) => {
                        error!(
                            error = %e,
                            destination = ?request.destination(),
                            "Error reading response chunk; removing partial file"
                        );
                        drop(file);
                        tokio::fs::remove_file(request.destination()).await?;
                        return Err(DownloadError::Network(e));
                    }
                    None => break,
                }
            }
        }
    }

    progress.force_update();
    let _ = request.update_progress(progress);
    file.sync_all().await?;

    info!(
        destination = ?request.destination(),
        bytes = progress.bytes_downloaded(),
        "Download completed successfully"
    );

    Ok(DownloadResult {
        path: request.destination().to_path_buf(),
        bytes_downloaded: progress.bytes_downloaded(),
    })
}
