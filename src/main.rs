mod cli;

use clap::Parser;
use cli::Command;
use cli::args::{ItemArgs, ItemSpec};
use steam_vent_cdn::connection::SteamClient;
use steam_vent_cdn::downloader::client::CdnDownloader;
use steam_vent_cdn::error::{Result, WorkshopError};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    let common = &cli.common;

    let (multi, _guard) = cli::setup_tracing(common.verbose, &common.output)?;

    let credentials = cli::resolve_credentials(common.username.clone(), common.password.clone())?;

    let app_id = match &cli.command {
        Command::Item(args) => args.app_id,
        Command::App(args) => args.app_id,
    };

    let config = cli::create_config(
        credentials,
        app_id,
        common.parallel_download,
        common.server_mode,
    );

    let steam_client = SteamClient::new(config);
    let steam_session = steam_client.connect().await?;

    // ACF fields stay at 0 for anonymous sessions.
    let identity = if steam_session.is_anonymous() {
        cli::SessionIdentity {
            steam_id: 0,
            account_id: 0,
        }
    } else {
        cli::SessionIdentity {
            steam_id: steam_session.steam_id(),
            account_id: steam_session.account_id(),
        }
    };

    let downloader = CdnDownloader::connect(steam_session).await?;

    match &cli.command {
        Command::Item(args) => {
            let item_id = resolve_item_id(&downloader, args).await?;

            if args.list_versions {
                cli::run_list_versions_pipeline(&downloader, item_id).await?;
            } else {
                cli::run_download_pipeline(
                    &downloader,
                    &multi,
                    args.app_id,
                    item_id,
                    common.output.clone(),
                    args.timestamp,
                    args.manifest_id,
                    common.layout,
                    &identity,
                )
                .await?;
            }
        }
        Command::App(args) => {
            let platform = args.os.unwrap_or_default();
            cli::run_app_download_pipeline(
                &downloader,
                &multi,
                args.app_id,
                common.output.clone(),
                platform,
                &args.manifest,
                common.layout,
                common.shared_depots,
                &identity,
            )
            .await?;
        }
    }

    Ok(())
}

async fn resolve_item_id(downloader: &CdnDownloader, args: &ItemArgs) -> Result<u64> {
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
