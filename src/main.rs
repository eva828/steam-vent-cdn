mod cli;

use clap::Parser;
use cli::args::ItemSpec;
use cli::Args;
use steam_vent_cdn::connection::SteamClient;
use steam_vent_cdn::downloader::client::CdnDownloader;
use steam_vent_cdn::error::{Result, WorkshopError};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (multi, _guard) = cli::setup_tracing(args.verbose, &args.output)?;

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

    let item_id = resolve_item_id(&downloader, &args).await?;

    if args.list_versions {
        cli::run_list_versions_pipeline(&downloader, item_id).await?;
    } else {
        cli::run_download_pipeline(
            &downloader,
            &multi,
            args.app_id,
            item_id,
            args.output,
            args.timestamp,
            args.manifest_id,
        )
        .await?;
    }

    Ok(())
}

async fn resolve_item_id(downloader: &CdnDownloader, args: &Args) -> Result<u64> {
    match &args.item {
        Some(ItemSpec::Id(id)) => Ok(*id),
        Some(ItemSpec::Slug(slug)) => {
            let id = downloader.fetch_item_id_by_slug(args.app_id, slug).await?;
            tracing::info!("Resolved slug '{}' to item ID {}", slug, id);
            Ok(id)
        }
        None => {
            if let Some(id) = args.item_id {
                Ok(id)
            } else if let Some(slug) = &args.slug {
                let id = downloader.fetch_item_id_by_slug(args.app_id, slug).await?;
                tracing::info!("Resolved slug '{}' to item ID {}", slug, id);
                Ok(id)
            } else {
                Err(WorkshopError::MissingItemId)?
            }
        }
    }
}
