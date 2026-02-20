use thiserror::Error;

#[derive(Error, Debug)]
pub enum SteamVentError {
    #[error("Steam connection error: {0}")]
    Connection(String),

    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("CDN error: {0}")]
    Cdn(String),

    #[error("Manifest error: {0}")]
    Manifest(String),

    #[error("Decryption error: {0}")]
    Decryption(String),

    #[error("Invalid data: {0}")]
    InvalidData(String),

    #[error("Anyhow error: {0}")]
    Anyhow(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, SteamVentError>;
