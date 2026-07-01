//! Stream the first file of a Workshop item into memory instead of writing to disk.
//!
//! Usage:
//!     cargo run --example stream_to_memory -- <APP_ID> <ITEM_ID>

use std::sync::Arc;

use futures::StreamExt;
use steam_vent_cdn::{CdnDownloader, Config, ProgressCallback, SteamClient};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: cargo run --example stream_to_memory -- <APP_ID> <ITEM_ID>");
        std::process::exit(1);
    }

    let app_id: u32 = args[1].parse()?;
    let item_id: u64 = args[2].parse()?;

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

    let progress = Arc::new(NoopProgress);
    let pipeline = downloader.create_pipeline(progress);

    let first_file = items.files.first().expect("No files in item");
    let mut stream = pipeline.download_file(first_file)?;

    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        buffer.extend_from_slice(&chunk?);
    }

    println!(
        "Downloaded '{}' into memory: {} bytes",
        first_file.filename,
        buffer.len()
    );

    Ok(())
}

struct NoopProgress;

impl ProgressCallback for NoopProgress {
    fn on_download_progress(&self, _bytes: u64) {}
    fn on_decode_progress(&self, _bytes: u64) {}
    fn on_write_progress(&self, _bytes: u64) {}
}
