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

impl DownloadItem {
    pub fn is_directory(&self) -> bool {
        match self.flags {
            Some(flags) => flags & FLAG_DIRECTORY != 0,
            None => {
                tracing::warn!(
                    "flags field is missing for item '{}', treating as file",
                    self.filename
                );
                false
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ManifestItems {
    pub directories: Vec<DownloadItem>,
    pub files: Vec<DownloadItem>,
}

impl ManifestItems {
    pub fn classify(items: Vec<DownloadItem>) -> Self {
        let mut directories = Vec::new();
        let mut files = Vec::new();

        for item in items {
            if item.is_directory() {
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

        Self { directories, files }
    }
}
