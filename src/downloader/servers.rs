use crate::connection::{ConnectionExt, SteamSession};
use crate::error::{CdnError, Result, SteamVentError};
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

/// A sorted list of CDN servers.
///
/// Guaranteed to be in priority order: SteamCache servers first (typically
/// faster due to local proximity), then generic CDN servers, sorted by
/// weighted_load within each group.
#[derive(Debug, Clone)]
pub(crate) struct CdnServerList {
    servers: Vec<CdnServer>,
}

impl CdnServerList {
    /// Fetch server list from Steam and return a priority-sorted list.
    pub async fn discover(client: &mut SteamSession) -> Result<Self> {
        tracing::debug!("Finding CDN servers");

        let srv_req = CContentServerDirectory_GetServersForSteamPipe_Request {
            cell_id: Some(0),
            max_servers: Some(20),
            ..Default::default()
        };

        let max_retries = client.max_retries();
        let retry_delay = client.retry_delay();

        let srv_resp = client
            .connection_mut()
            .service_method_with_retry(srv_req, max_retries, retry_delay)
            .await?;

        if srv_resp.servers.is_empty() {
            return Err(SteamVentError::Cdn(CdnError::NoServers));
        }

        let servers: Vec<CdnServer> = srv_resp
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

        Ok(Self::sorted(servers))
    }

    pub fn new() -> Self {
        Self {
            servers: Vec::new(),
        }
    }

    /// Build from a raw vec of `CdnServer`, applying priority sorting.
    pub fn sorted(mut servers: Vec<CdnServer>) -> Self {
        servers.sort_by(|a, b| {
            let a_is_cache = a.server_type == "SteamCache";
            let b_is_cache = b.server_type == "SteamCache";
            // SteamCache first, then CDN
            b_is_cache.cmp(&a_is_cache).then_with(|| {
                a.weighted_load
                    .partial_cmp(&b.weighted_load)
                    .unwrap_or_else(|| {
                        if a.weighted_load.is_nan() || b.weighted_load.is_nan() {
                            tracing::warn!(
                                "CDN server has invalid weighted_load (NaN): {}={}, {}={}",
                                a.vhost,
                                a.weighted_load,
                                b.vhost,
                                b.weighted_load
                            );
                        }
                        std::cmp::Ordering::Equal
                    })
            })
        });

        for (i, s) in servers.iter().enumerate() {
            tracing::debug!(
                "CDN server [{}]: {} (type={}, load={}, weighted_load={:.1}, priority={})",
                i,
                s.vhost,
                s.server_type,
                s.load,
                s.weighted_load,
                s.priority_class
            );
        }

        Self { servers }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.servers.len()
    }

    /// Return all vhosts in priority-sorted order.
    #[allow(dead_code)]
    pub fn vhosts(&self) -> Vec<String> {
        self.servers.iter().map(|s| s.vhost.clone()).collect()
    }

    /// Access a single vhost by index.
    pub fn vhost_at(&self, idx: usize) -> &str {
        &self.servers[idx].vhost
    }
}

impl Default for CdnServerList {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Vec<CdnServer>> for CdnServerList {
    fn from(servers: Vec<CdnServer>) -> Self {
        Self::sorted(servers)
    }
}
