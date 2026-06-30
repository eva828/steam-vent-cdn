pub mod cdn;
pub mod client;
pub mod item;
pub mod manifest;
pub mod pipeline;
pub mod reader;

pub use cdn::DepotContext;
pub use client::CdnDownloader;
pub use item::{ChunkInfo, DownloadItem, classify_items};
pub use pipeline::{DownloadPipeline, FileByteStream, ProgressCallback};
pub use reader::CdnRandomAccessReader;

// VersionInfo is used by CLI, so re-export it
pub use steam_api::VersionInfo;

pub(crate) mod archive;
pub(crate) mod steam_api;
