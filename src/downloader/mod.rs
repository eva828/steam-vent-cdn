#![allow(clippy::module_inception)]
mod http;
pub mod downloader;
pub mod models;
pub(crate) mod workers;

pub use downloader::CdnDownloader;
pub use models::{
    ChunkInfo, DepotContext, DownloadItem, DownloadPipeline, FileByteStream, ProgressCallback,
    VersionInfo,
};
