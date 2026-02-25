use std::path::Path;
use std::sync::Arc;

use crate::cdn::CdnClient;
use crate::connection::{ConnectionExt, SteamSession};
use crate::error::{CdnConnectionError, Result, SteamConnectionError, SteamVentError};
use steam_vent::ConnectionTrait;
use steam_vent_proto::steammessages_clientserver_2::{
    CMsgClientGetDepotDecryptionKey, CMsgClientGetDepotDecryptionKeyResponse,
};
use steam_vent_proto::steammessages_contentsystem_steamclient::CContentServerDirectory_GetManifestRequestCode_Request;
use steam_vent_proto::steammessages_publishedfile_steamclient::{
    CPublishedFile_GetChangeHistory_Request, CPublishedFile_GetDetails_Request,
    PublishedFileDetails,
};

use super::models::{
    ChunkDownloadTask, CompressedChunk, DepotContext, DownloadItem, DownloadManifest,
    DownloadedChunk, ProgressCallback, VersionInfo,
};
use super::workers::{DecodeWorker, DownloadWorker, WriteWorker};

pub struct CdnDownloader {
    steam_client: SteamSession,
    pub(crate) cdn_client: CdnClient,
}

impl CdnDownloader {
    pub fn new(steam_client: SteamSession) -> Self {
        let config = steam_client.config().clone();
        Self {
            steam_client,
            cdn_client: CdnClient::new(config),
        }
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
            return Err(SteamVentError::InvalidData(format!(
                "No details found for item {}",
                item_id
            )));
        }
        Ok(details_resp.publishedfiledetails[0].clone())
    }

    pub async fn fetch_change_history(&mut self, item_id: u64) -> Result<Vec<VersionInfo>> {
        tracing::debug!("Fetching change history for item {}", item_id);

        let mut history_req = CPublishedFile_GetChangeHistory_Request::new();
        history_req.set_publishedfileid(item_id);
        history_req.set_count(100);

        let history_resp = self
            .steam_client
            .connection_mut()
            .service_method(history_req)
            .await
            .map_err(|e| SteamConnectionError::ApiCallFailed(Box::new(e)))
            .map_err(SteamVentError::Connection)?;

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

        if versions.is_empty() {
            return Err(SteamVentError::InvalidData(
                "No version history available for this item".to_string(),
            ));
        }

        tracing::debug!("Found {} versions", versions.len());
        Ok(versions)
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
            .map_err(|e| SteamConnectionError::ApiCallFailed(Box::new(e)))
            .map_err(SteamVentError::Connection)?;
        code_resp.manifest_request_code.ok_or_else(|| {
            SteamVentError::CdnConnection(CdnConnectionError::ManifestRequestCodeFailed)
        })
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
            .map_err(|e| SteamConnectionError::ApiCallFailed(Box::new(e)))
            .map_err(SteamVentError::Connection)?;
        if key_resp.eresult() != 1 {
            return Err(SteamVentError::CdnConnection(
                CdnConnectionError::DepotKeyFailed(key_resp.eresult()),
            ));
        }
        Ok(key_resp.depot_encryption_key().to_vec())
    }

    pub(crate) async fn fetch_manifest_by_id(
        &mut self,
        manifest_id: u64,
        app_id: u32,
    ) -> Result<DownloadManifest> {
        self.cdn_client
            .resolve_cdn_hosts(self.steam_client.connection_mut())
            .await?;
        let servers = self.cdn_client.cdn_hosts().to_vec();
        if servers.is_empty() {
            return Err(SteamVentError::CdnConnection(
                CdnConnectionError::HostResolutionFailed,
            ));
        }

        let depot_id = app_id;

        tracing::debug!("Manifest ID: {}", manifest_id);
        tracing::debug!("App/Depot ID: {}", app_id);

        let mrc = self
            .fetch_manifest_request_code(app_id, depot_id, manifest_id)
            .await?;
        tracing::debug!("Manifest Request Code: {}", mrc);

        let manifest_bytes = self
            .cdn_client
            .download_manifest(&servers, depot_id, manifest_id, mrc)
            .await?;

        let (mut manifest, metadata) = crate::manifest::parse_manifest(&manifest_bytes)?;

        let mut depot_key_opt = None;
        let mut need_key_for_filenames = false;
        let mut depot_id_for_key = app_id;

        if let Some(ref m) = metadata {
            if m.filenames_encrypted.unwrap_or(false) {
                need_key_for_filenames = true;
                depot_id_for_key = m.depot_id.unwrap_or(app_id);
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
            server_hosts: servers,
            depot_id,
            depot_key: depot_key_opt,
        });

        Ok(DownloadManifest { manifest, depot })
    }

    pub async fn fetch_manifest_info(&mut self, item_id: u64) -> Result<Vec<DownloadItem>> {
        let details = self.fetch_item_details(item_id).await?;

        let title = details.title.as_deref().unwrap_or("Unknown");
        tracing::info!("Title: {}", title);

        let manifest_id = details.hcontent_file.ok_or_else(|| {
            SteamVentError::InvalidData("No manifest ID (hcontent_file) found".to_string())
        })?;
        let config_app_id = self.steam_client.config().app_id;
        let app_id = details.consumer_appid.unwrap_or(config_app_id);

        let manifest_data = self.fetch_manifest_by_id(manifest_id, app_id).await?;
        manifest_data.into_download_items()
    }

    pub async fn download_to_directory(
        &mut self,
        files: &[DownloadItem],
        base_path: &Path,
        progress: Arc<dyn ProgressCallback>,
    ) -> Result<()> {
        let mut tasks: Vec<ChunkDownloadTask> = Vec::new();
        for (file_index, file) in files.iter().enumerate() {
            let depot = Arc::clone(&file.depot);
            for (chunk_index, chunk) in file.chunks.iter().enumerate() {
                tasks.push(ChunkDownloadTask {
                    chunk_id: hex::encode(&chunk.sha),
                    depot: Arc::clone(&depot),
                    file_index,
                    chunk_index,
                    compressed_size: chunk.compressed_size,
                    uncompressed_size: chunk.uncompressed_size,
                    crc: chunk.crc,
                });
            }
        }

        let parallel_download = self.steam_client.config().parallel_download;
        let parallel_decode = self.steam_client.config().parallel_decode;

        let download_channel_size = (parallel_download + parallel_decode).max(64);
        let decode_channel_size = (parallel_decode * 2).max(32);

        let (download_tx, download_rx) =
            tokio::sync::mpsc::channel::<Result<CompressedChunk>>(download_channel_size);
        let (decode_tx, decode_rx) =
            tokio::sync::mpsc::channel::<Result<DownloadedChunk>>(decode_channel_size);

        let download_worker = DownloadWorker::new(
            self.cdn_client.clone(),
            parallel_download,
            progress.clone(),
            download_tx,
        );

        let decode_worker = DecodeWorker::new(decode_tx, progress.clone());

        let write_worker = WriteWorker::new(
            base_path.to_path_buf(),
            progress.clone(),
            Arc::new(files.to_vec()),
        );

        // Start workers
        download_worker.start(tasks).await;

        let decode_handle = tokio::spawn(async move {
            decode_worker.start(download_rx).await;
        });

        // The write worker runs in the current task
        let write_result = write_worker.start(decode_rx).await;

        let _ = decode_handle.await;

        write_result
    }
}
