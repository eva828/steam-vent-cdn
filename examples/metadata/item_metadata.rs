//! Show metadata for a Steam Workshop item.
//!
//! Usage:
//!     cargo run --example item_metadata -- <ITEM_ID>

use chrono::DateTime;
use steam_vent_cdn::{CdnDownloader, Config, SteamClient};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: cargo run --example item_metadata -- <ITEM_ID>");
        std::process::exit(1);
    }

    let item_id: u64 = args[1].parse()?;

    tracing_subscriber::fmt::init();

    let config = Config::builder().build();
    let client = SteamClient::new(config);
    let session = client.connect().await?;
    let mut downloader = CdnDownloader::new(session)?;

    let details = downloader.fetch_item_details(item_id).await?;

    println!("Published File ID: {}", details.publishedfileid);
    println!("Title: {}", details.title);
    println!("Description: {}", details.description);
    println!("Consumer App ID: {:?}", details.consumer_appid);
    println!("Manifest ID: {:?}", details.manifest_id);
    println!("Preview URL: {:?}", details.preview_url);
    println!("File URL: {:?}", details.file_url);

    if let Some(ts) = details.time_created {
        let dt = DateTime::from_timestamp(ts as i64, 0).unwrap_or(DateTime::UNIX_EPOCH);
        println!("Created: {} ({})", dt, ts);
    }
    if let Some(ts) = details.time_updated {
        let dt = DateTime::from_timestamp(ts as i64, 0).unwrap_or(DateTime::UNIX_EPOCH);
        println!("Updated: {} ({})", dt, ts);
    }

    println!("Creator: {:?}", details.creator);
    if !details.tags.is_empty() {
        println!("Tags: {}", details.tags.join(", "));
    }

    Ok(())
}
