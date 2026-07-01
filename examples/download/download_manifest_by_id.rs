//! Download files directly from a known manifest ID and app ID.
//!
//! This bypasses the Workshop item details lookup.
//!
//! Usage:
//!     cargo run --example download_manifest_by_id -- <APP_ID> <MANIFEST_ID>

use std::path::PathBuf;
use std::sync::Arc;

use steam_vent_cdn::{CdnDownloader, Config, SteamClient};

#[path = "../_common.rs"]
mod common;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!(
            "Usage: cargo run --example download_manifest_by_id -- <APP_ID> <MANIFEST_ID>"
        );
        std::process::exit(1);
    }

    let app_id: u32 = args[1].parse()?;
    let manifest_id: u64 = args[2].parse()?;
    let output_dir = PathBuf::from("./output");

    tracing_subscriber::fmt::init();

    let config = Config::builder().app_id(app_id).build();
    let client = SteamClient::new(config);
    let session = client.connect().await?;
    let mut downloader = CdnDownloader::new(session)?;

    let items = downloader.fetch_manifest_by_id(manifest_id, app_id).await?;

    println!(
        "Found {} directories and {} files",
        items.directories.len(),
        items.files.len()
    );

    for dir in &items.directories {
        tokio::fs::create_dir_all(output_dir.join(&dir.filename)).await?;
    }

    let progress = Arc::new(common::NoopProgress);
    let pipeline = downloader.create_pipeline(progress);

    let mut tasks = tokio::task::JoinSet::new();
    for file in &items.files {
        let pipeline = pipeline.clone();
        let base = output_dir.clone();
        let file = file.clone();
        tasks.spawn(async move {
            let mut stream = pipeline.download_file(&file)?;
            common::write_stream_to_file(&mut stream, &base).await
        });
    }

    while let Some(result) = tasks.join_next().await {
        result??;
    }

    Ok(())
}
