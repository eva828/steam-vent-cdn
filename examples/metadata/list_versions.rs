//! List all available versions of a Steam Workshop item.
//!
//! Usage:
//!     cargo run --example list_versions -- <APP_ID> <ITEM_ID>

use chrono::DateTime;
use steam_vent_cdn::{CdnDownloader, Config, SteamClient};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: cargo run --example list_versions -- <APP_ID> <ITEM_ID>");
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

    println!("Found {} version(s):", versions.len());
    for version in versions {
        let dt =
            DateTime::from_timestamp(version.timestamp as i64, 0).unwrap_or(DateTime::UNIX_EPOCH);
        println!(
            "  timestamp={}  manifest_id={}  date={}  description={}",
            version.timestamp, version.manifest_id, dt, version.description
        );
    }

    Ok(())
}
