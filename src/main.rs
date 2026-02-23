mod cli;

use clap::Parser;
use cli::Args;
use steam_vent_cdn::{CdnDownloader, Result, SteamClient};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let multi = cli::setup_tracing(args.verbose, &args.output)?;

    tracing::debug!("Item ID: {}", args.item_id);

    let credentials = cli::resolve_credentials(args.username.clone(), args.password.clone())?;
    let config = cli::create_config(
        credentials,
        args.app_id,
        args.parallel_download,
        args.parallel_decode,
    );

    let steam_client = SteamClient::new(config);
    let steam_session = steam_client.connect().await?;

    let mut downloader = CdnDownloader::new(steam_session);

    if args.list_versions {
        cli::run_list_versions_pipeline(&mut downloader, args.item_id).await?;
    } else {
        cli::run_download_pipeline(&args, &mut downloader, &multi).await?;
    }

    Ok(())
}
