use crate::archive::ZipArchiveExtractor;
use crate::config::DownloaderConfig;
use crate::error::{Result, SteamVentError};
use reqwest::Client as HttpClient;
use steam_vent::ConnectionTrait;
use steam_vent::connection::Connection;
use steam_vent_proto::steammessages_contentsystem_steamclient::CContentServerDirectory_GetServersForSteamPipe_Request;

#[derive(Clone)]
pub struct CdnClient {
    http_client: HttpClient,
    cdn_host: Option<String>,
    config: DownloaderConfig,
}

impl CdnClient {
    pub fn new(config: DownloaderConfig) -> Self {
        Self {
            http_client: HttpClient::builder()
                .timeout(config.timeout)
                .build()
                .unwrap_or_else(|_| HttpClient::new()),
            cdn_host: None,
            config,
        }
    }

    pub async fn resolve_cdn_host(&mut self, conn: &mut Connection) -> Result<()> {
        if self.cdn_host.is_some() {
            return Ok(());
        }

        log::info!("Finding CDN servers...");

        let srv_req = CContentServerDirectory_GetServersForSteamPipe_Request {
            cell_id: Some(0),
            max_servers: Some(5),
            ..Default::default()
        };

        let srv_resp = conn
            .service_method(srv_req)
            .await
            .map_err(|e| SteamVentError::Connection(e.to_string()))?;

        if srv_resp.servers.is_empty() {
            return Err(SteamVentError::Cdn("No CDN servers available".to_string()));
        }

        self.cdn_host = Some(srv_resp.servers[0].vhost().to_string());
        Ok(())
    }

    pub fn cdn_host(&self) -> Option<&str> {
        self.cdn_host.as_deref()
    }

    pub async fn download_manifest(
        &self,
        server_host: &str,
        depot_id: u32,
        manifest_id: u64,
        mrc: u64,
    ) -> Result<Vec<u8>> {
        let manifest_url = format!(
            "https://{}/depot/{}/manifest/{}/5/{}",
            server_host, depot_id, manifest_id, mrc
        );
        log::info!("Downloading manifest from {}...", manifest_url);
        let resp = self.http_client.get(&manifest_url).send().await?;
        let resp = resp.error_for_status()?;
        let bytes = resp.bytes().await?;
        Ok(bytes.to_vec())
    }

    pub async fn download_chunk(
        &self,
        server_host: &str,
        depot_id: u32,
        chunk_id: &str,
    ) -> Result<Vec<u8>> {
        let chunk_url = format!(
            "https://{}/depot/{}/chunk/{}",
            server_host, depot_id, chunk_id
        );

        let mut retry_count = 0;
        loop {
            let resp = self.http_client.get(&chunk_url).send().await?;

            if resp.status().is_success() {
                let data = resp.bytes().await?;
                if let Ok(extractor) = ZipArchiveExtractor::new(&data) {
                    return extractor.extract();
                } else {
                    return Ok(data.to_vec());
                }
            } else if retry_count < self.config.max_retries {
                log::warn!("Retrying chunk {} (status: {})", chunk_id, resp.status());
                retry_count += 1;
                tokio::time::sleep(self.config.retry_delay).await;
            } else {
                return Err(SteamVentError::Cdn(format!(
                    "Failed to download chunk {}: {}",
                    chunk_id,
                    resp.status()
                )));
            }
        }
    }
}
