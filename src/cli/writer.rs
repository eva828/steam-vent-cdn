use std::path::Path;

use futures::StreamExt;
use tokio::io::AsyncWriteExt;

use steam_vent_cdn::{DownloadStream, Result, SteamVentError};

/// Consume a [`DownloadStream`] and write every file to the filesystem under
/// `base_path`.  Each [`FileEntry`](steam_vent_cdn::FileEntry) is processed
/// concurrently so that interleaved CDN chunks don't cause deadlocks.
pub async fn write_stream_to_directory(
    stream: &mut DownloadStream,
    base_path: &Path,
) -> Result<()> {
    let base_path = base_path.to_path_buf();
    let mut tasks = tokio::task::JoinSet::new();

    while let Some(result) = stream.next().await {
        let mut entry = result?;
        let base = base_path.clone();

        tasks.spawn(async move {
            let file_path = base.join(&entry.filename);
            if let Some(p) = file_path.parent() {
                tokio::fs::create_dir_all(p).await?;
            }

            let mut file = tokio::fs::File::create(&file_path).await?;
            let mut bytes_written: u64 = 0;

            while let Some(chunk_result) = entry.next().await {
                let data = chunk_result?;
                bytes_written += data.len() as u64;
                file.write_all(&data).await?;
            }

            file.flush().await?;
            tracing::info!(
                "Completed file: {} ({} bytes)",
                file_path.display(),
                bytes_written
            );
            Ok::<_, SteamVentError>(())
        });
    }

    // Wait for all file-write tasks to finish.
    while let Some(result) = tasks.join_next().await {
        result
            .map_err(|e| {
                SteamVentError::Io(std::io::Error::other(format!(
                    "File write task panicked: {}",
                    e
                )))
            })?
            ?;
    }

    Ok(())
}
