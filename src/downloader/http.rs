use crate::config::Config;
use crate::error::{CdnError, Result, SteamVentError};
use reqwest::Client as HttpClient;

use super::servers::CdnServerList;

/// HTTP fetcher for CDN content downloads.
/// Handles low-level HTTP operations for manifest and chunk downloads.
#[derive(Clone)]
pub(crate) struct HttpFetcher {
    http_client: HttpClient,
    config: Config,
}

impl HttpFetcher {
    pub fn new(config: Config) -> Result<Self> {
        let http_client = HttpClient::builder()
            .timeout(config.timeout)
            .http2_prior_knowledge()
            .build()
            .map_err(|e| {
                SteamVentError::Cdn(CdnError::InvalidConfiguration(format!(
                    "Failed to build HTTP client with custom timeout: {}",
                    e
                )))
            })?;
        Ok(Self {
            http_client,
            config,
        })
    }

    pub async fn download_manifest(
        &self,
        servers: &CdnServerList,
        depot_id: u32,
        manifest_id: u64,
        mrc: u64,
    ) -> Result<Vec<u8>> {
        for i in 0..servers.len() {
            let server_host = servers.vhost_at(i);
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
                    let body_preview =
                        String::from_utf8_lossy(&body_bytes[..body_bytes.len().min(256)]);
                    tracing::warn!(
                        "HTTP {} - Headers: {:?} - Body: {}",
                        status,
                        headers
                            .into_iter()
                            .filter_map(|(k, v)| v.to_str().ok().map(|v| (
                                k.map(|k| k.as_str().to_string()).unwrap_or_default(),
                                v.to_string()
                            )))
                            .collect::<Vec<_>>(),
                        body_preview
                    );
                    return Err(SteamVentError::Cdn(CdnError::HttpError { status }));
                }
                resp.bytes().await.map_err(SteamVentError::from)
            }
            .await;

            match fetch_result {
                Ok(b) => {
                    tracing::info!(
                        "Downloaded manifest from {} ({} bytes)",
                        server_host,
                        b.len()
                    );
                    return Ok(b.to_vec());
                }
                Err(e) => {
                    tracing::warn!("Failed to fetch manifest from {}: {}", server_host, e);
                }
            }
        }
        Err(SteamVentError::Cdn(CdnError::AllServersFailed))
    }

    pub async fn download_chunk(
        &self,
        servers: &CdnServerList,
        depot_id: u32,
        chunk_id: &str,
        start_server_index: usize,
    ) -> Result<Vec<u8>> {
        if servers.is_empty() {
            return Err(SteamVentError::Cdn(CdnError::NoServers));
        }

        let mut retry_count = 0;
        let max_attempts = std::cmp::max(
            self.config.retry_policy.max_retries,
            servers.len() as u32,
        );

        loop {
            // Start from the assigned server, rotate through all servers on retry
            let server_idx = (start_server_index + retry_count as usize) % servers.len();
            let server_host = servers.vhost_at(server_idx);
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
                    let body_preview =
                        String::from_utf8_lossy(&body_bytes[..body_bytes.len().min(256)]);

                    tracing::warn!(
                        "Retrying chunk {} from {} (attempt {}/{}, status: {}, headers: {:?}, body: {})",
                        chunk_id,
                        server_host,
                        retry_count + 1,
                        max_attempts,
                        status,
                        headers
                            .into_iter()
                            .filter_map(|(k, v)| v.to_str().ok().map(|v| (
                                k.map(|k| k.as_str().to_string()).unwrap_or_default(),
                                v.to_string()
                            )))
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
                    let delay_ms = self.config.retry_policy.retry_delay.as_millis();
                    tracing::debug!(
                        "Waiting {}ms before retry {} for chunk {}",
                        delay_ms,
                        retry_count + 1,
                        chunk_id
                    );
                    tokio::time::sleep(self.config.retry_policy.retry_delay).await;
                }
            } else {
                return Err(SteamVentError::Cdn(CdnError::AllServersFailed));
            }
        }
    }
}
