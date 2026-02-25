pub mod args;
pub mod progress;
pub mod ui;

pub use args::Args;
use indicatif::MultiProgress;
use progress::ProgressManager;
use std::sync::Arc;
use steam_vent_cdn::{
    CdnDownloader, Config, Credentials, DownloadItem, ManifestError, Result, RetryPolicy,
    SteamVentError,
};
use tracing_subscriber::Layer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::util::SubscriberInitExt;

pub fn setup_tracing(verbose: bool, output_dir: &std::path::Path) -> Result<MultiProgress> {
    std::fs::create_dir_all(output_dir)?;
    let log_path = output_dir.join("steam-vent-cdn.log");
    let log_file = std::fs::File::create(&log_path)?;
    let (non_blocking, guard) = tracing_appender::non_blocking(log_file);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if verbose {
            tracing_subscriber::EnvFilter::new("steam_vent_cdn=debug,steam_vent=debug,info")
        } else {
            tracing_subscriber::EnvFilter::new("info")
        }
    });

    let file_layer = tracing_subscriber::fmt::layer()
        .with_level(true)
        .with_target(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_ansi(false)
        .with_writer(non_blocking)
        .boxed();

    let multi = MultiProgress::new();
    let writer =
        tracing_indicatif::IndicatifWriter::<tracing_indicatif::writer::Stdout>::new(multi.clone());

    let base_stdout = tracing_subscriber::fmt::layer()
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_ansi(true)
        .with_writer(writer);

    let stdout_layer = if verbose {
        base_stdout
            .with_level(true)
            .with_target(true)
            .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
            .boxed()
    } else {
        base_stdout
            .with_level(false)
            .with_target(false)
            .without_time()
            .boxed()
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    std::mem::forget(guard);
    Ok(multi)
}

pub fn resolve_credentials(
    username: Option<String>,
    password: Option<String>,
) -> Result<Credentials> {
    match (username, password) {
        (Some(username), Some(password)) => Ok(Credentials::UserPass { username, password }),
        (Some(username), None) => {
            let password = rpassword::prompt_password("Password: ").map_err(|e| {
                SteamVentError::Manifest(ManifestError::Custom(format!(
                    "Password input failed: {}",
                    e
                )))
            })?;
            Ok(Credentials::UserPass { username, password })
        }
        _ => Ok(Credentials::Anonymous),
    }
}

pub fn create_config(credentials: Credentials, app_id: u32, parallel_download: usize) -> Config {
    Config::builder()
        .credentials(credentials)
        .app_id(app_id)
        .parallel_download(parallel_download)
        .timeout(std::time::Duration::from_secs(30))
        .retry_policy(RetryPolicy {
            max_retries: 3,
            retry_delay: std::time::Duration::from_secs(1),
        })
        .build()
}

const FLAG_DIRECTORY: u32 = 0x40;

pub(crate) fn classify_items(items: Vec<DownloadItem>) -> (Vec<DownloadItem>, Vec<DownloadItem>) {
    let mut directories = Vec::new();
    let mut files = Vec::new();

    for item in items {
        if item.flags.unwrap_or(0) & FLAG_DIRECTORY != 0 {
            directories.push(item);
        } else {
            files.push(item);
        }
    }

    tracing::info!(
        "Found {} directories and {} files to download.",
        directories.len(),
        files.len()
    );

    (directories, files)
}

pub async fn run_list_versions_pipeline(
    downloader: &mut CdnDownloader,
    item_id: u64,
) -> Result<()> {
    let versions = downloader.fetch_change_history(item_id).await?;
    ui::display_version_history(item_id, &versions);
    Ok(())
}

pub async fn run_download_pipeline(
    args: &Args,
    downloader: &mut CdnDownloader,
    multi: &MultiProgress,
) -> Result<()> {
    let versions = downloader.fetch_change_history(args.item_id).await?;

    let timestamp = if let Some(ts) = args.version {
        ts
    } else {
        versions.first().map(|v| v.timestamp).ok_or_else(|| {
            steam_vent_cdn::SteamVentError::InvalidData("No versions found".to_string())
        })?
    };

    let version_info = versions
        .iter()
        .find(|v| v.timestamp == timestamp)
        .ok_or_else(|| {
            steam_vent_cdn::SteamVentError::InvalidData(format!(
                "Timestamp {} not found in version history",
                timestamp
            ))
        })?;

    tracing::info!("Downloading version with timestamp: {}", timestamp);
    tracing::info!("Description: {}", version_info.description);

    let items = downloader.fetch_manifest_info(args.item_id).await?;

    let base_path = args
        .output
        .join(args.app_id.to_string())
        .join(args.item_id.to_string())
        .join(timestamp.to_string());

    let (directories, files) = classify_items(items);

    if args.list {
        ui::display_file_list(args.item_id, &files);
        return Ok(());
    }

    for dir in directories {
        let output_path = base_path.join(&dir.filename);

        tokio::fs::create_dir_all(&output_path).await?;
    }

    let progress = ProgressManager::new(multi, &files);
    let progress = Arc::new(progress);

    downloader
        .download_to_directory(&files, &base_path, progress.clone())
        .await?;

    progress.finish();
    Ok(())
}
