use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::error::{DownloadError, Result, SteamVentError};
use reqwest::Client as HttpClient;

use super::servers::{ContentServerList, RoundRobin};

#[derive(Clone)]
pub(crate) struct CdnFetcher {
    http_client: HttpClient,
    semaphore: Arc<Semaphore>,
    config: Config,
}

impl CdnFetcher {
    pub(crate) fn new(config: Config, semaphore: Arc<Semaphore>) -> Result<Self> {
        let http_client = HttpClient::builder()
            .timeout(config.timeout)
            .http2_prior_knowledge()
            .build()
            .map_err(|e| {
                DownloadError::InvalidConfiguration(format!("Failed to build HTTP client: {}", e))
            })?;
        Ok(Self {
            http_client,
            semaphore,
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
        if let Some(cache) = &self.config.cache
            && let Some(data) = cache.get_manifest(depot_id, manifest_id).await
        {
            tracing::debug!(
                "Manifest {}/{} cache hit ({} bytes)",
                depot_id,
                manifest_id,
                data.len()
            );
            return Ok(data);
        }

        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| DownloadError::SemaphoreClosed)?;

        for round in 0..=1 {
            let rr = RoundRobin::new(servers);
            for _ in 0..servers.len() {
                let Ok(server_host) = rr.next() else { break };
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
                        if let Some(cache) = &self.config.cache {
                            cache.put_manifest(depot_id, manifest_id, &b).await;
                        }
                        return Ok(b.to_vec());
                    }
                    Err(e) => {
                        let is_client_error = matches!(
                            &e,
                            SteamVentError::Download(DownloadError::HttpError { status })
                                if status.as_u16() == 404 || status.as_u16() == 403
                        );
                        if !is_client_error {
                            servers.report_failure(server_host);
                        }
                        match &e {
                            SteamVentError::Download(DownloadError::HttpError { status }) => {
                                tracing::debug!(
                                    "Failed to fetch manifest from {}: HTTP {}",
                                    server_host,
                                    status
                                );
                            }
                            _ => {
                                tracing::debug!(
                                    "Failed to fetch manifest from {}: {}",
                                    server_host,
                                    e
                                );
                            }
                        }
                    }
                }
            }

            // All servers failed — wait for blacklist recovery if applicable
            if round == 0 {
                let remaining = servers.max_blacklist_remaining();
                if remaining > Duration::ZERO {
                    let wait = remaining + Duration::from_secs(2);
                    tracing::warn!(
                        "All servers failed, waiting {:.1}s for blacklist recovery...",
                        wait.as_secs_f64()
                    );
                    tokio::time::sleep(wait).await;
                } else {
                    break;
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
        if let Some(cache) = &self.config.cache
            && let Some(data) = cache.get_chunk(chunk_id).await
        {
            tracing::debug!("Chunk {} cache hit ({} bytes)", chunk_id, data.len());
            return Ok(data);
        }

        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| DownloadError::SemaphoreClosed)?;

        let max_attempts = self.config.retry_policy.max_retries + 1;

        for round in 0..=1 {
            for attempt_count in 1..=max_attempts {
                let server_host = servers.next()?;
                let chunk_url = format!(
                    "https://{}/depot/{}/chunk/{}",
                    server_host, depot_id, chunk_id
                );

                tracing::debug!(
                    "Attempting chunk {} from {} (attempt {}/{})",
                    chunk_id,
                    server_host,
                    attempt_count,
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
                        if let Some(cache) = &self.config.cache {
                            cache.put_chunk(chunk_id, &data).await;
                        }
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
                            attempt_count,
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
                            attempt_count,
                            max_attempts,
                            error_kind,
                            e
                        );
                    }
                }

                if attempt_count < max_attempts && should_sleep {
                    let delay_ms = self.config.retry_policy.retry_delay.as_millis();
                    tracing::debug!(
                        "Waiting {}ms before retry {} for chunk {}",
                        delay_ms,
                        attempt_count + 1,
                        chunk_id
                    );
                    tokio::time::sleep(self.config.retry_policy.retry_delay).await;
                }
            }

            // All servers failed — wait for blacklist recovery if applicable
            if round == 0 {
                let remaining = servers.max_blacklist_remaining();
                if remaining > Duration::ZERO {
                    let wait = remaining + Duration::from_secs(2);
                    tracing::warn!(
                        "All servers failed for chunk {}, waiting {:.1}s for blacklist recovery...",
                        chunk_id,
                        wait.as_secs_f64()
                    );
                    tokio::time::sleep(wait).await;
                } else {
                    break;
                }
            }
        }

        let healthy = servers.healthy_count();
        tracing::warn!(
            "Chunk {} failed after {} attempts ({} healthy servers remaining)",
            chunk_id,
            max_attempts,
            healthy
        );
        Err(DownloadError::AllServersFailed)?
    }
}


