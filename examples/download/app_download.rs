//! Download all depots of an app for the current platform.
//!
//! Usage:
//!     cargo run --example app_download -- <APP_ID> [OUTPUT_DIR]

use std::path::PathBuf;
use std::sync::Arc;

use steam_vent_cdn::config::Config;
use steam_vent_cdn::connection::SteamClient;
use steam_vent_cdn::downloader::client::CdnDownloader;

#[path = "../_common.rs"]
mod common;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        eprintln!("Usage: cargo run --example app_download -- <APP_ID> [OUTPUT_DIR]");
        std::process::exit(1);
    }

    let app_id: u32 = args[1].parse()?;
    let output_dir = PathBuf::from(args.get(2).map(String::as_str).unwrap_or("./output"));

    tracing_subscriber::fmt::init();

    let config = Config::builder().app_id(app_id).build();
    let client = SteamClient::new(config);
    let session = client.connect().await?;
    let downloader = CdnDownloader::connect(session).await?;

    let info = downloader.fetch_app_info(app_id).await?;
    println!("App '{}' (build {})", info.name, info.build_id);

    // Same mapping as `Platform::current().oslist_name()` in the CLI:
    // unknown hosts download every depot instead of filtering everything out.
    let os = match std::env::consts::OS {
        "windows" => Some("windows"),
        "macos" => Some("macos"),
        "linux" => Some("linux"),
        _ => None,
    };
    let base = output_dir
        .join(app_id.to_string())
        .join(info.build_id.to_string());

    for depot in info.depots_for_os(os) {
        if let Some(owner) = depot.depot_from_app {
            println!(
                "Skipping shared depot {} (provided by app {})",
                depot.depot_id, owner
            );
            continue;
        }
        println!("Depot {} ({} bytes)", depot.depot_id, depot.size);

        let manifest = downloader
            .fetch_manifest_by_id(depot.manifest_id, app_id, depot.depot_id)
            .await?;
        let depot_dir = base.join(depot.depot_id.to_string());

        for dir in &manifest.directories {
            tokio::fs::create_dir_all(depot_dir.join(&dir.filename)).await?;
        }

        let progress = Arc::new(common::NoopProgress);
        let pipeline = downloader.create_pipeline(progress);

        let mut tasks = tokio::task::JoinSet::new();
        for file in &manifest.files {
            let pipeline = pipeline.clone();
            let base = depot_dir.clone();
            let file = file.clone();
            tasks.spawn(async move {
                let mut stream = pipeline.download_file(&file)?;
                common::write_stream_to_file(&mut stream, &base).await
            });
        }

        while let Some(result) = tasks.join_next().await {
            result??;
        }
    }

    Ok(())
}
