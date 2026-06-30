pub mod cdn;
pub mod client;
pub mod item;
pub mod manifest;
pub mod random_access;
pub mod sequential;

pub use cdn::DepotContext;
pub use client::CdnDownloader;
pub use item::{ChunkInfo, DownloadItem, classify_items};
pub use random_access::CdnRandomAccessReader;
pub use sequential::{DownloadPipeline, FileByteStream, ProgressCallback};

// VersionInfo is used by CLI, so re-export it
pub use steam_api::VersionInfo;

pub(crate) mod archive;
pub(crate) mod steam_api;
