use thiserror::Error;

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum SteamSessionError {
    #[error("Steam server discovery failed: {0}")]
    ServerDiscovery(#[from] steam_vent::ServerDiscoveryError),

    #[error("Steam session initialization failed: {0}")]
    SessionInit(String),

    #[error("Steam login failed: {0}")]
    LoginFailed(String),

    #[error("Steam API call failed")]
    ApiCallFailed(#[source] Box<steam_vent::NetworkError>),
}

impl From<steam_vent::NetworkError> for SteamSessionError {
    fn from(err: steam_vent::NetworkError) -> Self {
        Self::ApiCallFailed(Box::new(err))
    }
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum DownloadError {
    #[error("No CDN servers available")]
    NoServers,

    #[error("All CDN servers failed")]
    AllServersFailed,

    #[error("All download attempts failed")]
    AllAttemptsFailed,

    #[error("Failed to resolve CDN hosts")]
    HostResolutionFailed,

    #[error("HTTP error: {status}")]
    HttpError { status: reqwest::StatusCode },

    #[error("Failed to get manifest request code")]
    ManifestRequestCodeFailed,

    #[error("Failed to get depot decryption key: EResult {0}")]
    DepotKeyFailed(i32),

    #[error("Invalid configuration: {0}")]
    InvalidConfiguration(String),

    #[error("Download semaphore closed")]
    SemaphoreClosed,

    #[error("Incomplete download: {received}/{total} chunks for {filename}")]
    IncompleteDownload {
        filename: String,
        received: usize,
        total: usize,
    },

    #[error("Gap in chunks: offset {offset} is not covered by any chunk")]
    GapInChunks { offset: u64 },
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum CompressionError {
    #[error("Zstd decompression failed")]
    Zstd(#[from] std::io::Error),

    #[error("LZMA/VZa decompression failed")]
    Lzma(#[from] lzma_rs::error::Error),

    #[error("ZIP extraction failed")]
    Zip(#[from] zip::result::ZipError),

    #[error("Invalid compression data: {0}")]
    InvalidData(&'static str),

    #[error("Invalid magic: expected {expected:02X?}")]
    InvalidMagic { expected: &'static [u8] },
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum ChunkError {
    #[error("Chunk size mismatch: expected {expected} bytes, got {actual} bytes")]
    SizeMismatch { expected: u32, actual: u32 },

    #[error("Chunk checksum mismatch: expected Adler32 {expected:#010X}, got {actual:#010X}")]
    ChecksumMismatch { expected: u32, actual: u32 },

    #[error("Unexpected chunk state: expected {expected}, got {actual}")]
    UnexpectedState {
        expected: &'static str,
        actual: &'static str,
    },
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum EncryptionError {
    #[error("Key initialization failed")]
    KeyInit(#[source] aes::cipher::InvalidLength),

    #[error("IV initialization failed")]
    IvInit(#[source] aes::cipher::InvalidLength),

    #[error("Decryption failed: {0}")]
    Decrypt(String),

    #[error("Unpadding failed")]
    Unpadding,
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum ManifestError {
    #[error("Failed to decode manifest: {0}")]
    Decode(String),

    #[error("Segment length {length} exceeds remaining data {remaining}")]
    SegmentLengthExceeded { length: usize, remaining: usize },

    #[error("Unknown segment magic: {0:#010X}")]
    UnknownSegmentMagic(u32),

    #[error("No Payload segment found in manifest")]
    MissingPayload,

    #[error("Missing field in manifest: {0}")]
    MissingField(&'static str),

    #[error("'{name}' is absolute or escapes the output directory")]
    UnsafePath { name: String },
}

// EResult values: https://steamerrors.com/
#[derive(Debug)]
pub enum WorkshopResult {
    Unknown,
    Ok,
    FileNotFound,
    AccessDenied,
    Other(u32),
}

impl From<Option<u32>> for WorkshopResult {
    fn from(value: Option<u32>) -> Self {
        match value {
            None => Self::Unknown,
            Some(1) => Self::Ok,
            Some(9) => Self::FileNotFound,
            Some(15) => Self::AccessDenied,
            Some(n) => Self::Other(n),
        }
    }
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum WorkshopError {
    #[error("GetDetails failed for item {item_id}: {result:?}")]
    GetDetailsFailed {
        item_id: u64,
        result: WorkshopResult,
    },

    #[error("Item details for {0} is missing or empty")]
    ItemDetailsMissing(u64),

    #[error("Item is not a Steam Workshop item")]
    NotAWorkshopItem,

    #[error("No manifest ID (hcontent_file) found")]
    MissingManifestId,

    #[error("consumer_appid is missing in item details")]
    MissingConsumerAppId,

    #[error("No versions found")]
    NoVersionsFound,

    #[error("Version {0} not found")]
    VersionNotFound(String),

    #[error("Either ITEM_ID or --slug must be provided")]
    MissingItemId,

    #[error("No workshop item found with slug '{0}'")]
    ItemNotFoundBySlug(String),

    #[error("Multiple workshop items match slug '{0}' ({1} total). Use item ID to disambiguate.")]
    AmbiguousSlug(String, u32),

    #[error("item {0} is delivered as a web file and cannot be installed with the Steam layout")]
    WebItemNotSupported(u64),
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum VdfError {
    #[error("unexpected end of VDF input at offset {offset}")]
    UnexpectedEof { offset: usize },

    #[error("unknown VDF type byte {value:#04X} at offset {offset}")]
    UnknownType { value: u8, offset: usize },

    #[error("invalid UTF-8 in VDF string at offset {offset}")]
    InvalidUtf8 { offset: usize },

    #[error("invalid UTF-16 in VDF string at offset {offset}")]
    InvalidUtf16 { offset: usize },

    #[error("trailing data after the VDF document: {remaining} bytes")]
    TrailingData { remaining: usize },

    #[error("expected a VDF object but found {found}")]
    ExpectedObject { found: &'static str },

    #[error("missing VDF key '{path}'")]
    MissingKey { path: &'static str },

    #[error("VDF document nests deeper than {limit} levels")]
    NestingTooDeep { limit: usize },
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum AppError {
    #[error("PICS returned no app info for app {0}")]
    UnknownAppId(u32),

    #[error("PICS app info for app {0} is empty")]
    EmptyAppInfo(u32),

    #[error("unsupported host OS '{0}'; pass --os windows|macos|linux|all")]
    UnsupportedHostOs(&'static str),

    #[error("app {0} requires an access token; log in with an account that owns it")]
    AccessTokenRequired(u32),

    #[error("app {0} has no build id for the public branch")]
    NoPublicBuild(u32),

    #[error("app {0} has no usable depots")]
    NoDepots(u32),

    #[error("app {0} has no depots matching the selected platform and language")]
    NoDepotsForSelection(u32),

    #[error("app {0} has no install directory (config.installdir) required for the Steam layout")]
    MissingInstallDir(u32),

    #[error("app {app_id} has an unsafe install directory '{name}'")]
    UnsafeInstallDir { app_id: u32, name: String },

    #[error("app {app_id} does not provide shared depot {depot_id} (not installed or not public)")]
    SharedDepotUnavailable { app_id: u32, depot_id: u32 },

    #[error("resolving shared depots exceeded {0} levels; check for a cyclic dependency")]
    SharedDepotDepth(u32),

    #[error(
        "app {app_id} has no depot {depot_id} (--manifest override does not match any known depot)"
    )]
    UnknownDepot { app_id: u32, depot_id: u32 },

    #[error("duplicate --manifest override for depot {0}")]
    DuplicateManifestOverride(u32),
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum SteamVentError {
    #[error("Steam session error")]
    SteamSession(#[from] SteamSessionError),

    #[error("Download error")]
    Download(#[from] DownloadError),

    #[error("Compression error")]
    Compression(#[from] CompressionError),

    #[error("Encryption error")]
    Encryption(#[from] EncryptionError),

    #[error("Chunk error")]
    Chunk(#[from] ChunkError),

    #[error("Network error")]
    Network(#[from] reqwest::Error),

    #[error("Manifest error")]
    Manifest(#[from] ManifestError),

    #[error("Workshop error")]
    Workshop(#[from] WorkshopError),

    #[error("VDF parse error")]
    Vdf(#[from] VdfError),

    #[error("App info error")]
    AppInfo(#[from] AppError),

    #[error("IO error")]
    Io(#[from] std::io::Error),

    #[error("Internal task panicked: {0}")]
    InternalPanic(String),
}

pub type Result<T> = std::result::Result<T, SteamVentError>;
