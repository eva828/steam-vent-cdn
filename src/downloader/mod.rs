pub mod cdn;
pub mod client;
pub mod manifest;
pub mod progress;
pub mod random_access;
pub mod sequential;

pub use cdn::DepotContext;
pub use client::CdnDownloader;
pub use manifest::{ChunkInfo, Manifest, ManifestEntry};
pub use progress::ProgressCallback;
pub use random_access::CdnRandomAccessReader;
pub use sequential::{DownloadPipeline, FileByteStream};

pub use steam_api::{ItemDetails, VersionInfo};

pub(crate) mod archive;
pub(crate) mod steam_api;
