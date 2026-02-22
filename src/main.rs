mod cli;

use clap::Parser;
use cli::Args;
use steam_vent_cdn::{SteamClient, CdnDownloader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let multi = cli::setup_tracing(args.verbose);

    tracing::debug!("Item ID: {}", args.item_id);

    let credentials = cli::resolve_credentials(args.username.clone(), args.password.clone());
    let config = cli::create_config(credentials, args.app_id, args.parallel_download, args.parallel_decode);

    let steam_client = SteamClient::new(config);
    let steam_session = steam_client.connect().await?;

    let mut downloader = CdnDownloader::new(steam_session);

    cli::run_download_pipeline(&args, &mut downloader, &multi).await?;

    Ok(())
}
