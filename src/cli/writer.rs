use std::path::Path;

use futures::StreamExt;
use tokio::io::AsyncWriteExt;

use steam_vent_cdn::downloader::sequential::FileByteStream;
use steam_vent_cdn::error::Result;

pub async fn write_file(stream: &mut FileByteStream, base_path: &Path) -> Result<()> {
    let file_path = base_path.join(&stream.filename);
    if let Some(p) = file_path.parent() {
        tokio::fs::create_dir_all(p).await?;
    }

    let mut file = tokio::fs::File::create(&file_path).await?;
    let mut bytes_written: u64 = 0;

    while let Some(chunk_result) = stream.next().await {
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
    Ok(())
}
