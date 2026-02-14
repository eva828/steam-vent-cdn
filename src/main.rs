use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

mod downloader;
mod protos;
use downloader::TModDownloader;

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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    println!("Item ID: {}", args.item_id);

    let mut downloader = TModDownloader::new(args.username, args.password).await?;
    downloader.login().await?;

    if args.list {
        downloader.list_files(args.item_id, &args.filter).await?;
    } else {
        let downloaded = downloader
            .download_item(args.item_id, &args.output, &args.filter)
            .await?;
        println!("Downloaded {} files.", downloaded.len());
    }

    Ok(())
}
