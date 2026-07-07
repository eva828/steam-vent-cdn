use std::path::Path;
use std::sync::Arc;

use futures::stream::TryStreamExt;
use indicatif::MultiProgress;
use url::Url;

use steam_vent_cdn::downloader::client::CdnDownloader;
use steam_vent_cdn::downloader::{ItemDetails, Manifest, VersionInfo};
use steam_vent_cdn::error::{Result, WorkshopError};

use super::progress::ProgressManager;
use super::{Args, ui, writer};

pub async fn run_list_versions_pipeline(downloader: &CdnDownloader, item_id: u64) -> Result<()> {
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
    let version = resolve_version(downloader, args.item_id, args.timestamp).await?;

    tracing::info!("Downloading version with timestamp: {}", version.timestamp);
    tracing::info!("Description: {}", version.description);

    let details = downloader.fetch_item_details(args.item_id).await?;

    let base_path = args
        .output
        .join(args.app_id.to_string())
        .join(args.item_id.to_string())
        .join(version.timestamp.to_string());

    if let Some(file_url) = &details.file_url {
        let filename = extract_filename(&details, file_url);
        download_web_item(downloader, file_url, &filename, &base_path).await?;
    } else {
        let manifest = downloader
            .fetch_manifest_by_id(version.manifest_id, args.app_id)
            .await?;

        download_cdn_files(&manifest, &base_path, downloader, multi).await?;
    }

    Ok(())
}

async fn resolve_version(
    downloader: &CdnDownloader,
    item_id: u64,
    timestamp: Option<u32>,
) -> Result<VersionInfo> {
    if let Some(ts) = timestamp {
        let mut stream = downloader.fetch_versions(item_id, None);
        while let Some(version) = stream.try_next().await? {
            if version.timestamp == ts {
                return Ok(version);
            }
        }
        Err(WorkshopError::VersionNotFound(format!("Timestamp {}", ts)))?
    } else {
        Ok(downloader
            .fetch_versions(item_id, Some(1))
            .try_next()
            .await?
            .ok_or(WorkshopError::NoVersionsFound)?)
    }
}

fn extract_filename(details: &ItemDetails, file_url: &str) -> String {
    if let Some(name) = details.filename.as_deref().filter(|s| !s.is_empty()) {
        return name.to_string();
    }
    Url::parse(file_url)
        .ok()
        .and_then(|u| {
            u.path_segments()?
                .next_back()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "workshop_item".to_string())
}

async fn download_web_item(
    downloader: &CdnDownloader,
    file_url: &str,
    filename: &str,
    base_path: &Path,
) -> Result<()> {
    let output_path = base_path.join(filename);
    tracing::info!("Downloading web file from {}", file_url);
    let data: Vec<u8> = downloader.download_web_file(file_url).await?;
    let size = data.len() as u64;
    tokio::fs::create_dir_all(output_path.parent().unwrap_or(&output_path)).await?;
    tokio::fs::write(&output_path, &data).await?;
    tracing::info!("Downloaded web file ({} bytes) to {:?}", size, output_path);
    Ok(())
}

async fn download_cdn_files(
    manifest: &Manifest,
    base_path: &Path,
    downloader: &CdnDownloader,
    multi: &MultiProgress,
) -> Result<()> {
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
        let base = base_path.to_owned();
        let file = file.clone();
        tasks.spawn(async move {
            let mut stream = pipeline.download_file(&file)?;
            writer::write_file(&mut stream, &base).await
        });
    }

    while let Some(result) = tasks.join_next().await {
        result
            .map_err(|e| std::io::Error::other(format!("File download task panicked: {}", e)))??;
    }

    progress.finish();
    Ok(())
}
