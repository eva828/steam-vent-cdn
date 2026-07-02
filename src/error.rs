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
pub enum CdnError {
    #[error("No CDN servers available")]
    NoServers,

    #[error("All CDN servers failed")]
    AllServersFailed,

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
    InvalidLzmaHeader(&'static str),
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
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum WorkshopError {
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
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum SteamVentError {
    #[error("Steam session error")]
    SteamSession(#[from] SteamSessionError),

    #[error("CDN error")]
    Cdn(#[from] CdnError),

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

    #[error("IO error")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SteamVentError>;
