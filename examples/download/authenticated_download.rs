//! Download a Steam Workshop item with username/password authentication.
//!
//! This example is skipped unless STEAM_USERNAME and STEAM_PASSWORD are set.
//!
//! Usage:
//!     STEAM_USERNAME=<user> STEAM_PASSWORD=<pass> \
//!         cargo run --example authenticated_download -- <APP_ID> <ITEM_ID>

use std::path::PathBuf;

use steam_vent_cdn::{CdnDownloader, Config, Credentials, SteamClient, classify_items};

#[path = "../_common.rs"]
mod common;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!(
            "Usage: STEAM_USERNAME=<user> STEAM_PASSWORD=<pass> cargo run --example authenticated_download -- <APP_ID> <ITEM_ID>"
        );
        std::process::exit(1);
    }

    let username = match std::env::var("STEAM_USERNAME") {
        Ok(u) => u,
        Err(_) => {
            println!("Skipping authenticated_download example: STEAM_USERNAME not set.");
            return Ok(());
        }
    };
    let password = match std::env::var("STEAM_PASSWORD") {
        Ok(p) => p,
        Err(_) => {
            println!("Skipping authenticated_download example: STEAM_PASSWORD not set.");
            return Ok(());
        }
    };

    let app_id: u32 = args[1].parse()?;
    let item_id: u64 = args[2].parse()?;
    let output_dir = PathBuf::from("./output");

    tracing_subscriber::fmt::init();

    let config = Config::builder()
        .credentials(Credentials::UserPass { username, password })
        .app_id(app_id)
        .build();
    let client = SteamClient::new(config);
    let session = client.connect().await?;
    let mut downloader = CdnDownloader::new(session)?;

    let versions = downloader.fetch_all_change_history(item_id).await?;
    let latest = versions.first().expect("No versions found");

    let items = downloader
        .fetch_manifest_info(item_id, latest.manifest_id)
        .await?;
    let (directories, files) = classify_items(items);

    for dir in &directories {
        tokio::fs::create_dir_all(output_dir.join(&dir.filename)).await?;
    }

    let progress = common::download_progress();
    let pipeline = downloader.create_pipeline(progress);

    let mut tasks = tokio::task::JoinSet::new();
    for file in &files {
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
