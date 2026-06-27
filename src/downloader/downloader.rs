use std::sync::Arc;

use crate::connection::{ConnectionExt, SteamSession};
use crate::error::{CdnError, Result, SteamSessionError, SteamVentError, WorkshopError};
use steam_vent::ConnectionTrait;
use steam_vent_proto::steammessages_clientserver_2::{
    CMsgClientGetDepotDecryptionKey, CMsgClientGetDepotDecryptionKeyResponse,
};
use steam_vent_proto::steammessages_contentsystem_steamclient::CContentServerDirectory_GetManifestRequestCode_Request;
use steam_vent_proto::steammessages_publishedfile_steamclient::{
    CPublishedFile_GetChangeHistory_Request, CPublishedFile_GetDetails_Request,
    PublishedFileDetails,
};

use super::http::HttpFetcher;

use super::models::{
    DecodedChunk, DepotContext, DownloadItem, DownloadManifest, DownloadPipeline, FileByteStream,
    ProgressCallback, RawChunk, VersionInfo,
};
use super::servers::CdnServerList;
use super::workers;

pub struct CdnDownloader {
    steam_client: SteamSession,
    http_fetcher: HttpFetcher,
    cdn_servers: CdnServerList,
}

impl CdnDownloader {
    pub fn new(steam_client: SteamSession) -> Result<Self> {
        let config = steam_client.config().clone();
        Ok(Self {
            steam_client,
            http_fetcher: HttpFetcher::new(config)?,
            cdn_servers: CdnServerList::new(),
        })
    }

    pub(crate) async fn fetch_item_details(
        &mut self,
        item_id: u64,
    ) -> Result<PublishedFileDetails> {
        let details_req = CPublishedFile_GetDetails_Request {
            publishedfileids: vec![item_id],
            ..Default::default()
        };

        let max_retries = self.steam_client.max_retries();
        let retry_delay = self.steam_client.retry_delay();

        let details_resp = self
            .steam_client
            .connection_mut()
            .service_method_with_retry(details_req, max_retries, retry_delay)
            .await?;

        if details_resp.publishedfiledetails.is_empty() {
            return Err(SteamVentError::Workshop(WorkshopError::ItemDetailsMissing(
                item_id,
            )));
        }
        Ok(details_resp.publishedfiledetails[0].clone())
    }

    /// Fetch a specific range of the change history for a workshop item.
    ///
    /// For fetching all versions, see [`fetch_all_change_history`].
    ///
    /// # Arguments
    ///
    /// * `item_id` - The Steam Workshop item ID
    /// * `range` - The range of versions to fetch (e.g., `0..100`)
    pub async fn fetch_change_history(
        &mut self,
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

        let history_resp = self
            .steam_client
            .connection_mut()
            .service_method(history_req)
            .await
            .map_err(|e| SteamSessionError::ApiCallFailed(Box::new(e)))
            .map_err(SteamVentError::SteamSession)?;

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

    /// Fetch all change history for a workshop item.
    ///
    /// This method automatically handles pagination to fetch all available versions
    /// by making multiple API calls if necessary.
    ///
    /// # Arguments
    ///
    /// * `item_id` - The Steam Workshop item ID
    pub async fn fetch_all_change_history(&mut self, item_id: u64) -> Result<Vec<VersionInfo>> {
        let mut all_versions = Vec::new();
        let mut start_index = 0u32;

        loop {
            let versions = self
                .fetch_change_history(item_id, start_index..(start_index + 100))
                .await?;
            let count = versions.len() as u32;

            if count == 0 && all_versions.is_empty() {
                return Err(SteamVentError::Workshop(WorkshopError::NoVersionsFound));
            }

            all_versions.extend(versions);

            // If we got fewer than 100 items, we've reached the end
            if count < 100 {
                break;
            }

            start_index += count;
        }

        tracing::debug!("Total versions fetched: {}", all_versions.len());
        Ok(all_versions)
    }

    async fn resolve_cdn_hosts(&mut self) -> Result<()> {
        if !self.cdn_servers.is_empty() {
            return Ok(());
        }
        self.cdn_servers = CdnServerList::discover(&mut self.steam_client).await?;
        Ok(())
    }

    pub(crate) async fn fetch_manifest_request_code(
        &mut self,
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
            .steam_client
            .connection_mut()
            .service_method(code_req)
            .await
            .map_err(|e| SteamSessionError::ApiCallFailed(Box::new(e)))
            .map_err(SteamVentError::SteamSession)?;
        code_resp
            .manifest_request_code
            .ok_or_else(|| SteamVentError::Cdn(CdnError::ManifestRequestCodeFailed))
    }

    pub(crate) async fn fetch_depot_key(&mut self, app_id: u32, depot_id: u32) -> Result<Vec<u8>> {
        tracing::debug!("Requesting depot decryption key");

        let mut key_req = CMsgClientGetDepotDecryptionKey::new();
        key_req.set_depot_id(depot_id);
        key_req.set_app_id(app_id);

        let key_resp: CMsgClientGetDepotDecryptionKeyResponse = self
            .steam_client
            .connection_mut()
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

    pub(crate) async fn fetch_manifest_by_id(
        &mut self,
        manifest_id: u64,
        app_id: u32,
    ) -> Result<DownloadManifest> {
        self.resolve_cdn_hosts().await?;
        if self.cdn_servers.is_empty() {
            return Err(SteamVentError::Cdn(CdnError::HostResolutionFailed));
        }

        let depot_id = app_id;

        tracing::debug!("Manifest ID: {}", manifest_id);
        tracing::debug!("App/Depot ID: {}", app_id);

        let mrc = self
            .fetch_manifest_request_code(app_id, depot_id, manifest_id)
            .await?;
        tracing::debug!("Manifest Request Code: {}", mrc);

        let manifest_bytes = self
            .http_fetcher
            .download_manifest(&self.cdn_servers, depot_id, manifest_id, mrc)
            .await?;

        let (mut manifest, metadata) = crate::manifest::parse_manifest(&manifest_bytes)?;

        let mut depot_key_opt = None;
        let mut need_key_for_filenames = false;
        let mut depot_id_for_key = app_id;

        if let Some(ref m) = metadata {
            let filenames_encrypted = m.filenames_encrypted.ok_or_else(|| {
                SteamVentError::Manifest(crate::error::ManifestError::MissingField(
                    "metadata.filenames_encrypted",
                ))
            })?;
            if filenames_encrypted {
                need_key_for_filenames = true;
                depot_id_for_key = m.depot_id.ok_or_else(|| {
                    SteamVentError::Manifest(crate::error::ManifestError::MissingField(
                        "metadata.depot_id",
                    ))
                })?;
                tracing::debug!(
                    "Manifest filenames are encrypted (depot_id: {}).",
                    depot_id_for_key
                );
            }
        }

        match self.fetch_depot_key(app_id, depot_id_for_key).await {
            Ok(depot_key) => {
                tracing::debug!("Depot key received ({} bytes).", depot_key.len());
                if need_key_for_filenames {
                    crate::manifest::decrypt_manifest_filenames(&mut manifest, &depot_key)?;
                    tracing::debug!("Decrypted manifest filenames.");
                }
                depot_key_opt = Some(depot_key);
            }
            Err(e) => {
                if need_key_for_filenames {
                    tracing::error!("Failed to fetch depot key for filename decryption: {}", e);
                    return Err(e);
                } else {
                    tracing::debug!("No depot key fetched (not needed).");
                }
            }
        }

        let depot = Arc::new(DepotContext {
            server_hosts: self.cdn_servers.clone(),
            depot_id,
            depot_key: depot_key_opt,
        });

        Ok(DownloadManifest { manifest, depot })
    }

    /// Fetch the latest manifest ID for a workshop item.
    pub async fn fetch_latest_manifest_id(&mut self, item_id: u64) -> Result<u64> {
        let details = self.fetch_item_details(item_id).await?;
        details
            .hcontent_file
            .ok_or_else(|| SteamVentError::Workshop(WorkshopError::MissingManifestId))
    }

    /// Fetch manifest info for a specific manifest ID.
    pub async fn fetch_manifest_info(
        &mut self,
        item_id: u64,
        manifest_id: u64,
    ) -> Result<Vec<DownloadItem>> {
        let details = self.fetch_item_details(item_id).await?;

        let title = details.title.as_deref().unwrap_or("Unknown");
        tracing::info!("Title: {}", title);

        let app_id = details
            .consumer_appid
            .ok_or_else(|| SteamVentError::Workshop(WorkshopError::MissingConsumerAppId))?;

        let manifest_data = self.fetch_manifest_by_id(manifest_id, app_id).await?;
        manifest_data.into_download_items()
    }

    /// Create a [`DownloadPipeline`] that can download individual files.
    ///
    /// The pipeline shares a download semaphore so the total number of
    /// concurrent HTTP chunk downloads is bounded by `parallel_download`
    /// across all `download_file` calls.
    pub fn create_pipeline(&self, progress: Arc<dyn ProgressCallback>) -> DownloadPipeline {
        let parallel_download = self.steam_client.config().parallel_download;
        DownloadPipeline {
            http_fetcher: self.http_fetcher.clone(),
            download_sem: Arc::new(tokio::sync::Semaphore::new(parallel_download)),
            progress,
            parallel_download,
        }
    }
}

impl DownloadPipeline {
    /// Download a single file and return a [`FileByteStream`] providing its
    /// bytes in sequential order.
    ///
    /// Internally spawns a 3-stage pipeline (download → decode → reassemble)
    /// dedicated to this file.  The returned stream owns all worker tasks;
    /// dropping it aborts them.
    ///
    /// Multiple `download_file` calls share the same download semaphore, so
    /// the total concurrent HTTP chunk downloads stays bounded.
    pub fn download_file(&self, item: &DownloadItem) -> Result<FileByteStream> {
        let parallel_download = self.parallel_download;

        let download_channel_size = (parallel_download * 2).max(64);
        let logical_cores = match std::thread::available_parallelism() {
            Ok(n) => n.get(),
            Err(e) => {
                tracing::warn!(
                    "Failed to detect CPU parallelism ({}), falling back to 8 cores",
                    e
                );
                8
            }
        };
        let decode_channel_size = (logical_cores * 2).max(32);
        let output_channel_size = decode_channel_size;

        // Stage 1 → 2: raw (encrypted+compressed) chunks
        let (download_tx, download_rx) =
            tokio::sync::mpsc::channel::<Result<RawChunk>>(download_channel_size);
        // Stage 2 → 3: decoded (decrypted+decompressed) chunks
        let (decoded_tx, decoded_rx) =
            tokio::sync::mpsc::channel::<Result<DecodedChunk>>(decode_channel_size);
        // Stage 3 → consumer: reassembled bytes in offset order
        let (output_tx, output_rx) =
            tokio::sync::mpsc::channel::<Result<Vec<u8>>>(output_channel_size);

        let mut handles = tokio::task::JoinSet::new();

        // Stage 1: spawn chunk downloads (returns immediately)
        workers::spawn_chunk_downloads(
            &self.http_fetcher,
            &self.download_sem,
            &self.progress,
            item,
            download_tx,
        );

        // Stage 2: decode worker
        let progress_decode = Arc::clone(&self.progress);
        handles.spawn(async move {
            workers::run_decode_loop(download_rx, decoded_tx, progress_decode).await;
        });

        // Stage 3: reassembly worker
        let progress_reassembly = Arc::clone(&self.progress);
        let filename = item.filename.clone();
        let total_chunks = item.chunks.len();
        handles.spawn(async move {
            workers::run_reassembly_loop(
                decoded_rx,
                output_tx,
                progress_reassembly,
                filename,
                total_chunks,
            )
            .await;
        });

        Ok(FileByteStream::new(
            item.filename.clone(),
            item.size,
            output_rx,
            handles,
        ))
    }
}
