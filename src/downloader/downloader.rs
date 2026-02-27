use std::path::Path;
use std::sync::Arc;

use crate::connection::{ConnectionExt, SteamSession};
use steam_vent_proto::steammessages_contentsystem_steamclient::CContentServerDirectory_GetServersForSteamPipe_Request;
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
    ChunkDownloadTask, CompressedChunk, DepotContext, DownloadItem, DownloadManifest,
    DownloadedChunk, ProgressCallback, VersionInfo,
};
use super::workers::{DecodeWorker, DownloadWorker, WriteWorker};

/// Metadata for a single CDN server, used for intelligent server selection.
#[derive(Debug, Clone)]
pub(crate) struct CdnServer {
    pub vhost: String,
    pub server_type: String,
    pub weighted_load: f32,
    pub load: i32,
    pub priority_class: u32,
}

pub struct CdnDownloader {
    steam_client: SteamSession,
    http_fetcher: HttpFetcher,
    cdn_servers: Vec<CdnServer>,
}

impl CdnDownloader {
    pub fn new(steam_client: SteamSession) -> Result<Self> {
        let config = steam_client.config().clone();
        Ok(Self {
            steam_client,
            http_fetcher: HttpFetcher::new(config)?,
            cdn_servers: Vec::new(),
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

        if versions.is_empty() {
            return Err(SteamVentError::Workshop(WorkshopError::NoVersionsFound));
        }

        tracing::debug!("Found {} versions", versions.len());
        Ok(versions)
    }

    async fn resolve_cdn_hosts(&mut self) -> Result<()> {
        if !self.cdn_servers.is_empty() {
            return Ok(());
        }

        tracing::debug!("Finding CDN servers");

        let srv_req = CContentServerDirectory_GetServersForSteamPipe_Request {
            cell_id: Some(0),
            max_servers: Some(20),
            ..Default::default()
        };

        let max_retries = self.steam_client.max_retries();
        let retry_delay = self.steam_client.retry_delay();

        let srv_resp = self
            .steam_client
            .connection_mut()
            .service_method_with_retry(srv_req, max_retries, retry_delay)
            .await?;

        if srv_resp.servers.is_empty() {
            return Err(SteamVentError::Cdn(CdnError::NoServers));
        }

        let mut servers: Vec<CdnServer> = srv_resp
            .servers
            .into_iter()
            .map(|s| CdnServer {
                vhost: s.vhost().to_string(),
                server_type: s.type_().to_string(),
                weighted_load: s.weighted_load(),
                load: s.load(),
                priority_class: s.priority_class(),
            })
            .collect();

        // Sort to prioritize SteamCache servers (local cache, typically faster)
        // over generic CDN servers (alibaba, google, fastly, akamai).
        // Within the same type, sort by weighted_load (lower = less loaded).
        //
        // Steam's priority_class: SteamCache servers have higher values (e.g., 4)
        // but are typically faster due to geographic proximity. CDN servers have
        // lower values (1-2) but may be slower depending on the CDN provider.
        servers.sort_by(|a, b| {
            let a_is_cache = a.server_type == "SteamCache";
            let b_is_cache = b.server_type == "SteamCache";
            // SteamCache first, then CDN
            b_is_cache.cmp(&a_is_cache).then_with(|| {
                a.weighted_load
                    .partial_cmp(&b.weighted_load)
                    .unwrap_or_else(|| {
                        if a.weighted_load.is_nan() || b.weighted_load.is_nan() {
                            tracing::warn!(
                                "CDN server has invalid weighted_load (NaN): {}={}, {}={}",
                                a.vhost,
                                a.weighted_load,
                                b.vhost,
                                b.weighted_load
                            );
                        }
                        std::cmp::Ordering::Equal
                    })
            })
        });

        for (i, s) in servers.iter().enumerate() {
            tracing::debug!(
                "CDN server [{}]: {} (type={}, load={}, weighted_load={:.1}, priority={})",
                i,
                s.vhost,
                s.server_type,
                s.load,
                s.weighted_load,
                s.priority_class
            );
        }

        self.cdn_servers = servers;
        Ok(())
    }

    /// Get CDN server vhosts for primary use, preferring SteamCache servers.
    /// Falls back to all servers only if no SteamCache servers are available.
    fn server_hosts(&self) -> Vec<String> {
        let steam_cache: Vec<String> = self
            .cdn_servers
            .iter()
            .filter(|s| s.server_type == "SteamCache")
            .map(|s| s.vhost.clone())
            .collect();
        if steam_cache.is_empty() {
            // No SteamCache servers available, use all servers
            self.cdn_servers.iter().map(|s| s.vhost.clone()).collect()
        } else {
            steam_cache
        }
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
        let servers = self.server_hosts();
        if servers.is_empty() {
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
            .download_manifest(&servers, depot_id, manifest_id, mrc)
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
            server_hosts: servers,
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

        let (download_tx, download_rx) =
            tokio::sync::mpsc::channel::<Result<CompressedChunk>>(download_channel_size);
        let (decode_tx, decode_rx) =
            tokio::sync::mpsc::channel::<Result<DownloadedChunk>>(decode_channel_size);

        let download_worker = DownloadWorker::new(
            self.http_fetcher.clone(),
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

        match decode_handle.await {
            Ok(()) => {}
            Err(e) => {
                tracing::error!("Decode worker panicked: {}", e);
                return Err(SteamVentError::Io(std::io::Error::other(format!(
                    "Decode worker panicked: {}",
                    e
                ))));
            }
        }

        write_result
    }
}
