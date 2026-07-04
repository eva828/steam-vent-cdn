//! Demonstrate error handling patterns for SteamVentError variants.
//!
//! Usage:
//!     cargo run --example error_handling -- <APP_ID> <ITEM_ID>

use futures::stream::TryStreamExt;
use steam_vent_cdn::config::Config;
use steam_vent_cdn::connection::SteamClient;
use steam_vent_cdn::downloader::client::CdnDownloader;
use steam_vent_cdn::error::SteamVentError;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: cargo run --example error_handling -- <APP_ID> <ITEM_ID>");
        std::process::exit(1);
    }

    let app_id: u32 = args[1].parse()?;
    let item_id: u64 = args[2].parse()?;

    tracing_subscriber::fmt::init();

    let config = Config::builder().app_id(app_id).build();
    let client = SteamClient::new(config);
    let session = client.connect().await?;
    let downloader = CdnDownloader::connect(session).await?;

    match downloader.fetch_versions(item_id, None).try_collect::<Vec<_>>().await {
        Ok(versions) => {
            println!("Found {} version(s)", versions.len());
        }
        Err(SteamVentError::Workshop(e)) => {
            eprintln!("Workshop error: {}", e);
        }
        Err(SteamVentError::SteamSession(e)) => {
            eprintln!("Steam session error: {}", e);
        }
        Err(SteamVentError::Cdn(e)) => {
            eprintln!("CDN error: {}", e);
        }
        Err(SteamVentError::Network(e)) => {
            eprintln!("Network error: {}", e);
        }
        Err(SteamVentError::Io(e)) => {
            eprintln!("IO error: {}", e);
        }
        Err(e) => {
            eprintln!("Other error: {}", e);
        }
    }

    Ok(())
}
