use crate::connection::ConnectionExt;
use crate::downloader::client::CdnDownloader;
use crate::error::{DownloadError, Result, SteamSessionError, SteamVentError, WorkshopError};

use futures::Stream;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::vec::IntoIter;
use steam_vent::ConnectionTrait;
use steam_vent_proto::steammessages_clientserver_2::{
    CMsgClientGetDepotDecryptionKey, CMsgClientGetDepotDecryptionKeyResponse,
};
use steam_vent_proto::steammessages_contentsystem_steamclient::CContentServerDirectory_GetManifestRequestCode_Request;
use steam_vent_proto::steammessages_publishedfile_steamclient::{
    CPublishedFile_GetChangeHistory_Request, CPublishedFile_GetDetails_Request,
    PublishedFileDetails,
};

#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub timestamp: u32,
    pub manifest_id: u64,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct ItemDetails {
    pub publishedfileid: u64,
    pub title: String,
    pub description: Option<String>,
    pub consumer_appid: Option<u32>,
    pub manifest_id: Option<u64>,
    pub preview_url: Option<String>,
    pub file_url: Option<String>,
    pub filename: Option<String>,
    pub time_created: Option<u32>,
    pub time_updated: Option<u32>,
    pub creator: Option<u64>,
    pub tags: Vec<String>,
}

impl From<PublishedFileDetails> for ItemDetails {
    fn from(details: PublishedFileDetails) -> Self {
        Self {
            publishedfileid: details.publishedfileid(),
            title: details.title.unwrap_or_default(),
            description: details.file_description,
            consumer_appid: details.consumer_appid,
            manifest_id: details.hcontent_file,
            preview_url: details.preview_url.clone(),
            file_url: details.file_url.clone(),
            filename: details.filename.clone(),
            time_created: details.time_created,
            time_updated: details.time_updated,
            creator: details.creator,
            tags: details.tags.into_iter().filter_map(|t| t.tag).collect(),
        }
    }
}

impl CdnDownloader {
    pub async fn fetch_item_details(&self, item_id: u64) -> Result<ItemDetails> {
        let details_req = CPublishedFile_GetDetails_Request {
            publishedfileids: vec![item_id],
            ..Default::default()
        };

        let config = self.steam_session.config();
        let max_retries = config.retry_policy.max_retries;
        let retry_delay = config.retry_policy.retry_delay;

        let details_resp = self
            .steam_session
            .connection()
            .service_method_with_retry(details_req, max_retries, retry_delay)
            .await?;

        if details_resp.publishedfiledetails.is_empty() {
            return Err(WorkshopError::ItemDetailsMissing(item_id))?;
        }

        Ok(details_resp.publishedfiledetails[0].clone().into())
    }

    pub async fn fetch_change_history(
        &self,
        item_id: u64,
        range: std::ops::Range<u32>,
    ) -> Result<Vec<VersionInfo>> {
        let start_index = range.start;
        let count = range.end.saturating_sub(range.start);

        tracing::debug!(
            "Fetching change history for item {} (start_index={}, count={})",
            item_id,
            start_index,
            count
        );

        let mut history_req = CPublishedFile_GetChangeHistory_Request::new();
        history_req.set_publishedfileid(item_id);
        history_req.set_startindex(start_index);
        history_req.set_count(count);

        let config = self.steam_session.config();
        let max_retries = config.retry_policy.max_retries;
        let retry_delay = config.retry_policy.retry_delay;

        let history_resp = self
            .steam_session
            .connection()
            .service_method_with_retry(history_req, max_retries, retry_delay)
            .await?;

        let versions: Vec<VersionInfo> = history_resp
            .changes
            .into_iter()
            .map(|change| {
                let timestamp = change.timestamp();
                let manifest_id = change.manifest_id();
                let description = change.change_description().to_string();
                VersionInfo {
                    timestamp,
                    manifest_id,
                    description,
                }
            })
            .collect();

        tracing::debug!(
            "Fetched {} versions at index {}",
            versions.len(),
            start_index
        );

        Ok(versions)
    }
}

const VERSION_PAGE_SIZE: u32 = 100;

type VersionPageFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<VersionInfo>>> + Send + 'a>>;

struct VersionStream<'a> {
    downloader: &'a CdnDownloader,
    item_id: u64,
    max: Option<u32>,
    offset: u32,
    buffer: IntoIter<VersionInfo>,
    pending: Option<VersionPageFuture<'a>>,
    exhausted: bool,
}

impl<'a> Stream for VersionStream<'a> {
    type Item = Result<VersionInfo>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(version) = this.buffer.next() {
                return Poll::Ready(Some(Ok(version)));
            }

            if this.exhausted {
                return Poll::Ready(None);
            }

            match this.pending.as_mut() {
                Some(fut) => match fut.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => {
                        this.pending = None;
                        match result {
                            Ok(versions) => {
                                let count = versions.len() as u32;
                                this.buffer = versions.into_iter();
                                if count < VERSION_PAGE_SIZE {
                                    this.exhausted = true;
                                } else {
                                    this.offset += count;
                                }
                            }
                            Err(e) => {
                                this.exhausted = true;
                                return Poll::Ready(Some(Err(e)));
                            }
                        }
                    }
                },
                None => {
                    if this.max.is_some_and(|m| this.offset >= m) {
                        this.exhausted = true;
                        return Poll::Ready(None);
                    }

                    let request_count = this
                        .max
                        .map(|m| (m - this.offset).min(VERSION_PAGE_SIZE))
                        .unwrap_or(VERSION_PAGE_SIZE);
                    let range = this.offset..(this.offset + request_count);

                    this.pending = Some(Box::pin(
                        this.downloader.fetch_change_history(this.item_id, range),
                    ));
                }
            }
        }
    }
}

impl CdnDownloader {
    pub fn fetch_versions<'a>(
        &'a self,
        item_id: u64,
        max: Option<u32>,
    ) -> impl Stream<Item = Result<VersionInfo>> + Unpin + Send + 'a {
        VersionStream {
            downloader: self,
            item_id,
            max,
            offset: 0,
            buffer: Vec::new().into_iter(),
            pending: None,
            exhausted: false,
        }
    }

    pub(crate) async fn fetch_manifest_request_code(
        &self,
        app_id: u32,
        depot_id: u32,
        manifest_id: u64,
    ) -> Result<u64> {
        tracing::debug!("Requesting manifest request code");

        let mut code_req = CContentServerDirectory_GetManifestRequestCode_Request::new();
        code_req.set_app_id(app_id);
        code_req.set_depot_id(depot_id);
        code_req.set_manifest_id(manifest_id);
        code_req.set_app_branch("public".into());

        let code_resp = self
            .steam_session
            .connection()
            .service_method(code_req)
            .await
            .map_err(|e| SteamSessionError::ApiCallFailed(Box::new(e)))
            .map_err(SteamVentError::SteamSession)?;
        Ok(code_resp
            .manifest_request_code
            .ok_or(DownloadError::ManifestRequestCodeFailed)?)
    }

    pub(crate) async fn fetch_depot_key(&self, app_id: u32, depot_id: u32) -> Result<Vec<u8>> {
        let cache_key = (app_id, depot_id);
        if let Some(key) = self.depot_key_cache.lock().get(&cache_key).cloned() {
            tracing::debug!(
                "Depot key cache hit for app_id={}, depot_id={} ({} bytes).",
                app_id,
                depot_id,
                key.len()
            );
            return Ok(key);
        }

        tracing::debug!("Requesting depot decryption key");

        let mut key_req = CMsgClientGetDepotDecryptionKey::new();
        key_req.set_depot_id(depot_id);
        key_req.set_app_id(app_id);

        let key_resp: CMsgClientGetDepotDecryptionKeyResponse = self
            .steam_session
            .connection()
            .job(key_req)
            .await
            .map_err(|e| SteamSessionError::ApiCallFailed(Box::new(e)))
            .map_err(SteamVentError::SteamSession)?;
        if key_resp.eresult() != 1 {
            return Err(DownloadError::DepotKeyFailed(key_resp.eresult()))?;
        }
        let key = key_resp.depot_encryption_key().to_vec();
        self.depot_key_cache
            .lock()
            .insert(cache_key, key.clone());
        Ok(key)
    }
}
