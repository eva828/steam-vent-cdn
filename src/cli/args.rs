use std::path::PathBuf;

use clap::ArgGroup;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
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
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Download a Steam Workshop item
    Item(ItemArgs),
    /// Download the depot contents of a Steam app
    App(AppArgs),
}

#[derive(clap::Args, Debug)]
pub struct CommonArgs {
    #[arg(
        short,
        long,
        global = true,
        default_value = "./output",
        help = "The directory where the downloaded files will be saved"
    )]
    pub output: PathBuf,

    #[arg(
        short,
        long,
        global = true,
        env = "STEAM_USERNAME",
        help = "Steam username for authentication"
    )]
    pub username: Option<String>,

    #[arg(
        short,
        long,
        global = true,
        env = "STEAM_PASSWORD",
        hide_env_values = true,
        help = "If a username is provided but the password is omitted, you will be prompted for it."
    )]
    pub password: Option<String>,

    #[arg(
        long,
        global = true,
        default_value_t = 16,
        help = "Parallel download connections"
    )]
    pub parallel_download: usize,

    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "cache-preferred",
        help = "Server selection mode: cache-preferred(alias: all), cdn, or cache"
    )]
    pub server_mode: ServerMode,

    #[arg(short, long, global = true, help = "Enable verbose output")]
    pub verbose: bool,

    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "depot",
        help = "Output layout: depot (one directory per depot) or steam (Steam client compatible)"
    )]
    pub layout: Layout,
}

/// Output directory layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Default)]
pub enum Layout {
    /// `output/<APP_ID>/<BUILD_ID>/<DEPOT_ID>/...`
    #[default]
    Depot,
    /// Steam client compatible layout under `steamapps/`.
    Steam,
}

/// Handling of depots provided by another app (shared installs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Default)]
pub enum SharedDepotsMode {
    /// Do not download shared depots; print the command needed to fetch them.
    #[default]
    Skip,
    /// Resolve and download the owning app automatically.
    Auto,
}

#[derive(clap::Args, Debug)]
#[command(
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
pub struct ItemArgs {
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
}

/// Platform used to filter depots by their `oslist`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Platform {
    Windows,
    Macos,
    Linux,
    All,
}

impl Platform {
    /// The platform of the running process, or `None` when it is not one of the
    /// platforms Steam serves depots for.
    pub fn detect() -> Option<Self> {
        match std::env::consts::OS {
            "windows" => Some(Self::Windows),
            "macos" => Some(Self::Macos),
            "linux" => Some(Self::Linux),
            _ => None,
        }
    }

    /// The token used in a depot's `oslist`, or `None` when every platform is included.
    pub fn oslist_name(self) -> Option<&'static str> {
        match self {
            Platform::Windows => Some("windows"),
            Platform::Macos => Some("macos"),
            Platform::Linux => Some("linux"),
            Platform::All => None,
        }
    }
}

/// Language used to pick localized depots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Language {
    /// Include every language.
    All,
    One(String),
}

impl Language {
    /// The language to filter with, or `None` when every language is included.
    pub fn as_filter(&self) -> Option<&str> {
        match self {
            Self::All => None,
            Self::One(language) => Some(language),
        }
    }
}

impl Default for Language {
    fn default() -> Self {
        Self::One("english".to_string())
    }
}

impl std::str::FromStr for Language {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() {
            return Err("expected a language name or 'all'".to_string());
        }
        if value.eq_ignore_ascii_case("all") {
            Ok(Self::All)
        } else {
            Ok(Self::One(value.to_ascii_lowercase()))
        }
    }
}

/// A `DEPOT_ID:MANIFEST_ID` pair used to override the manifest of a depot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepotManifest {
    pub depot_id: u32,
    pub manifest_id: u64,
}

impl std::str::FromStr for DepotManifest {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (depot, manifest) = value
            .split_once(':')
            .ok_or_else(|| format!("expected DEPOT_ID:MANIFEST_ID, got '{value}'"))?;
        let depot_id = depot
            .trim()
            .parse::<u32>()
            .map_err(|e| format!("invalid depot ID in '{value}': {e}"))?;
        let manifest_id = manifest
            .trim()
            .parse::<u64>()
            .map_err(|e| format!("invalid manifest ID in '{value}': {e}"))?;
        Ok(Self {
            depot_id,
            manifest_id,
        })
    }
}

#[derive(clap::Args, Debug)]
pub struct AppArgs {
    #[arg(help = "The ID of the app to download")]
    pub app_id: u32,

    #[arg(
        long,
        value_enum,
        help = "Only download depots for this platform (defaults to the current OS)"
    )]
    pub os: Option<Platform>,

    #[arg(
        long,
        default_value = "english",
        value_name = "LANGUAGE",
        help = "Download depots for this language; depots without a language are always included. Use 'all' for every language"
    )]
    pub language: Language,

    #[arg(
        long = "manifest",
        value_name = "DEPOT_ID:MANIFEST_ID",
        help = "Override the manifest of a depot; can be given multiple times. Forces inclusion even when filtered out by --os or --language. Unknown depots are rejected."
    )]
    pub manifest: Vec<DepotManifest>,

    #[arg(
        long,
        value_enum,
        default_value = "skip",
        help = "How to handle depots provided by another app with --layout steam: skip (print a command) or auto"
    )]
    pub shared_depots: SharedDepotsMode,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_item_subcommand() {
        let cli = Cli::try_parse_from(["steam-vent-cdn", "item", "4000", "160250458"]).unwrap();
        match cli.command {
            Command::Item(args) => {
                assert_eq!(args.app_id, 4000);
                assert!(matches!(args.item, Some(ItemSpec::Id(160250458))));
            }
            Command::App(_) => panic!("expected item subcommand"),
        }
    }

    #[test]
    fn parses_app_subcommand_with_global_options() {
        let cli = Cli::try_parse_from([
            "steam-vent-cdn",
            "app",
            "440",
            "--os",
            "linux",
            "--manifest",
            "441:123456789",
            "-o",
            "/tmp/out",
        ])
        .unwrap();
        assert_eq!(cli.common.output, PathBuf::from("/tmp/out"));
        match cli.command {
            Command::App(args) => {
                assert_eq!(args.app_id, 440);
                assert_eq!(args.os, Some(Platform::Linux));
                assert_eq!(
                    args.manifest,
                    vec![DepotManifest {
                        depot_id: 441,
                        manifest_id: 123456789
                    }]
                );
            }
            Command::Item(_) => panic!("expected app subcommand"),
        }
    }

    #[test]
    fn rejects_malformed_manifest_override() {
        assert!("441".parse::<DepotManifest>().is_err());
        assert!("x:1".parse::<DepotManifest>().is_err());
        assert!("441:y".parse::<DepotManifest>().is_err());
    }

    #[test]
    fn oslist_name_maps_platforms() {
        assert_eq!(Platform::Linux.oslist_name(), Some("linux"));
        assert_eq!(Platform::All.oslist_name(), None);
    }

    #[test]
    fn parses_layout_and_shared_depot_options() {
        let cli = Cli::try_parse_from([
            "steam-vent-cdn",
            "app",
            "440",
            "--layout",
            "steam",
            "--shared-depots",
            "auto",
        ])
        .unwrap();
        assert_eq!(cli.common.layout, Layout::Steam);
        match cli.command {
            Command::App(args) => assert_eq!(args.shared_depots, SharedDepotsMode::Auto),
            Command::Item(_) => panic!("expected app subcommand"),
        }

        let default = Cli::try_parse_from(["steam-vent-cdn", "app", "440"]).unwrap();
        assert_eq!(default.common.layout, Layout::Depot);
        match default.command {
            Command::App(args) => {
                assert_eq!(args.shared_depots, SharedDepotsMode::Skip);
                assert_eq!(args.language, Language::One("english".to_string()));
            }
            Command::Item(_) => panic!("expected app subcommand"),
        }
    }

    #[test]
    fn shared_depots_is_only_accepted_by_app() {
        assert!(
            Cli::try_parse_from([
                "steam-vent-cdn",
                "item",
                "4000",
                "1",
                "--shared-depots",
                "auto"
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_language_option() {
        assert_eq!("all".parse::<Language>().unwrap().as_filter(), None);
        assert_eq!(
            "French".parse::<Language>().unwrap().as_filter(),
            Some("french")
        );
        assert!("".parse::<Language>().is_err());
        assert!("   ".parse::<Language>().is_err());
    }

    #[test]
    fn detect_maps_the_host_os() {
        let expected = match std::env::consts::OS {
            "windows" => Some(Platform::Windows),
            "macos" => Some(Platform::Macos),
            "linux" => Some(Platform::Linux),
            _ => None,
        };
        assert_eq!(Platform::detect(), expected);
    }
}
