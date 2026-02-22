use crate::config::DownloaderConfig;
use crate::connection::ConnectionExt;
use crate::error::{Result, SteamVentError};
use reqwest::Client as HttpClient;
use steam_vent_proto::steammessages_contentsystem_steamclient::CContentServerDirectory_GetServersForSteamPipe_Request;

#[derive(Clone)]
pub struct CdnClient {
    http_client: HttpClient,
    cdn_hosts: Vec<String>,
    config: DownloaderConfig,
}

impl CdnClient {
    pub fn new(config: DownloaderConfig) -> Self {
        Self {
            http_client: HttpClient::builder()
                .timeout(config.timeout)
                .build()
                .unwrap_or_else(|_| HttpClient::new()),
            cdn_hosts: Vec::new(),
            config,
        }
    }

    pub async fn resolve_cdn_hosts(&mut self, connection: &mut impl ConnectionExt) -> Result<()> {
        if !self.cdn_hosts.is_empty() {
            return Ok(());
        }

        log::info!("Finding CDN servers...");

        let srv_req = CContentServerDirectory_GetServersForSteamPipe_Request {
            cell_id: Some(0),
            max_servers: Some(5),
            ..Default::default()
        };

        let srv_resp = connection
            .service_method_with_retry(srv_req, self.config.max_retries, self.config.retry_delay)
            .await?;

        if srv_resp.servers.is_empty() {
            return Err(SteamVentError::Cdn("No CDN servers available".to_string()));
        }

        self.cdn_hosts = srv_resp
            .servers
            .into_iter()
            .map(|s| s.vhost().to_string())
            .collect();
        Ok(())
    }

    pub fn cdn_hosts(&self) -> &[String] {
        &self.cdn_hosts
    }

    pub async fn download_manifest(
        &self,
        server_hosts: &[String],
        depot_id: u32,
        manifest_id: u64,
        mrc: u64,
    ) -> Result<Vec<u8>> {
        for server_host in server_hosts {
            let manifest_url = format!(
                "https://{}/depot/{}/manifest/{}/5/{}",
                server_host, depot_id, manifest_id, mrc
            );
            log::info!("Downloading manifest from {}...", manifest_url);

            let fetch_result = async {
                self.http_client
                    .get(&manifest_url)
                    .send()
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await
            }
            .await;

            match fetch_result {
                Ok(b) => return Ok(b.to_vec()),
                Err(e) => {
                    log::warn!("Failed to fetch manifest from {}: {}", server_host, e);
                }
            }
        }
        Err(SteamVentError::Cdn(
            "Failed to download manifest from all available CDN servers".to_string(),
        ))
    }

    pub async fn download_chunk(
        &self,
        server_hosts: &[String],
        depot_id: u32,
        chunk_id: &str,
        depot_key: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        if server_hosts.is_empty() {
            return Err(SteamVentError::Cdn("No CDN servers available".to_string()));
        }

        let mut retry_count = 0;
        let max_attempts = std::cmp::max(self.config.max_retries, server_hosts.len() as u32);

        loop {
            let server_host = &server_hosts[(retry_count as usize) % server_hosts.len()];
            let chunk_url = format!(
                "https://{}/depot/{}/chunk/{}",
                server_host, depot_id, chunk_id
            );

            let mut should_sleep = true;

            match self.http_client.get(&chunk_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let data = resp.bytes().await?;
                    match crate::archive::SteamChunk::new(&data).decode(depot_key) {
                        Ok(decoded) => return Ok(decoded),
                        Err(e) => {
                            log::warn!("Chunk decoding failed for {}: {}", chunk_id, e);
                            return Ok(data.to_vec());
                        }
                    }
                }
                Ok(resp) => {
                    let status = resp.status();
                    log::warn!(
                        "Retrying chunk {} from {} (status: {})",
                        chunk_id,
                        server_host,
                        status
                    );
                    if status == reqwest::StatusCode::NOT_FOUND
                        || status == reqwest::StatusCode::FORBIDDEN
                    {
                        should_sleep = false;
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Retrying chunk {} from {}: error {}",
                        chunk_id,
                        server_host,
                        e
                    );
                }
            }

            if retry_count < max_attempts {
                retry_count += 1;
                if should_sleep {
                    tokio::time::sleep(self.config.retry_delay).await;
                }
            } else {
                return Err(SteamVentError::Cdn(format!(
                    "Failed to download chunk {} after {} attempts",
                    chunk_id, max_attempts
                )));
            }
        }
    }
}
