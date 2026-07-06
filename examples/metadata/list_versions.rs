//! List all available versions of a Steam Workshop item.
//!
//! Usage:
//!     cargo run --example list_versions -- <APP_ID> <ITEM_ID>

use futures::stream::TryStreamExt;
use steam_vent_cdn::config::Config;
use steam_vent_cdn::connection::SteamClient;
use steam_vent_cdn::downloader::client::CdnDownloader;
use time::OffsetDateTime;
use time::macros::format_description;

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
    let downloader = CdnDownloader::connect(session).await?;

    let versions: Vec<_> = downloader
        .fetch_versions(item_id, None)
        .try_collect()
        .await?;

    let format = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");

    println!("Found {} version(s):", versions.len());
    for version in versions {
        let dt = OffsetDateTime::from_unix_timestamp(version.timestamp as i64)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);
        let date = dt.format(format).unwrap_or_else(|_| "Unknown".to_string());
        println!(
            "  timestamp={}  manifest_id={}  date={}  description={}",
            version.timestamp, version.manifest_id, date, version.description
        );
    }

    Ok(())
}
