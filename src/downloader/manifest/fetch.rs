use crate::downloader::cdn::DepotContext;
use crate::downloader::client::CdnDownloader;
use crate::downloader::manifest::decode::RawManifest;
use crate::downloader::manifest::types::Manifest;
use crate::error::{Result, WorkshopError};
use std::sync::Arc;

impl CdnDownloader {
    /// Fetch and decode a manifest for an explicit app/depot pair.
    pub async fn fetch_manifest_by_id(
        &self,
        manifest_id: u64,
        app_id: u32,
        depot_id: u32,
    ) -> Result<Manifest> {
        tracing::debug!(
            "Using app_id={}, depot_id={}, manifest_id={}",
            app_id,
            depot_id,
            manifest_id
        );
        self.fetch_and_decode_manifest(manifest_id, app_id, depot_id)
            .await
    }

    async fn fetch_and_decode_manifest(
        &self,
        manifest_id: u64,
        app_id: u32,
        depot_id: u32,
    ) -> Result<Manifest> {
        let mrc = self
            .fetch_manifest_request_code(app_id, depot_id, manifest_id)
            .await?;
        tracing::debug!("Manifest Request Code: {}", mrc);

        let manifest_bytes = self
            .http_fetcher
            .download_manifest(&self.content_servers, depot_id, manifest_id, mrc)
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
            servers: self.content_servers.clone(),
            depot_id,
            depot_key: depot_key_opt,
        });

        let entries = raw_manifest.into_entries(depot)?;
        Ok(Manifest::classify(entries))
    }

    pub async fn fetch_latest_manifest_id(&self, item_id: u64) -> Result<u64> {
        let details = self.fetch_item_details(item_id).await?;
        Ok(details
            .manifest_id
            .ok_or(WorkshopError::MissingManifestId)?)
    }

    pub async fn fetch_manifest_info(&self, item_id: u64, manifest_id: u64) -> Result<Manifest> {
        let details = self.fetch_item_details(item_id).await?;

        tracing::info!("Title: {}", details.title);

        let app_id = details
            .consumer_appid
            .ok_or(WorkshopError::MissingConsumerAppId)?;

        // Workshop manifests use the consumer app as both app and depot id.
        self.fetch_manifest_by_id(manifest_id, app_id, app_id).await
    }
}
