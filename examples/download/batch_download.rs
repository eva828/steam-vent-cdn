//! Download multiple Steam Workshop items in one run.
//!
//! Usage:
//!     cargo run --example batch_download -- <APP_ID_1> <ITEM_ID_1> [<APP_ID_2> <ITEM_ID_2> ...]

use std::path::PathBuf;
use std::sync::Arc;

use futures::stream::TryStreamExt;
use steam_vent_cdn::config::Config;
use steam_vent_cdn::connection::SteamClient;
use steam_vent_cdn::downloader::client::CdnDownloader;

#[path = "../_common.rs"]
mod common;

#[derive(Clone)]
struct Target {
    app_id: u32,
    item_id: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() % 2 != 1 {
        eprintln!(
            "Usage: cargo run --example batch_download -- <APP_ID_1> <ITEM_ID_1> [<APP_ID_2> <ITEM_ID_2> ...]"
        );
        std::process::exit(1);
    }

    let mut targets = Vec::new();
    for i in (1..args.len()).step_by(2) {
        targets.push(Target {
            app_id: args[i].parse()?,
            item_id: args[i + 1].parse()?,
        });
    }

    let output_dir = PathBuf::from("./output");

    tracing_subscriber::fmt::init();

    let config = Config::builder().build();
    let client = SteamClient::new(config);
    let session = client.connect().await?;
    let downloader = CdnDownloader::connect(session).await?;

    for target in targets {
        println!(
            "Downloading app_id={} item_id={}",
            target.app_id, target.item_id
        );

        let versions: Vec<_> = downloader
            .fetch_versions(target.item_id, None)
            .try_collect()
            .await?;
        let latest = versions.first().expect("No versions found");

        let manifest = downloader
            .fetch_manifest_info(target.item_id, latest.manifest_id)
            .await?;

        let base = output_dir
            .join(target.app_id.to_string())
            .join(target.item_id.to_string())
            .join(latest.timestamp.to_string());

        for dir in &manifest.directories {
            tokio::fs::create_dir_all(base.join(&dir.filename)).await?;
        }

        let progress = Arc::new(common::NoopProgress);
        let pipeline = downloader.create_pipeline(progress);

        let mut tasks = tokio::task::JoinSet::new();
        for file in &manifest.files {
            let pipeline = pipeline.clone();
            let file_base = base.clone();
            let file = file.clone();
            tasks.spawn(async move {
                let mut stream = pipeline.download_file(&file)?;
                common::write_stream_to_file(&mut stream, &file_base).await
            });
        }

        while let Some(result) = tasks.join_next().await {
            result??;
        }
    }

    Ok(())
}
