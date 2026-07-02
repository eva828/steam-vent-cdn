#![allow(dead_code)]

use std::path::Path;

use futures::StreamExt;
use steam_vent_cdn::downloader::progress::ProgressCallback;
use steam_vent_cdn::downloader::sequential::FileByteStream;
use steam_vent_cdn::error::Result;
use tokio::fs;
use tokio::io::AsyncWriteExt;

pub struct NoopProgress;

impl ProgressCallback for NoopProgress {
    fn on_download_progress(&self, _bytes: u64) {}
    fn on_decode_progress(&self, _bytes: u64) {}
    fn on_write_progress(&self, _bytes: u64) {}
}

pub async fn write_stream_to_file(stream: &mut FileByteStream, base: &Path) -> Result<()> {
    let path = base.join(&stream.filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let mut out = fs::File::create(&path).await?;
    while let Some(chunk) = stream.next().await {
        out.write_all(&chunk?).await?;
    }
    out.flush().await?;

    tracing::info!("Wrote {}", path.display());
    Ok(())
}
