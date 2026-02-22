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
                .http2_prior_knowledge()
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

        tracing::debug!("Finding CDN servers");

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
            tracing::debug!("Downloading manifest from {}", manifest_url);

            let fetch_result = async {
                let resp = self.http_client.get(&manifest_url).send().await?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let headers = resp.headers().clone();
                    let body_bytes = resp.bytes().await.unwrap_or_default();
                    let body_preview = String::from_utf8_lossy(&body_bytes[..body_bytes.len().min(256)]);
                    return Err(SteamVentError::Cdn(format!(
                        "HTTP {} - Headers: {:?} - Body: {}",
                        status,
                        headers
                            .into_iter()
                            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.map(|k| k.as_str().to_string()).unwrap_or_default(), v.to_string())))
                            .collect::<Vec<_>>(),
                        body_preview
                    )));
                }
                resp.bytes().await.map_err(SteamVentError::from)
            }
            .await;

            match fetch_result {
                Ok(b) => {
                    tracing::info!("Downloaded manifest from {} ({} bytes)", server_host, b.len());
                    return Ok(b.to_vec());
                }
                Err(e) => {
                    tracing::warn!("Failed to fetch manifest from {}: {}", server_host, e);
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

            tracing::debug!(
                "Attempting chunk {} from {} (attempt {}/{})",
                chunk_id,
                server_host,
                retry_count + 1,
                max_attempts
            );

            let mut should_sleep = true;

            match self.http_client.get(&chunk_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let data = resp.bytes().await?;
                    tracing::debug!(
                        "Downloaded chunk {} from {} (compressed: {} bytes)",
                        chunk_id,
                        server_host,
                        data.len()
                    );
                    return Ok(data.to_vec());
                }
                Ok(resp) => {
                    let status = resp.status();
                    let headers = resp.headers().clone();
                    let body_bytes = resp.bytes().await.unwrap_or_default();
                    let body_preview = String::from_utf8_lossy(&body_bytes[..body_bytes.len().min(256)]);

                    tracing::warn!(
                        "Retrying chunk {} from {} (attempt {}/{}, status: {}, headers: {:?}, body: {})",
                        chunk_id,
                        server_host,
                        retry_count + 1,
                        max_attempts,
                        status,
                        headers
                            .into_iter()
                            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.map(|k| k.as_str().to_string()).unwrap_or_default(), v.to_string())))
                            .collect::<Vec<_>>(),
                        body_preview
                    );
                    if status == reqwest::StatusCode::NOT_FOUND
                        || status == reqwest::StatusCode::FORBIDDEN
                    {
                        should_sleep = false;
                    }
                }
                Err(e) => {
                    let error_kind = if e.is_timeout() {
                        "timeout"
                    } else if e.is_connect() {
                        "connection"
                    } else if e.is_request() {
                        "request"
                    } else {
                        "network"
                    };

                    tracing::warn!(
                        "Retrying chunk {} from {} (attempt {}/{}, error kind: {}, error: {})",
                        chunk_id,
                        server_host,
                        retry_count + 1,
                        max_attempts,
                        error_kind,
                        e
                    );
                }
            }

            if retry_count < max_attempts {
                retry_count += 1;
                if should_sleep {
                    let delay_ms = self.config.retry_delay.as_millis();
                    tracing::debug!(
                        "Waiting {}ms before retry {} for chunk {}",
                        delay_ms,
                        retry_count + 1,
                        chunk_id
                    );
                    tokio::time::sleep(self.config.retry_delay).await;
                }
            } else {
                return Err(SteamVentError::Cdn(format!(
                    "Failed to download chunk {} after {} attempts (tried {} servers)",
                    chunk_id, max_attempts, server_hosts.len()
                )));
            }
        }
    }

    pub fn decode_chunk(compressed_data: &[u8], depot_key: Option<&[u8]>) -> Result<Vec<u8>> {
        crate::archive::SteamChunk::new(compressed_data).decode(depot_key)
    }
}
