mod archive;
mod cdn;
mod config;
mod connection;
mod downloader;
mod error;
mod manifest;

use clap::Parser;
use downloader::{CdnDownloader, DownloadItem};
use error::{Result, SteamVentError};
use futures::StreamExt;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    item_id: u64,

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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    println!("Item ID: {}", args.item_id);

    let credentials = match (args.username, args.password) {
        (Some(username), Some(password)) => {
            crate::config::Credentials::UserPass { username, password }
        }
        _ => crate::config::Credentials::Anonymous,
    };
    let config = crate::config::DownloaderConfig::new(credentials);

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

    println!("Found {} files to download.", items.len());

    for item in items {
        let stream = downloader.get_stream(&item).await?;
        let output_path = args.output.join(&item.filename);

        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = std::fs::File::create(&output_path)?;

        let chunks: Vec<std::result::Result<Vec<u8>, SteamVentError>> = stream.collect().await;

        for chunk_result in chunks {
            let chunk = chunk_result?;
            std::io::Write::write_all(&mut file, &chunk)?;
        }

        println!("Downloaded: {}", item.filename);
    }

    Ok(())
}
