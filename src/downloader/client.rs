use crate::connection::SteamSession;
use crate::downloader::cdn::{CdnServerList, HttpFetcher};
use crate::error::Result;

pub struct CdnDownloader {
    pub(crate) steam_session: SteamSession,
    pub(crate) http_fetcher: HttpFetcher,
    pub(crate) cdn_servers: CdnServerList,
}

impl CdnDownloader {
    pub fn new(steam_session: SteamSession) -> Result<Self> {
        let config = steam_session.config().clone();
        Ok(Self {
            steam_session,
            http_fetcher: HttpFetcher::new(config)?,
            cdn_servers: CdnServerList::new(),
        })
    }
}
