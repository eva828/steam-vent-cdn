use clap::Parser;

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

    #[arg(
        short,
        long,
        help = "Only list the files in the workshop item without downloading"
    )]
    pub list: bool,

    #[arg(long, default_value_t = 16, help = "Parallel download connections")]
    pub parallel_download: usize,

    #[arg(long, default_value_t = 8, help = "Parallel decode workers")]
    pub parallel_decode: usize,

    #[arg(short, long, help = "Enable verbose output")]
    pub verbose: bool,
}
