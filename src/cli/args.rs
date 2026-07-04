use clap::Parser;
use steam_vent_cdn::config::ServerMode;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(help = "The ID of the app that the workshop item belongs to")]
    pub app_id: u32,

    #[arg(help = "The ID of the workshop item to download")]
    pub item_id: u64,

    #[arg(
        short,
        long,
        default_value = "./output",
        help = "The directory where the downloaded files will be saved"
    )]
    pub output: std::path::PathBuf,

    #[arg(short, long, help = "Steam username for authentication")]
    pub username: Option<String>,

    #[arg(
        short,
        long,
        help = "If a username is provided but the password is omitted, you will be prompted for it."
    )]
    pub password: Option<String>,

    #[arg(long, help = "List all available versions of the workshop item")]
    pub list_versions: bool,

    #[arg(
        long,
        value_name = "TIMESTAMP",
        help = "Download specific version by timestamp (e.g., 1708434000)"
    )]
    pub timestamp: Option<u32>,

    #[arg(long, default_value_t = 16, help = "Parallel download connections")]
    pub parallel_download: usize,

    #[arg(
        long,
        value_enum,
        default_value = "cache-preferred",
        help = "Server selection mode: cache-preferred(alias: all), cdn, or cache"
    )]
    pub server_mode: ServerMode,

    #[arg(short, long, help = "Enable verbose output")]
    pub verbose: bool,
}
