use crate::connection::ConnectionExt;
use crate::downloader::client::CdnDownloader;
use crate::error::{CdnError, Result, SteamSessionError, SteamVentError, WorkshopError};
use std::pin::Pin;

use async_stream::try_stream;
use futures::Stream;
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
    pub description: String,
    pub consumer_appid: Option<u32>,
    pub manifest_id: Option<u64>,
    pub preview_url: Option<String>,
    pub file_url: Option<String>,
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
            description: details.file_description.unwrap_or_default(),
            consumer_appid: details.consumer_appid,
            manifest_id: details.hcontent_file,
            preview_url: details.preview_url.clone(),
            file_url: details.file_url.clone(),
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
            return Err(SteamVentError::Workshop(WorkshopError::ItemDetailsMissing(
                item_id,
            )));
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

    pub fn fetch_versions<'a>(
        &'a self,
        item_id: u64,
        max: Option<u32>,
    ) -> Pin<Box<dyn Stream<Item = Result<VersionInfo>> + Send + 'a>> {
        Box::pin(try_stream! {
            if max == Some(0) {
                return;
            }

            const PAGE_SIZE: u32 = 100;
            let mut offset = 0u32;

            while max.map_or(true, |m| offset < m) {
                let request_count = max
                    .map(|m| (m - offset).min(PAGE_SIZE))
                    .unwrap_or(PAGE_SIZE);

                let versions = self
                    .fetch_change_history(item_id, offset..(offset + request_count))
                    .await?;
                let count = versions.len() as u32;

                for version in versions {
                    yield version;
                }

                if count < request_count {
                    break;
                }

                offset += count;
            }
        })
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
        code_resp
            .manifest_request_code
            .ok_or_else(|| SteamVentError::Cdn(CdnError::ManifestRequestCodeFailed))
    }

    pub(crate) async fn fetch_depot_key(&self, app_id: u32, depot_id: u32) -> Result<Vec<u8>> {
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
            return Err(SteamVentError::Cdn(CdnError::DepotKeyFailed(
                key_resp.eresult(),
            )));
        }
        Ok(key_resp.depot_encryption_key().to_vec())
    }
}
