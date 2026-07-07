use crate::config::Config;
use crate::error::{DownloadError, Result, SteamVentError};
use reqwest::Client as HttpClient;

use super::servers::{ContentServerList, RoundRobin};

#[derive(Clone)]
pub(crate) struct CdnFetcher {
    http_client: HttpClient,
    config: Config,
}

impl CdnFetcher {
    pub(crate) fn new(config: Config) -> Result<Self> {
        let http_client = HttpClient::builder()
            .timeout(config.timeout)
            .http2_prior_knowledge()
            .build()
            .map_err(|e| {
                DownloadError::InvalidConfiguration(format!(
                    "Failed to build HTTP client: {}",
                    e
                ))
            })?;
        Ok(Self {
            http_client,
            config,
        })
    }

    pub(crate) async fn download_manifest(
        &self,
        servers: &ContentServerList,
        depot_id: u32,
        manifest_id: u64,
        manifest_request_code: u64,
    ) -> Result<Vec<u8>> {
        for server_host in servers.iter() {
            let manifest_url = format!(
                "https://{}/depot/{}/manifest/{}/5/{}",
                server_host, depot_id, manifest_id, manifest_request_code
            );
            tracing::debug!("Downloading manifest from {}", manifest_url);

            let fetch_result = async {
                let resp = self.http_client.get(&manifest_url).send().await?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let headers: Vec<String> = resp
                        .headers()
                        .iter()
                        .map(|(k, v)| format!("{}: {}", k, v.to_str().unwrap_or("<binary>")))
                        .collect();
                    let body = resp.bytes().await.unwrap_or_default();
                    let body_preview = String::from_utf8_lossy(&body[..body.len().min(256)]);
                    tracing::debug!(
                        "Manifest request failed (status={}, headers=[{}]): {}",
                        status,
                        headers.join(", "),
                        body_preview
                    );
                    return Err(DownloadError::HttpError { status })?;
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
                    servers.report_success(server_host);
                    return Ok(b.to_vec());
                }
                Err(e) => {
                    servers.report_failure(server_host);
                    tracing::debug!("Failed to fetch manifest from {}: {}", server_host, e);
                }
            }
        }
        Err(DownloadError::AllServersFailed)?
    }

    pub(crate) async fn download_chunk(
        &self,
        servers: &RoundRobin<'_>,
        depot_id: u32,
        chunk_id: &str,
    ) -> Result<Vec<u8>> {
        let mut attempt_count = 0;
        let max_attempts =
            std::cmp::max(self.config.retry_policy.max_retries, servers.len() as u32);

        loop {
            let server_host = servers.next()?;
            let chunk_url = format!(
                "https://{}/depot/{}/chunk/{}",
                server_host, depot_id, chunk_id
            );

            tracing::debug!(
                "Attempting chunk {} from {} (attempt {}/{})",
                chunk_id,
                server_host,
                attempt_count + 1,
                max_attempts
            );

            let mut should_sleep = true;

            match self.http_client.get(&chunk_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let data = resp.bytes().await?;
                    servers.report_success(server_host);
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
                    servers.report_failure(server_host);

                    let body = resp.bytes().await.unwrap_or_default();
                    let body_preview = String::from_utf8_lossy(&body[..body.len().min(256)]);
                    tracing::debug!(
                        "Retrying chunk {} from {} (attempt {}/{}, status: {}, body: {})",
                        chunk_id,
                        server_host,
                        attempt_count + 1,
                        max_attempts,
                        status,
                        body_preview,
                    );
                    if status == reqwest::StatusCode::NOT_FOUND
                        || status == reqwest::StatusCode::FORBIDDEN
                    {
                        should_sleep = false;
                    }
                }
                Err(e) => {
                    servers.report_failure(server_host);

                    let error_kind = if e.is_timeout() {
                        "timeout"
                    } else if e.is_connect() {
                        "connection"
                    } else if e.is_request() {
                        "request"
                    } else {
                        "network"
                    };

                    tracing::debug!(
                        "Retrying chunk {} from {} (attempt {}/{}, error kind: {}, error: {})",
                        chunk_id,
                        server_host,
                        attempt_count + 1,
                        max_attempts,
                        error_kind,
                        e
                    );
                }
            }

            if attempt_count < max_attempts {
                attempt_count += 1;
                if should_sleep {
                    let delay_ms = self.config.retry_policy.retry_delay.as_millis();
                    tracing::debug!(
                        "Waiting {}ms before retry {} for chunk {}",
                        delay_ms,
                        attempt_count + 1,
                        chunk_id
                    );
                    tokio::time::sleep(self.config.retry_policy.retry_delay).await;
                }
            } else {
                let healthy = servers.healthy_count();
                tracing::warn!(
                    "Chunk {} failed after {} attempts ({} healthy servers remaining)",
                    chunk_id,
                    max_attempts,
                    healthy
                );
                return Err(DownloadError::AllServersFailed)?;
            }
        }
    }
}
