use crate::config::Config;
use crate::error::{DownloadError, Result, SteamVentError};
use reqwest::Client;

#[derive(Clone)]
pub(crate) struct HttpFetcher {
    client: Client,
    config: Config,
}

impl HttpFetcher {
    pub(crate) fn new(config: Config) -> Result<Self> {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| {
                DownloadError::InvalidConfiguration(format!("Failed to build HTTP client: {}", e))
            })?;
        Ok(Self { client, config })
    }

    pub(crate) async fn download_file(&self, url: &str) -> Result<Vec<u8>> {
        let max_attempts = self.config.retry_policy.max_retries + 1;
        let retry_delay = self.config.retry_policy.retry_delay;

        for attempt in 0..max_attempts {
            match self.client.get(url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    return resp
                        .bytes()
                        .await
                        .map(|b| b.to_vec())
                        .map_err(SteamVentError::from);
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.bytes().await.unwrap_or_default();
                    let body_preview = String::from_utf8_lossy(&body[..body.len().min(256)]);
                    tracing::debug!(
                        "Web download failed (attempt {}/{}, status: {}, body: {})",
                        attempt + 1,
                        max_attempts,
                        status,
                        body_preview,
                    );
                    if status == reqwest::StatusCode::NOT_FOUND
                        || status == reqwest::StatusCode::FORBIDDEN
                    {
                        return Err(DownloadError::HttpError { status })?;
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        "Web download request failed (attempt {}/{}): {}",
                        attempt + 1,
                        max_attempts,
                        e,
                    );
                }
            }

            if attempt + 1 < max_attempts {
                tokio::time::sleep(retry_delay).await;
            }
        }

        Err(DownloadError::AllAttemptsFailed)?
    }
}
