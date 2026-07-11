//! Download a specific version of a Steam Workshop item by timestamp.
//!
//! Usage:
//!     cargo run --example download_specific_version -- <APP_ID> <ITEM_ID> <TIMESTAMP>

use std::path::PathBuf;
use std::sync::Arc;

use futures::stream::TryStreamExt;
use steam_vent_cdn::config::Config;
use steam_vent_cdn::connection::SteamClient;
use steam_vent_cdn::downloader::client::CdnDownloader;
use steam_vent_cdn::error::WorkshopError;

#[path = "../_common.rs"]
mod common;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!(
            "Usage: cargo run --example download_specific_version -- <APP_ID> <ITEM_ID> <TIMESTAMP>"
        );
        std::process::exit(1);
    }

    let app_id: u32 = args[1].parse()?;
    let item_id: u64 = args[2].parse()?;
    let timestamp: u32 = args[3].parse()?;
    let output_dir = PathBuf::from("./output").join(timestamp.to_string());

    tracing_subscriber::fmt::init();

    let config = Config::builder().app_id(app_id).build();
    let client = SteamClient::new(config);
    let session = client.connect().await?;
    let downloader = CdnDownloader::connect(session).await?;

    let mut stream = Box::pin(downloader.fetch_versions(item_id, None));
    let mut version = None;
    while let Some(v) = stream.try_next().await? {
        if v.timestamp == timestamp {
            version = Some(v);
            break;
        }
    }
    let version = version.ok_or(WorkshopError::VersionNotFound(format!(
        "Timestamp {}",
        timestamp
    )))?;

    tracing::info!("Downloading version: {}", version.timestamp);

    let manifest = downloader
        .fetch_manifest_info(item_id, version.manifest_id)
        .await?;

    for dir in &manifest.directories {
        tokio::fs::create_dir_all(output_dir.join(&dir.filename)).await?;
    }

    let progress = Arc::new(common::NoopProgress);
    let pipeline = downloader.create_pipeline(progress);

    let mut tasks = tokio::task::JoinSet::new();
    for file in &manifest.files {
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
