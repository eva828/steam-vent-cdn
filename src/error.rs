use thiserror::Error;

#[derive(Error, Debug)]
pub enum SteamConnectionError {
    #[error("Steam server discovery failed: {0}")]
    ServerDiscovery(#[from] steam_vent::ServerDiscoveryError),

    #[error("Steam session initialization failed: {0}")]
    SessionInit(String),

    #[error("Steam login failed: {0}")]
    LoginFailed(String),

    #[error("Steam API call failed")]
    ApiCallFailed(#[source] Box<steam_vent::NetworkError>),
}

impl From<steam_vent::NetworkError> for SteamConnectionError {
    fn from(err: steam_vent::NetworkError) -> Self {
        Self::ApiCallFailed(Box::new(err))
    }
}

#[derive(Error, Debug)]
pub enum CdnConnectionError {
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
}

#[derive(Error, Debug)]
pub enum CompressionError {
    #[error("Zstd decompression failed")]
    Zstd(#[from] std::io::Error),

    #[error("LZMA/VZa decompression failed")]
    Lzma(#[from] lzma_rs::error::Error),

    #[error("ZIP extraction failed")]
    Zip(#[from] zip::result::ZipError),

    #[error("Invalid compression data: {0}")]
    InvalidData(String),
}

#[derive(Error, Debug)]
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
pub enum ManifestError {
    #[error("Failed to decode manifest: {0}")]
    Decode(String),

    #[error("Manifest error: {0}")]
    Custom(String),
}

#[derive(Error, Debug)]
pub enum SteamVentError {
    #[error("Steam connection error")]
    Connection(#[from] SteamConnectionError),

    #[error("CDN connection error")]
    CdnConnection(#[from] CdnConnectionError),

    #[error("Compression error")]
    Compression(#[from] CompressionError),

    #[error("Encryption error")]
    Encryption(#[from] EncryptionError),

    #[error("Network error")]
    Network(#[from] reqwest::Error),

    #[error("Manifest error")]
    Manifest(#[from] ManifestError),

    #[error("Invalid data: {0}")]
    InvalidData(String),

    #[error("IO error")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SteamVentError>;
