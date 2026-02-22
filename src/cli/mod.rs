pub mod args;
pub mod progress;
pub mod ui;

pub use args::Args;
use progress::ProgressManager;
use steam_vent_cdn::{Config, Credentials, Result, CdnDownloader, DownloadItem};
use indicatif::MultiProgress;
use std::sync::Arc;
use tracing_subscriber::Layer;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::prelude::*;

pub fn setup_tracing(verbose: bool, output_dir: &std::path::Path) -> std::result::Result<MultiProgress, Box<dyn std::error::Error>> {
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
    let writer = tracing_indicatif::IndicatifWriter::<tracing_indicatif::writer::Stdout>::new(multi.clone());

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

pub fn resolve_credentials(username: Option<String>, password: Option<String>) -> Credentials {
    match (username, password) {
        (Some(username), Some(password)) => Credentials::UserPass { username, password },
        (Some(username), None) => {
            let password = rpassword::prompt_password("Password: ").unwrap_or_default();
            Credentials::UserPass { username, password }
        }
        _ => Credentials::Anonymous,
    }
}

pub fn create_config(
    credentials: Credentials,
    app_id: u32,
    parallel_download: usize,
    parallel_decode: usize,
) -> Config {
    Config::builder()
        .credentials(credentials)
        .app_id(app_id)
        .parallel_download(parallel_download)
        .parallel_decode(parallel_decode)
        .build()
}

const FLAG_DIRECTORY: u32 = 0x40;

pub fn classify_items(items: Vec<DownloadItem>) -> (Vec<DownloadItem>, Vec<DownloadItem>) {
    let mut directories = Vec::new();
    let mut files = Vec::new();

    for item in items {
        if item.manifest_info.flags.unwrap_or(0) & FLAG_DIRECTORY != 0 {
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

pub async fn create_directories(
    output: &std::path::Path,
    app_id: u32,
    item_id: u64,
    directories: &[DownloadItem],
) -> Result<()> {
    for dir in directories {
        let output_path = output
            .join(app_id.to_string())
            .join(item_id.to_string())
            .join(&dir.filename);

        tokio::fs::create_dir_all(&output_path).await?;
    }
    Ok(())
}

pub async fn prepare_output_directories(
    output: &std::path::Path,
    app_id: u32,
    item_id: u64,
    files: &[DownloadItem],
) -> Result<()> {
    for file in files {
        let output_path = output
            .join(app_id.to_string())
            .join(item_id.to_string())
            .join(&file.filename);

        if let Some(parent) = output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    Ok(())
}

pub async fn run_download_pipeline(
    args: &Args,
    downloader: &mut CdnDownloader,
    multi: &MultiProgress,
) -> Result<()> {
    let manifest_files = downloader.fetch_manifest_info(args.item_id).await?;

    let items: Vec<DownloadItem> = manifest_files
        .into_iter()
        .map(|(filename, manifest_info)| DownloadItem {
            item_id: args.item_id,
            filename,
            manifest_info,
        })
        .collect();

    let (directories, files) = classify_items(items);

    if args.list {
        ui::display_file_list(args.item_id, &files);
        return Ok(());
    }

    create_directories(&args.output, args.app_id, args.item_id, &directories).await?;
    prepare_output_directories(&args.output, args.app_id, args.item_id, &files).await?;

    let progress = ProgressManager::new(multi, &files);
    let progress = Arc::new(progress);

    let base_path = args
        .output
        .join(args.app_id.to_string())
        .join(args.item_id.to_string());

    downloader.download_to_directory(&files, &base_path, progress.clone()).await?;

    progress.finish();
    Ok(())
}
