//! Read a specific byte range from a file using CdnRandomAccessReader.
//!
//! Usage:
//!     cargo run --example random_access_reader -- <APP_ID> <ITEM_ID> <FILENAME> <OFFSET> <LENGTH>

use std::io::SeekFrom;
use std::sync::Arc;

use steam_vent_cdn::{CdnDownloader, Config, ProgressCallback, SteamClient};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 6 {
        eprintln!(
            "Usage: cargo run --example random_access_reader -- <APP_ID> <ITEM_ID> <FILENAME> <OFFSET> <LENGTH>"
        );
        std::process::exit(1);
    }

    let app_id: u32 = args[1].parse()?;
    let item_id: u64 = args[2].parse()?;
    let filename = args[3].clone();
    let offset: u64 = args[4].parse()?;
    let length: usize = args[5].parse()?;

    tracing_subscriber::fmt::init();

    let config = Config::builder().app_id(app_id).build();
    let client = SteamClient::new(config);
    let session = client.connect().await?;
    let mut downloader = CdnDownloader::new(session)?;

    let versions = downloader.fetch_all_change_history(item_id).await?;
    let latest = versions.first().expect("No versions found");

    let items = downloader
        .fetch_manifest_info(item_id, latest.manifest_id)
        .await?;

    let file = items
        .files
        .into_iter()
        .find(|f| f.filename == filename)
        .ok_or_else(|| anyhow::anyhow!("File '{}' not found in manifest", filename))?;

    let progress = Arc::new(NoopProgress);
    let mut reader = downloader.create_reader(&file, progress);

    reader.seek(SeekFrom::Start(offset)).await?;
    let mut buffer = vec![0u8; length];
    let bytes_read = reader.read(&mut buffer).await?;
    buffer.truncate(bytes_read);

    println!(
        "Read {} bytes from {} at offset {}",
        buffer.len(),
        filename,
        offset
    );
    println!(
        "Preview: {:?}",
        String::from_utf8_lossy(&buffer[..buffer.len().min(256)])
    );

    Ok(())
}

struct NoopProgress;

impl ProgressCallback for NoopProgress {
    fn on_download_progress(&self, _bytes: u64) {}
    fn on_decode_progress(&self, _bytes: u64) {}
    fn on_write_progress(&self, _bytes: u64) {}
}
