use crate::downloader::cdn::DepotContext;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ChunkInfo {
    pub sha: Vec<u8>,
    pub crc: u32,
    pub offset: u64,
    pub uncompressed_size: u32,
    pub compressed_size: u32,
}

#[derive(Debug, Clone)]
pub struct DownloadItem {
    pub filename: String,
    pub chunks: Vec<ChunkInfo>,
    pub flags: Option<u32>,
    pub size: Option<u64>,
    pub depot: Arc<DepotContext>,
}

const FLAG_DIRECTORY: u32 = 0x40;

pub fn classify_items(items: Vec<DownloadItem>) -> (Vec<DownloadItem>, Vec<DownloadItem>) {
    let mut directories = Vec::new();
    let mut files = Vec::new();

    for item in items {
        let is_directory = match item.flags {
            Some(flags) => flags & FLAG_DIRECTORY != 0,
            None => {
                tracing::warn!(
                    "flags field is missing for item '{}', treating as file",
                    item.filename
                );
                false
            }
        };
        if is_directory {
            directories.push(item);
        } else {
            files.push(item);
        }
    }

    tracing::info!(
        "Found {} directories and {} files to download.",
        directories.len(),
        files.len()
    );

    (directories, files)
}
