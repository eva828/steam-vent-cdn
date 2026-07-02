//! List all files in a Steam Workshop item without downloading them.
//!
//! Usage:
//!     cargo run --example list_files -- <APP_ID> <ITEM_ID>

use steam_vent_cdn::config::Config;
use steam_vent_cdn::connection::SteamClient;
use steam_vent_cdn::downloader::client::CdnDownloader;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: cargo run --example list_files -- <APP_ID> <ITEM_ID>");
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

    let manifest = downloader
        .fetch_manifest_info(item_id, latest.manifest_id)
        .await?;

    println!("Directories: {}", manifest.directories.len());
    for dir in manifest.directories {
        println!("  [dir] {}", dir.filename);
    }

    println!("Files: {}", manifest.files.len());
    for file in manifest.files {
        let size = file.size.unwrap_or(0);
        println!(
            "  {} ({} bytes, {} chunks)",
            file.filename,
            size,
            file.chunks.len()
        );
    }

    Ok(())
}
