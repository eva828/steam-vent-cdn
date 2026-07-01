use crate::downloader::cdn::DepotContext;
use crate::downloader::client::CdnDownloader;
use crate::downloader::manifest::decode::RawManifest;
use crate::downloader::manifest::types::Manifest;
use crate::error::{CdnError, Result, SteamVentError, WorkshopError};
use std::sync::Arc;

impl CdnDownloader {
    pub async fn fetch_manifest_by_id(
        &mut self,
        manifest_id: u64,
        app_id: u32,
    ) -> Result<Manifest> {
        self.cdn_servers.resolve(&mut self.steam_session).await?;
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

        let mut raw_manifest = RawManifest::decode(&manifest_bytes)?;
        let need_key_for_filenames = raw_manifest.filenames_encrypted()?;
        let depot_id_for_key = raw_manifest.depot_id_for_key(app_id)?;

        let mut depot_key_opt = None;

        match self.fetch_depot_key(app_id, depot_id_for_key).await {
            Ok(depot_key) => {
                tracing::debug!("Depot key received ({} bytes).", depot_key.len());
                if need_key_for_filenames {
                    raw_manifest.decrypt_filenames(&depot_key)?;
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

        let entries = raw_manifest.into_entries(depot)?;
        Ok(Manifest::classify(entries))
    }

    pub async fn fetch_latest_manifest_id(&mut self, item_id: u64) -> Result<u64> {
        let details = self.fetch_item_details(item_id).await?;
        details
            .manifest_id
            .ok_or_else(|| SteamVentError::Workshop(WorkshopError::MissingManifestId))
    }

    pub async fn fetch_manifest_info(
        &mut self,
        item_id: u64,
        manifest_id: u64,
    ) -> Result<Manifest> {
        let details = self.fetch_item_details(item_id).await?;

        tracing::info!("Title: {}", details.title);

        let app_id = details
            .consumer_appid
            .ok_or_else(|| SteamVentError::Workshop(WorkshopError::MissingConsumerAppId))?;

        self.fetch_manifest_by_id(manifest_id, app_id).await
    }
}
