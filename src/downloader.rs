use std::collections::HashMap;
use std::sync::Arc;

use crate::cdn::CdnClient;
use crate::connection::{ConnectionExt, SteamSession};
use crate::error::{Result, SteamVentError};
use futures::stream::{self, Stream, StreamExt};
use steam_vent::ConnectionTrait;
use steam_vent_proto::content_manifest::ContentManifestPayload;
use steam_vent_proto::steammessages_clientserver_2::{
    CMsgClientGetDepotDecryptionKey, CMsgClientGetDepotDecryptionKeyResponse,
};
use steam_vent_proto::steammessages_contentsystem_steamclient::CContentServerDirectory_GetManifestRequestCode_Request;
use steam_vent_proto::steammessages_publishedfile_steamclient::{
    CPublishedFile_GetDetails_Request, PublishedFileDetails,
};

pub struct CdnDownloader {
    steam_client: SteamSession,
    cdn_client: CdnClient,
}

#[derive(Debug, Clone)]
pub struct DepotContext {
    pub server_hosts: Vec<String>,
    pub depot_id: u32,
    pub depot_key: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct DownloadManifest {
    pub manifest: ContentManifestPayload,
    pub depot: Arc<DepotContext>,
}

#[derive(Debug, Clone)]
pub struct ChunkInfo {
    pub sha: Vec<u8>,
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

impl CdnDownloader {
    pub fn new(steam_client: SteamSession) -> Self {
        let config = steam_client.config().clone();
        Self {
            steam_client,
            cdn_client: CdnClient::new(config),
        }
    }

    async fn fetch_item_details(&mut self, item_id: u64) -> Result<PublishedFileDetails> {
        let details_req = CPublishedFile_GetDetails_Request {
            publishedfileids: vec![item_id],
            ..Default::default()
        };

        let max_retries = self.steam_client.config().max_retries;
        let retry_delay = self.steam_client.config().retry_delay;

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

    async fn fetch_manifest_request_code(
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
            .map_err(|e| SteamVentError::Connection(e.to_string()))?;
        code_resp
            .manifest_request_code
            .ok_or_else(|| SteamVentError::Cdn("Failed to get manifest request code".to_string()))
    }

    async fn fetch_depot_key(&mut self, app_id: u32, depot_id: u32) -> Result<Vec<u8>> {
        tracing::debug!("Requesting depot decryption key");

        let mut key_req = CMsgClientGetDepotDecryptionKey::new();
        key_req.set_depot_id(depot_id);
        key_req.set_app_id(app_id);

        let key_resp: CMsgClientGetDepotDecryptionKeyResponse = self
            .steam_client
            .connection_mut()
            .job(key_req)
            .await
            .map_err(|e| SteamVentError::Connection(e.to_string()))?;
        if key_resp.eresult() != 1 {
            return Err(SteamVentError::Cdn(format!(
                "Failed to get depot decryption key: EResult {}",
                key_resp.eresult()
            )));
        }
        Ok(key_resp.depot_encryption_key().to_vec())
    }

    async fn fetch_manifest(&mut self, item_id: u64) -> Result<DownloadManifest> {
        self.cdn_client
            .resolve_cdn_hosts(self.steam_client.connection_mut())
            .await?;
        let servers = self.cdn_client.cdn_hosts().to_vec();
        if servers.is_empty() {
            return Err(SteamVentError::Cdn("CDN hosts not resolved".to_string()));
        }

        let details = self.fetch_item_details(item_id).await?;

        let title = details.title.as_deref().unwrap_or("Unknown");
        tracing::info!("Title: {}", title);

        let manifest_id = details.hcontent_file.ok_or_else(|| {
            SteamVentError::InvalidData("No manifest ID (hcontent_file) found".to_string())
        })?;
        let config_app_id = self.steam_client.config().app_id;
        let app_id = details.consumer_appid.unwrap_or(config_app_id);
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
        let mut depot_id_for_key = app_id as u32;

        if let Some(ref m) = metadata {
            if m.filenames_encrypted.unwrap_or(false) {
                need_key_for_filenames = true;
                depot_id_for_key = m.depot_id.unwrap_or(app_id as u32);
                tracing::debug!(
                    "Manifest filenames are encrypted (depot_id: {}).",
                    depot_id_for_key
                );
            }
        }

        // Always attempt to fetch the depot key (both for filename decryption and chunk decryption)
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
                    return Err(SteamVentError::Cdn(format!(
                        "Failed to fetch depot key: {}",
                        e
                    )));
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
        let manifest_data = self.fetch_manifest(item_id).await?;
        let mut files = HashMap::new();

        for mapping in manifest_data.manifest.mappings {
            let filename = mapping.filename.unwrap_or_default();
            let chunks: Vec<ChunkInfo> = mapping
                .chunks
                .into_iter()
                .map(|c| ChunkInfo {
                    sha: c.sha.unwrap_or_default(),
                })
                .collect();

            files.insert(
                filename,
                FileManifestInfo {
                    chunks,
                    flags: mapping.flags,
                    size: mapping.size,
                    depot: Arc::clone(&manifest_data.depot),
                },
            );
        }

        Ok(files)
    }

    pub async fn get_stream(
        &self,
        item: &DownloadItem,
    ) -> Result<impl Stream<Item = Result<Vec<u8>>> + '_> {
        let depot = Arc::clone(&item.manifest_info.depot);
        let chunks: Vec<ChunkInfo> = item.manifest_info.chunks.clone();

        let concurrent_downloads = self.steam_client.config().concurrent_downloads;

        Ok(stream::iter(chunks)
            .map(move |chunk| {
                let depot = Arc::clone(&depot);
                let cdn_client = self.cdn_client.clone();
                async move {
                    let chunk_id = hex::encode(chunk.sha);
                    cdn_client
                        .download_chunk(
                            &depot.server_hosts,
                            depot.depot_id,
                            &chunk_id,
                            depot.depot_key.as_deref(),
                        )
                        .await
                }
            })
            .buffered(concurrent_downloads))
    }
}
