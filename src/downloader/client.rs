use std::sync::Arc;

use crate::connection::SteamSession;
use crate::downloader::cdn::http::HttpFetcher;
use crate::downloader::cdn::servers::CdnServerList;
use crate::downloader::manifest::ManifestEntry;
use crate::downloader::random_access::CdnRandomAccessReader;
use crate::downloader::{DownloadPipeline, ProgressCallback};
use crate::error::Result;

pub struct CdnDownloader {
    pub(crate) steam_session: SteamSession,
    pub(crate) http_fetcher: HttpFetcher,
    pub(crate) cdn_servers: CdnServerList,
    pub(crate) download_sem: Arc<tokio::sync::Semaphore>,
}

impl CdnDownloader {
    pub async fn connect(steam_session: SteamSession) -> Result<Self> {
        let config = steam_session.config().clone();
        let parallel_download = steam_session.config().parallel_download;
        let cdn_servers = CdnServerList::discover(&steam_session).await?;
        Ok(Self {
            steam_session,
            http_fetcher: HttpFetcher::new(config)?,
            cdn_servers,
            download_sem: Arc::new(tokio::sync::Semaphore::new(parallel_download)),
        })
    }

    pub fn create_pipeline(&self, progress: Arc<dyn ProgressCallback>) -> DownloadPipeline {
        let parallel_download = self.steam_session.config().parallel_download;
        DownloadPipeline {
            http_fetcher: self.http_fetcher.clone(),
            download_sem: Arc::clone(&self.download_sem),
            progress,
            parallel_download,
        }
    }

    pub fn create_reader(
        &self,
        entry: &ManifestEntry,
        progress: Arc<dyn ProgressCallback>,
    ) -> CdnRandomAccessReader {
        CdnRandomAccessReader::new(
            entry,
            self.http_fetcher.clone(),
            Arc::clone(&self.download_sem),
            progress,
        )
    }
}
