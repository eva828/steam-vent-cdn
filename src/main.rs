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
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    item_id: u64,

    #[arg(short, long)]
    app_id: Option<u32>,

    #[arg(short, long, default_value = "./mods")]
    output: PathBuf,

    #[arg(short, long, default_value = ".tmod")]
    filter: String,

    #[arg(short, long)]
    username: Option<String>,

    #[arg(short, long)]
    password: Option<String>,

    #[arg(short, long)]
    list: bool,

    #[arg(short = 'c', long)]
    concurrent_downloads: Option<usize>,

    #[arg(short, long)]
    verbose: bool,
}

fn setup_tracing(verbose: bool) -> tracing_appender::non_blocking::WorkerGuard {
    // Write logs to a file in the current directory
    let file_appender = tracing_appender::rolling::never(".", "run.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

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
        base_stdout.with_level(true).with_target(true).boxed()
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

    log::info!("Item ID: {}", args.item_id);

    let credentials = match (args.username, args.password) {
        (Some(username), Some(password)) => {
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

    let files = downloader.fetch_manifest_info(args.item_id).await?;

    let items: Vec<DownloadItem> = files
        .into_iter()
        .filter(|(filename, _)| filename.ends_with(".tmod"))
        .map(|(filename, manifest_info)| DownloadItem {
            item_id: args.item_id,
            filename,
            manifest_info,
        })
        .collect();

    log::info!("Found {} files to download.", items.len());

    for item in items {
        let stream = downloader.get_stream(&item).await?;
        tokio::pin!(stream);
        let output_path = args.output.join(&item.filename);

        if let Some(parent) = output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut file = tokio::fs::File::create(&output_path).await?;

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            file.write_all(&chunk).await?;
        }

        log::info!("Downloaded: {}", item.filename);
    }

    Ok(())
}
