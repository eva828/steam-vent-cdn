use std::sync::Arc;

use crate::connection::SteamSession;
use crate::downloader::cdn::http::CdnFetcher;
use crate::downloader::cdn::servers::ContentServerList;
use crate::downloader::manifest::ManifestEntry;
use crate::downloader::random_access::CdnRandomAccessReader;
use crate::downloader::web::HttpFetcher;
use crate::downloader::{DownloadPipeline, ProgressCallback};
use crate::error::Result;

pub struct CdnDownloader {
    pub(crate) steam_session: SteamSession,
    pub(crate) http_fetcher: CdnFetcher,
    pub(crate) web_fetcher: HttpFetcher,
    pub(crate) content_servers: ContentServerList,
    pub(crate) download_sem: Arc<tokio::sync::Semaphore>,
}

impl CdnDownloader {
    pub async fn connect(steam_session: SteamSession) -> Result<Self> {
        let config = steam_session.config().clone();
        let parallel_download = steam_session.config().parallel_download;
        let content_servers = ContentServerList::discover(&steam_session).await?;
        Ok(Self {
            steam_session,
            http_fetcher: CdnFetcher::new(config.clone())?,
            web_fetcher: HttpFetcher::new(config)?,
            content_servers,
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

    pub async fn download_web_file(&self, url: &str) -> Result<Vec<u8>> {
        self.web_fetcher.download_file(url).await
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
