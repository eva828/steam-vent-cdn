use crate::config::Config;
use crate::connection::ConnectionExt;
use crate::error::{CdnConnectionError, IntegrityError, Result, SteamVentError};
use reqwest::Client as HttpClient;
use steam_vent_proto::steammessages_contentsystem_steamclient::CContentServerDirectory_GetServersForSteamPipe_Request;

/// Metadata for a single CDN server, used for intelligent server selection.
#[derive(Debug, Clone)]
pub(crate) struct CdnServer {
    pub vhost: String,
    pub server_type: String,
    pub weighted_load: f32,
    pub load: i32,
    pub priority_class: u32,
}

#[derive(Clone)]
pub(crate) struct CdnClient {
    http_client: HttpClient,
    cdn_servers: Vec<CdnServer>,
    config: Config,
}

impl CdnClient {
    pub fn new(config: Config) -> Self {
        Self {
            http_client: HttpClient::builder()
                .timeout(config.timeout)
                .http2_prior_knowledge()
                .build()
                .unwrap_or_else(|_| HttpClient::new()),
            cdn_servers: Vec::new(),
            config,
        }
    }

    pub async fn resolve_cdn_hosts(&mut self, connection: &mut impl ConnectionExt) -> Result<()> {
        if !self.cdn_servers.is_empty() {
            return Ok(());
        }

        tracing::debug!("Finding CDN servers");

        let srv_req = CContentServerDirectory_GetServersForSteamPipe_Request {
            cell_id: Some(0),
            max_servers: Some(20),
            ..Default::default()
        };

        let srv_resp = connection
            .service_method_with_retry(
                srv_req,
                self.config.retry_policy.max_retries,
                self.config.retry_policy.retry_delay,
            )
            .await?;

        if srv_resp.servers.is_empty() {
            return Err(SteamVentError::CdnConnection(CdnConnectionError::NoServers));
        }

        let mut servers: Vec<CdnServer> = srv_resp
            .servers
            .into_iter()
            .map(|s| CdnServer {
                vhost: s.vhost().to_string(),
                server_type: s.type_().to_string(),
                weighted_load: s.weighted_load(),
                load: s.load(),
                priority_class: s.priority_class(),
            })
            .collect();

        // Sort to prioritize SteamCache servers (local cache, typically faster)
        // over generic CDN servers (alibaba, google, fastly, akamai).
        // Within the same type, sort by weighted_load (lower = less loaded).
        //
        // Steam's priority_class: SteamCache servers have higher values (e.g., 4)
        // but are typically faster due to geographic proximity. CDN servers have
        // lower values (1-2) but may be slower depending on the CDN provider.
        servers.sort_by(|a, b| {
            let a_is_cache = a.server_type == "SteamCache";
            let b_is_cache = b.server_type == "SteamCache";
            // SteamCache first, then CDN
            b_is_cache.cmp(&a_is_cache).then_with(|| {
                a.weighted_load
                    .partial_cmp(&b.weighted_load)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });

        for (i, s) in servers.iter().enumerate() {
            tracing::info!(
                "CDN server [{}]: {} (type={}, load={}, weighted_load={:.1}, priority={})",
                i,
                s.vhost,
                s.server_type,
                s.load,
                s.weighted_load,
                s.priority_class
            );
        }

        self.cdn_servers = servers;
        Ok(())
    }

    /// Get CDN server vhosts for primary use, preferring SteamCache servers.
    /// Falls back to all servers only if no SteamCache servers are available.
    pub fn cdn_hosts(&self) -> Vec<String> {
        let steam_cache: Vec<String> = self
            .cdn_servers
            .iter()
            .filter(|s| s.server_type == "SteamCache")
            .map(|s| s.vhost.clone())
            .collect();
        if steam_cache.is_empty() {
            // No SteamCache servers available, use all servers
            self.cdn_servers.iter().map(|s| s.vhost.clone()).collect()
        } else {
            steam_cache
        }
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
                    return Err(SteamVentError::CdnConnection(
                        CdnConnectionError::HttpError { status },
                    ));
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
        Err(SteamVentError::CdnConnection(
            CdnConnectionError::AllServersFailed,
        ))
    }

    pub async fn download_chunk(
        &self,
        server_hosts: &[String],
        depot_id: u32,
        chunk_id: &str,
        start_server_index: usize,
    ) -> Result<Vec<u8>> {
        if server_hosts.is_empty() {
            return Err(SteamVentError::CdnConnection(CdnConnectionError::NoServers));
        }

        let mut retry_count = 0;
        let max_attempts = std::cmp::max(
            self.config.retry_policy.max_retries,
            server_hosts.len() as u32,
        );

        loop {
            // Start from the assigned server, rotate through all servers on retry
            let server_idx = (start_server_index + retry_count as usize) % server_hosts.len();
            let server_host = &server_hosts[server_idx];
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
                return Err(SteamVentError::CdnConnection(
                    CdnConnectionError::AllServersFailed,
                ));
            }
        }
    }

    /// Phase 1: AES decryption only (lightweight, ~1ms).
    /// Returns decrypted data suitable for format detection and decompression.
    pub(crate) fn decrypt_chunk(
        compressed_data: &[u8],
        depot_key: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        crate::archive::SteamChunk::decrypt_only(compressed_data, depot_key)
    }

    /// Phase 2: Decompress already-decrypted data and verify integrity (size + CRC).
    /// This is the CPU-heavy step for VZa/LZMA formats (~170ms).
    pub(crate) fn decompress_and_verify(
        decrypted_data: &[u8],
        expected_uncompressed_size: u32,
        expected_crc: u32,
    ) -> Result<Vec<u8>> {
        let decompressed = crate::archive::SteamChunk::decompress(decrypted_data)?;

        // Verify uncompressed size matches manifest's cb_original
        let actual_size = decompressed.len() as u32;
        if actual_size != expected_uncompressed_size {
            return Err(SteamVentError::Integrity(IntegrityError::SizeMismatch {
                expected: expected_uncompressed_size,
                actual: actual_size,
            }));
        }

        // Verify Adler32 checksum matches manifest's crc
        // Steam uses Adler32 with initial seed 0 (non-standard; RFC 1950 uses 1)
        let mut hasher = adler::Adler32::from_checksum(0);
        hasher.write_slice(&decompressed);
        let actual_crc = hasher.checksum();
        if actual_crc != expected_crc {
            return Err(SteamVentError::Integrity(
                IntegrityError::ChecksumMismatch {
                    expected: expected_crc,
                    actual: actual_crc,
                },
            ));
        }

        Ok(decompressed)
    }
}
