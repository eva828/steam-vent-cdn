use std::collections::HashMap;

use crate::cdn::CdnClient;
use crate::config::DownloaderConfig;
use crate::connection::SteamClient;
use crate::error::{Result, SteamVentError};
use crate::manifest::{ManifestParser, ProtobufManifestParser};
use futures::stream::{self, Stream, StreamExt};
use hex;
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
    config: DownloaderConfig,
    steam_client: SteamClient,
    cdn_client: CdnClient,
    manifest_parser: ProtobufManifestParser,
}

#[derive(Debug, Clone)]
pub struct DownloadManifest {
    pub manifest: ContentManifestPayload,
    pub server_host: String,
    pub depot_id: u32,
}

#[derive(Debug, Clone)]
pub struct ChunkInfo {
    pub sha: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct FileManifestInfo {
    pub server_host: String,
    pub depot_id: u32,
    pub chunks: Vec<ChunkInfo>,
}

#[derive(Debug, Clone)]
pub struct DownloadItem {
    pub item_id: u64,
    pub filename: String,
    pub manifest_info: FileManifestInfo,
}

impl CdnDownloader {
    pub async fn new(username: Option<String>, password: Option<String>) -> Result<Self> {
        let config = DownloaderConfig::new(username, password);
        let steam_client = SteamClient::new(config.clone()).await?;
        Ok(Self {
            config: config.clone(),
            steam_client,
            cdn_client: CdnClient::new(config),
            manifest_parser: ProtobufManifestParser,
        })
    }

    pub async fn login(&mut self) -> Result<()> {
        if self.config.username.is_some() && self.config.password.is_some() {
            self.steam_client.login().await
        } else {
            self.steam_client.connect_anonymous().await
        }
    }

    async fn fetch_item_details(&mut self, item_id: u64) -> Result<PublishedFileDetails> {
        let details_req = CPublishedFile_GetDetails_Request {
            publishedfileids: vec![item_id],
            ..Default::default()
        };
        let details_resp = self
            .steam_client
            .connection_mut()?
            .service_method(details_req)
            .await
            .map_err(|e| SteamVentError::Connection(e.to_string()))?;
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
        log::info!("Requesting manifest request code...");

        let mut code_req = CContentServerDirectory_GetManifestRequestCode_Request::new();
        code_req.set_app_id(app_id);
        code_req.set_depot_id(depot_id);
        code_req.set_manifest_id(manifest_id);
        code_req.set_app_branch("public".into());

        let code_resp = self
            .steam_client
            .connection_mut()?
            .service_method(code_req)
            .await
            .map_err(|e| SteamVentError::Connection(e.to_string()))?;
        code_resp
            .manifest_request_code
            .ok_or_else(|| SteamVentError::Cdn("Failed to get manifest request code".to_string()))
    }

    async fn fetch_depot_key(&mut self, app_id: u32, depot_id: u32) -> Result<Vec<u8>> {
        log::info!("Requesting depot decryption key...");

        let mut key_req = CMsgClientGetDepotDecryptionKey::new();
        key_req.set_depot_id(depot_id);
        key_req.set_app_id(app_id);

        let key_resp: CMsgClientGetDepotDecryptionKeyResponse = self
            .steam_client
            .connection_mut()?
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
            .resolve_cdn_host(self.steam_client.connection_mut()?)
            .await?;
        let server = self
            .cdn_client
            .cdn_host()
            .ok_or_else(|| SteamVentError::Cdn("CDN host not resolved".to_string()))?
            .to_string();

        let details = self.fetch_item_details(item_id).await?;

        let title = details.title.as_deref().unwrap_or("Unknown");
        log::info!("Title: {}", title);

        let manifest_id = details.hcontent_file.ok_or_else(|| {
            SteamVentError::InvalidData("No manifest ID (hcontent_file) found".to_string())
        })?;
        let app_id = details.consumer_appid.unwrap_or(1281930);
        let depot_id = app_id;

        log::info!("Manifest ID: {}", manifest_id);
        log::info!("App/Depot ID: {}", app_id);

        let mrc = self
            .fetch_manifest_request_code(app_id, depot_id, manifest_id)
            .await?;
        log::info!("Manifest Request Code: {}", mrc);

        let manifest_bytes = self
            .cdn_client
            .download_manifest(&server, depot_id, manifest_id, mrc)
            .await?;

        let (mut manifest, metadata) = self.manifest_parser.parse(&manifest_bytes)?;

        if let Some(ref m) = metadata {
            if m.filenames_encrypted.unwrap_or(false) {
                log::info!("Manifest filenames are encrypted.");
                let depot_id_actual = m.depot_id.unwrap_or(app_id as u32);
                let depot_key = self.fetch_depot_key(app_id, depot_id_actual).await?;

                log::info!(
                    "Depot key received ({} bytes). Decrypting filenames...",
                    depot_key.len()
                );
                self.manifest_parser
                    .decrypt_filenames(&mut manifest, &depot_key)?;
            }
        }

        Ok(DownloadManifest {
            manifest,
            server_host: server,
            depot_id,
        })
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
                    server_host: manifest_data.server_host.clone(),
                    depot_id: manifest_data.depot_id,
                    chunks,
                },
            );
        }

        Ok(files)
    }

    pub async fn get_stream(
        &self,
        item: &DownloadItem,
    ) -> Result<impl Stream<Item = Result<Vec<u8>>> + '_> {
        let server_host = item.manifest_info.server_host.clone();
        let depot_id = item.manifest_info.depot_id;
        let chunks: Vec<ChunkInfo> = item.manifest_info.chunks.clone();

        Ok(stream::iter(chunks).then(move |chunk| {
            let server = server_host.clone();
            let cdn_client = self.cdn_client.clone();
            async move {
                let chunk_id = hex::encode(chunk.sha);
                cdn_client
                    .download_chunk(&server, depot_id, &chunk_id)
                    .await
            }
        }))
    }
}
