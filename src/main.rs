mod archive;
mod cdn;
mod config;
mod connection;
mod downloader;
mod error;
mod manifest;

use clap::Parser;
use downloader::{CdnDownloader, DownloadItem};
use error::Result;
use futures::StreamExt;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tracing_subscriber::Layer;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    item_id: u64,

    #[arg(short, long)]
    app_id: u32,

    #[arg(short, long, default_value = "./output")]
    output: PathBuf,

    #[arg(short, long)]
    filter: Option<String>,

    #[arg(short, long)]
    username: Option<String>,

    #[arg(
        short,
        long,
        help = "If a username is provided but the password is omitted, you will be prompted for it."
    )]
    password: Option<String>,

    #[arg(short, long)]
    list: bool,

    #[arg(short = 'c', long, default_value_t = 8)]
    concurrent_downloads: usize,

    #[arg(short, long)]
    verbose: bool,
}

fn setup_tracing(verbose: bool) -> tracing_appender::non_blocking::WorkerGuard {
    // Write logs to a file in the current directory, clearing previous contents
    let log_file = std::fs::File::create("run.log").expect("Failed to create log file");
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

    let base_stdout = tracing_subscriber::fmt::layer()
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_ansi(true)
        .with_writer(std::io::stdout);

    let stdout_layer = if verbose {
        base_stdout
            .with_level(true)
            .with_target(true)
            .with_timer(UtcTime::rfc_3339())
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

    guard
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let _guard = setup_tracing(args.verbose);

    tracing::debug!("Item ID: {}", args.item_id);

    let credentials = match (args.username, args.password) {
        (Some(username), Some(password)) => {
            crate::config::Credentials::UserPass { username, password }
        }
        (Some(username), None) => {
            let password = rpassword::prompt_password("Password: ").unwrap_or_default();
            crate::config::Credentials::UserPass { username, password }
        }
        _ => crate::config::Credentials::Anonymous,
    };
    let config = crate::config::DownloaderConfig::new(credentials)
        .with_app_id(args.app_id)
        .with_concurrent_downloads(args.concurrent_downloads);

    let steam_client = crate::connection::SteamClient::new(config);

    let steam_session = steam_client.connect().await?;

    let mut downloader = CdnDownloader::new(steam_session);

    let manifest_files = downloader.fetch_manifest_info(args.item_id).await?;

    let items: Vec<DownloadItem> = manifest_files
        .into_iter()
        .filter(|(filename, _)| {
            if let Some(f) = &args.filter {
                filename.ends_with(f)
            } else {
                true
            }
        })
        .map(|(filename, manifest_info)| DownloadItem {
            item_id: args.item_id,
            filename,
            manifest_info,
        })
        .collect();

    const FLAG_DIRECTORY: u32 = 0x40;

    let mut directories: Vec<DownloadItem> = Vec::new();
    let mut files: Vec<DownloadItem> = Vec::new();

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

    for dir in &directories {
        let output_path = args
            .output
            .join(args.app_id.to_string())
            .join(args.item_id.to_string())
            .join(&dir.filename);

        tokio::fs::create_dir_all(&output_path).await?;
    }

    for file in files {
        let output_path = args
            .output
            .join(args.app_id.to_string())
            .join(args.item_id.to_string())
            .join(&file.filename);

        if let Some(parent) = output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let stream = downloader.get_stream(&file).await?;
        tokio::pin!(stream);

        let mut file_handle = tokio::fs::File::create(&output_path).await?;

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            file_handle.write_all(&chunk).await?;
        }

        tracing::info!("Downloaded: {}", file.filename);
    }

    Ok(())
}
