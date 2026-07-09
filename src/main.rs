mod cli;

use clap::Parser;
use cli::Args;
use steam_vent_cdn::connection::SteamClient;
use steam_vent_cdn::downloader::client::CdnDownloader;
use steam_vent_cdn::error::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (multi, _guard) = cli::setup_tracing(args.verbose, &args.output)?;

    tracing::debug!("Item ID: {}", args.item_id);

    let credentials = cli::resolve_credentials(args.username.clone(), args.password.clone())?;
    let config = cli::create_config(
        credentials,
        args.app_id,
        args.parallel_download,
        args.server_mode,
    );

    let steam_client = SteamClient::new(config);
    let steam_session = steam_client.connect().await?;

    let downloader = CdnDownloader::connect(steam_session).await?;

    if args.list_versions {
        cli::run_list_versions_pipeline(&downloader, args.item_id).await?;
    } else {
        cli::run_download_pipeline(&args, &downloader, &multi).await?;
    }

    Ok(())
}
