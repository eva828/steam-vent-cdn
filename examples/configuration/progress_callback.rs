//! Demonstrate a custom ProgressCallback implementation.
//!
//! Usage:
//!     cargo run --example progress_callback -- <APP_ID> <ITEM_ID>

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::stream::TryStreamExt;
use steam_vent_cdn::config::Config;
use steam_vent_cdn::connection::SteamClient;
use steam_vent_cdn::downloader::client::CdnDownloader;
use steam_vent_cdn::downloader::progress::ProgressCallback;

#[path = "../_common.rs"]
mod common;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: cargo run --example progress_callback -- <APP_ID> <ITEM_ID>");
        std::process::exit(1);
    }

    let app_id: u32 = args[1].parse()?;
    let item_id: u64 = args[2].parse()?;
    let output_dir = PathBuf::from("./output");

    tracing_subscriber::fmt::init();

    let config = Config::builder().app_id(app_id).build();
    let client = SteamClient::new(config);
    let session = client.connect().await?;
    let downloader = CdnDownloader::connect(session).await?;

    let versions: Vec<_> = downloader.fetch_versions(item_id, None).try_collect().await?;
    let latest = versions.first().expect("No versions found");

    let manifest = downloader
        .fetch_manifest_info(item_id, latest.manifest_id)
        .await?;

    for dir in &manifest.directories {
        tokio::fs::create_dir_all(output_dir.join(&dir.filename)).await?;
    }

    let progress = Arc::new(SimpleProgress::default());
    let pipeline = downloader.create_pipeline(progress.clone());

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

    progress.print_summary();

    Ok(())
}

#[derive(Default)]
struct SimpleProgress {
    downloaded: AtomicU64,
    decoded: AtomicU64,
    written: AtomicU64,
}

impl SimpleProgress {
    fn print_summary(&self) {
        println!(
            "Downloaded: {} bytes, Decoded: {} bytes, Written: {} bytes",
            self.downloaded.load(Ordering::Relaxed),
            self.decoded.load(Ordering::Relaxed),
            self.written.load(Ordering::Relaxed),
        );
    }
}

impl ProgressCallback for SimpleProgress {
    fn on_download_progress(&self, bytes: u64) {
        self.downloaded.fetch_add(bytes, Ordering::Relaxed);
    }

    fn on_decode_progress(&self, bytes: u64) {
        self.decoded.fetch_add(bytes, Ordering::Relaxed);
    }

    fn on_write_progress(&self, bytes: u64) {
        self.written.fetch_add(bytes, Ordering::Relaxed);
    }
}
