use std::path::Path;
use std::sync::Arc;

use futures::stream::TryStreamExt;
use indicatif::MultiProgress;

use steam_vent_cdn::downloader::Manifest;
use steam_vent_cdn::downloader::client::CdnDownloader;
use steam_vent_cdn::error::{Result, SteamVentError, WorkshopError};

use super::progress::ProgressManager;
use super::{Args, ui, writer};

async fn download_files(
    manifest: &Manifest,
    item_id: u64,
    app_id: u32,
    timestamp: u32,
    output_dir: &Path,
    downloader: &CdnDownloader,
    multi: &MultiProgress,
) -> Result<()> {
    let base_path = output_dir
        .join(app_id.to_string())
        .join(item_id.to_string())
        .join(timestamp.to_string());

    for dir in &manifest.directories {
        let output_path = base_path.join(&dir.filename);
        tokio::fs::create_dir_all(&output_path).await?;
    }

    let progress = ProgressManager::new(multi, &manifest.files);
    let progress = Arc::new(progress);

    let pipeline = downloader.create_pipeline(progress.clone());

    let mut tasks = tokio::task::JoinSet::new();
    for file in &manifest.files {
        let pipeline = pipeline.clone();
        let base = base_path.clone();
        let file = file.clone();
        tasks.spawn(async move {
            let mut stream = pipeline.download_file(&file)?;
            writer::write_file(&mut stream, &base).await
        });
    }

    while let Some(result) = tasks.join_next().await {
        result.map_err(|e| {
            SteamVentError::Io(std::io::Error::other(format!(
                "File download task panicked: {}",
                e
            )))
        })??;
    }

    progress.finish();
    Ok(())
}

pub async fn run_list_versions_pipeline(
    downloader: &CdnDownloader,
    item_id: u64,
) -> Result<()> {
    let versions: Vec<_> = downloader
        .fetch_versions(item_id, None)
        .try_collect()
        .await?;
    ui::display_version_history(item_id, &versions);
    Ok(())
}

pub async fn run_download_pipeline(
    args: &Args,
    downloader: &CdnDownloader,
    multi: &MultiProgress,
) -> Result<()> {
    let version = if let Some(timestamp) = args.timestamp {
        let mut stream = downloader.fetch_versions(args.item_id, None);
        let mut found = None;
        while let Some(version) = stream.try_next().await? {
            if version.timestamp == timestamp {
                found = Some(version);
                break;
            }
        }
        found.ok_or_else(|| {
            SteamVentError::Workshop(WorkshopError::VersionNotFound(format!(
                "Timestamp {}",
                timestamp
            )))
        })?
    } else {
        downloader
            .fetch_versions(args.item_id, Some(1))
            .try_next()
            .await?
            .ok_or_else(|| SteamVentError::Workshop(WorkshopError::NoVersionsFound))?
    };

    let manifest = downloader
        .fetch_manifest_by_id(version.manifest_id, args.app_id)
        .await?;

    tracing::info!("Downloading version with timestamp: {}", version.timestamp);
    tracing::info!("Description: {}", version.description);

    download_files(
        &manifest,
        args.item_id,
        args.app_id,
        version.timestamp,
        &args.output,
        downloader,
        multi,
    )
    .await
}
