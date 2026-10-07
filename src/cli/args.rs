use clap::ArgGroup;
use clap::Parser;
use steam_vent_cdn::config::ServerMode;

#[derive(Debug, Clone)]
pub enum ItemSpec {
    Id(u64),
    Slug(String),
}

impl std::str::FromStr for ItemSpec {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(s.parse::<u64>()
            .map(ItemSpec::Id)
            .unwrap_or_else(|_| ItemSpec::Slug(s.to_string())))
    }
}

impl std::fmt::Display for ItemSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ItemSpec::Id(id) => write!(f, "{}", id),
            ItemSpec::Slug(slug) => write!(f, "{}", slug),
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about,
    long_about = None,
    group(
        ArgGroup::new("item_spec")
            .args(["item", "item_id", "slug"])
            .multiple(false)
    ),
    group(
        ArgGroup::new("version_spec")
            .args(["timestamp", "manifest_id"])
            .multiple(false)
    )
)]
pub struct Args {
    #[arg(help = "The ID of the app that the workshop item belongs to")]
    pub app_id: u32,

    #[arg(
        group = "item_spec",
        value_name = "ITEM",
        help = "Workshop item ID (numeric) or slug (non-numeric, auto-detected)"
    )]
    pub item: Option<ItemSpec>,

    #[arg(
        long,
        group = "item_spec",
        help = "Explicit workshop item ID (for scripting)"
    )]
    pub item_id: Option<u64>,

    #[arg(
        long,
        group = "item_spec",
        help = "Workshop item slug (for numeric slugs)"
    )]
    pub slug: Option<String>,

    #[arg(
        short,
        long,
        default_value = "./output",
        help = "The directory where the downloaded files will be saved"
    )]
    pub output: std::path::PathBuf,

    #[arg(
        short,
        long,
        env = "STEAM_USERNAME",
        help = "Steam username for authentication"
    )]
    pub username: Option<String>,

    #[arg(
        short,
        long,
        env = "STEAM_PASSWORD",
        hide_env_values = true,
        help = "If a username is provided but the password is omitted, you will be prompted for it."
    )]
    pub password: Option<String>,

    #[arg(long, help = "List all available versions of the workshop item")]
    pub list_versions: bool,

    #[arg(
        long,
        group = "version_spec",
        value_name = "TIMESTAMP",
        help = "Download specific version by timestamp (e.g., 1708434000)"
    )]
    pub timestamp: Option<u32>,

    #[arg(
        long,
        group = "version_spec",
        help = "Download specific version by manifest ID"
    )]
    pub manifest_id: Option<u64>,

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
