use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::cdn::CdnClient;
use crate::connection::{ConnectionExt, SteamSession};
use crate::error::{CdnConnectionError, Result, SteamConnectionError, SteamVentError};
use steam_vent::ConnectionTrait;
use steam_vent_proto::content_manifest::ContentManifestPayload;
use steam_vent_proto::steammessages_clientserver_2::{
    CMsgClientGetDepotDecryptionKey, CMsgClientGetDepotDecryptionKeyResponse,
};
use steam_vent_proto::steammessages_contentsystem_steamclient::CContentServerDirectory_GetManifestRequestCode_Request;
use steam_vent_proto::steammessages_publishedfile_steamclient::{
    CPublishedFile_GetChangeHistory_Request, CPublishedFile_GetDetails_Request, PublishedFileDetails,
};

pub trait ProgressCallback: Send + Sync {
    fn on_download_progress(&self, bytes: u64);
    fn on_decode_progress(&self, bytes: u64);
    fn on_write_progress(&self, bytes: u64);
    fn on_file_complete(&self, path: &Path, bytes: usize);
}

#[derive(Debug, Clone)]
pub struct ChunkDownloadTask {
    pub chunk_id: String,
    pub depot: Arc<DepotContext>,
    pub file_index: usize,
    pub chunk_index: usize,
    pub compressed_size: u32,
}

#[derive(Debug, Clone)]
pub struct CompressedChunk {
    pub depot: Arc<DepotContext>,
    pub file_index: usize,
    pub chunk_index: usize,
    pub compressed_data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct DownloadedChunk {
    pub file_index: usize,
    pub chunk_index: usize,
    pub data: Vec<u8>,
}

pub struct CdnDownloader {
    steam_client: SteamSession,
    pub(crate) cdn_client: CdnClient,
}

#[derive(Debug, Clone)]
pub struct DepotContext {
    pub server_hosts: Vec<String>,
    pub depot_id: u32,
    pub depot_key: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub(crate) struct DownloadManifest {
    pub manifest: ContentManifestPayload,
    pub depot: Arc<DepotContext>,
}

impl DownloadManifest {
    pub(crate) fn to_file_map(&self) -> Result<HashMap<String, FileManifestInfo>> {
        let mut files = HashMap::new();

        for mapping in &self.manifest.mappings {
            let filename = mapping.filename.clone().unwrap_or_default();
            let chunks: Vec<ChunkInfo> = mapping
                .chunks
                .iter()
                .map(|c| -> Result<ChunkInfo> {
                    let sha = c.sha.clone().ok_or_else(|| {
                        SteamVentError::InvalidData("Chunk SHA is missing".to_string())
                    })?;

                    let uncompressed_size = c.cb_original.ok_or_else(|| {
                        SteamVentError::InvalidData("Chunk cb_original is missing".to_string())
                    })?;

                    let compressed_size = c.cb_compressed.ok_or_else(|| {
                        SteamVentError::InvalidData("Chunk cb_compressed is missing".to_string())
                    })?;

                    Ok(ChunkInfo {
                        sha,
                        uncompressed_size,
                        compressed_size,
                    })
                })
                .collect::<Result<Vec<_>>>()?;

            files.insert(
                filename,
                FileManifestInfo {
                    chunks,
                    flags: mapping.flags,
                    size: mapping.size,
                    depot: Arc::clone(&self.depot),
                },
            );
        }

        Ok(files)
    }

    #[allow(dead_code)]
    pub(crate) fn to_download_items(&self, item_id: u64) -> Result<Vec<DownloadItem>> {
        let file_map = self.to_file_map()?;
        Ok(file_map
            .into_iter()
            .map(|(filename, manifest_info)| DownloadItem {
                item_id,
                filename,
                manifest_info,
            })
            .collect())
    }
}

#[derive(Debug, Clone)]
pub struct ChunkInfo {
    pub sha: Vec<u8>,
    pub uncompressed_size: u32,
    pub compressed_size: u32,
}

#[derive(Debug, Clone)]
pub struct FileManifestInfo {
    pub chunks: Vec<ChunkInfo>,
    pub flags: Option<u32>,
    #[allow(dead_code)]
    pub size: Option<u64>,
    pub depot: Arc<DepotContext>,
}

#[derive(Debug, Clone)]
pub struct DownloadItem {
    // Currently unused during download/extraction, but kept as it conceptually
    // identifies the workshop item this file belongs to. Useful for future extensions.
    #[allow(dead_code)]
    pub item_id: u64,
    pub filename: String,
    pub manifest_info: FileManifestInfo,
}

#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub timestamp: u32,
    pub manifest_id: u64,
    pub description: String,
}

impl CdnDownloader {
    pub fn new(steam_client: SteamSession) -> Self {
        let config = steam_client.config().clone();
        Self {
            steam_client,
            cdn_client: CdnClient::new(config),
        }
    }

    pub(crate) async fn fetch_item_details(&mut self, item_id: u64) -> Result<PublishedFileDetails> {
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
            .map_err(SteamConnectionError::ApiCallFailed)
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
            .map_err(SteamConnectionError::ApiCallFailed)
            .map_err(SteamVentError::Connection)?;
        code_resp
            .manifest_request_code
            .ok_or_else(|| SteamVentError::CdnConnection(CdnConnectionError::ManifestRequestCodeFailed))
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
            .map_err(SteamConnectionError::ApiCallFailed)
            .map_err(SteamVentError::Connection)?;
        if key_resp.eresult() != 1 {
            return Err(SteamVentError::CdnConnection(CdnConnectionError::DepotKeyFailed(key_resp.eresult())));
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
            return Err(SteamVentError::CdnConnection(CdnConnectionError::HostResolutionFailed));
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

        if let Some(ref m) = metadata
            && m.filenames_encrypted.unwrap_or(false) {
            need_key_for_filenames = true;
            depot_id_for_key = m.depot_id.unwrap_or(app_id);
            tracing::debug!(
                "Manifest filenames are encrypted (depot_id: {}).",
                depot_id_for_key
            );
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
 
    pub async fn fetch_manifest_info(
        &mut self,
        item_id: u64,
    ) -> Result<HashMap<String, FileManifestInfo>> {
        let details = self.fetch_item_details(item_id).await?;

        let title = details.title.as_deref().unwrap_or("Unknown");
        tracing::info!("Title: {}", title);

        let manifest_id = details.hcontent_file.ok_or_else(|| {
            SteamVentError::InvalidData("No manifest ID (hcontent_file) found".to_string())
        })?;
        let config_app_id = self.steam_client.config().app_id;
        let app_id = details.consumer_appid.unwrap_or(config_app_id);

        let manifest_data = self.fetch_manifest_by_id(manifest_id, app_id).await?;
        manifest_data.to_file_map()
    }

    pub async fn download_to_directory(
        &mut self,
        files: &[DownloadItem],
        base_path: &Path,
        progress: Arc<dyn ProgressCallback>,
    ) -> Result<()> {
        let mut tasks: Vec<ChunkDownloadTask> = Vec::new();
        for (file_index, file) in files.iter().enumerate() {
            let depot = Arc::clone(&file.manifest_info.depot);
            for (chunk_index, chunk) in file.manifest_info.chunks.iter().enumerate() {
                tasks.push(ChunkDownloadTask {
                    chunk_id: hex::encode(&chunk.sha),
                    depot: Arc::clone(&depot),
                    file_index,
                    chunk_index,
                    compressed_size: chunk.compressed_size,
                });
            }
        }

        let parallel_download = self.steam_client.config().parallel_download;
        let parallel_decode = self.steam_client.config().parallel_decode;

        let download_sem = Arc::new(tokio::sync::Semaphore::new(parallel_download));
        let decode_sem = Arc::new(tokio::sync::Semaphore::new(parallel_decode));

        let (download_tx, download_rx) = tokio::sync::mpsc::channel::<Result<CompressedChunk>>(32);
        let (decode_tx, mut decode_rx) = tokio::sync::mpsc::channel::<Result<DownloadedChunk>>(32);

        let cdn_client = self.cdn_client.clone();

        for task in tasks {
            let download_sem = download_sem.clone();
            let download_tx = download_tx.clone();
            let cdn_client = cdn_client.clone();

            tokio::spawn({
                let progress = progress.clone();
                async move {
                    let _permit = download_sem.acquire().await.unwrap();
                    let compressed_data = cdn_client
                        .download_chunk(
                            &task.depot.server_hosts,
                            task.depot.depot_id,
                            &task.chunk_id,
                        )
                        .await;

                    match compressed_data {
                        Ok(data) => {
                            progress.on_download_progress(task.compressed_size as u64);
                            let _ = download_tx.send(Ok(CompressedChunk {
                                depot: task.depot,
                                file_index: task.file_index,
                                chunk_index: task.chunk_index,
                                compressed_data: data,
                            })).await;
                        }
                        Err(e) => {
                            let _ = download_tx.send(Err(e)).await;
                        }
                    }
                }
            });
        }

        let download_rx = Arc::new(tokio::sync::Mutex::new(download_rx));
        let decode_sem_clone = decode_sem.clone();
        let decode_tx_clone = decode_tx.clone();

        tokio::spawn({
            let progress = progress.clone();
            async move {
                loop {
                    let mut rx = download_rx.lock().await;
                    match rx.recv().await {
                        Some(Ok(compressed)) => {
                            drop(rx);
                            let _permit = decode_sem_clone.acquire().await.unwrap();
                            let decode_tx = decode_tx_clone.clone();
                            let progress = progress.clone();

                            tokio::spawn(async move {
                                let depot_key = compressed.depot.depot_key.as_deref();
                                let decoded: Result<Vec<u8>> = CdnClient::decode_chunk(&compressed.compressed_data, depot_key);

                                match decoded {
                                    Ok(data) => {
                                        let len = data.len() as u64;
                                        progress.on_decode_progress(len);
                                        progress.on_write_progress(len);
                                        let _ = decode_tx.send(Ok(DownloadedChunk {
                                            file_index: compressed.file_index,
                                            chunk_index: compressed.chunk_index,
                                            data,
                                        })).await;
                                    }
                                    Err(e) => {
                                        let _ = decode_tx.send(Err(e)).await;
                                    }
                                }
                            });
                        }
                        Some(Err(e)) => {
                            let _ = decode_tx.send(Err(e)).await;
                        }
                        None => break,
                    }
                }
            }
        });

        drop(download_tx);

        #[allow(clippy::type_complexity)]
        let chunk_buffers: Arc<tokio::sync::Mutex<HashMap<usize, Vec<(usize, Vec<u8>)>>>> =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        while let Some(result) = decode_rx.recv().await {
            match result {
                Ok(chunk) => {
                    let mut buffers = chunk_buffers.lock().await;
                    buffers
                        .entry(chunk.file_index)
                        .or_insert_with(Vec::new)
                        .push((chunk.chunk_index, chunk.data));

                    let file = &files[chunk.file_index];
                    let total_chunks = file.manifest_info.chunks.len();

                    if let Some(chunks) = buffers.get(&chunk.file_index)
                        && chunks.len() == total_chunks {
                            let file_path = base_path.join(&file.filename);
                            let mut file_data: Vec<(usize, Vec<u8>)> = chunks.clone();
                            file_data.sort_by_key(|(idx, _)| *idx);

                            let mut full_data = Vec::new();
                            for (_, data) in file_data {
                                full_data.extend(data);
                            }

                            buffers.remove(&chunk.file_index);

                            let output_path = file_path.clone();
                            let full_data_len = full_data.len();
                            progress.on_file_complete(&output_path, full_data_len);
                            tokio::spawn(async move {
                                let mut file_handle = tokio::fs::File::create(&output_path).await.unwrap();
                                use tokio::io::AsyncWriteExt;
                                file_handle.write_all(&full_data).await.unwrap();
                            });
                        }
                }
                Err(e) => {
                    tracing::error!("Error: {}", e);
                    return Err(e);
                }
            }
        }

        for (file_index, buffers) in chunk_buffers.lock().await.iter() {
            if !buffers.is_empty() {
                let file = &files[*file_index];
                let file_path = base_path.join(&file.filename);
                let mut file_data: Vec<(usize, Vec<u8>)> = buffers.clone();
                file_data.sort_by_key(|(idx, _)| *idx);

                let mut full_data = Vec::new();
                for (_, data) in file_data {
                    full_data.extend(data);
                }

                let mut file_handle = tokio::fs::File::create(&file_path).await?;
                use tokio::io::AsyncWriteExt;
                file_handle.write_all(&full_data).await?;
                progress.on_file_complete(&file_path, full_data.len());
            }
        }

        Ok(())
    }
}
