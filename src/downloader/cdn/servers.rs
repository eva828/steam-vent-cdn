use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::config::ServerMode;
use crate::connection::{ConnectionExt, SteamSession};
use crate::error::{CdnError, Result, SteamVentError};
use steam_vent_proto::steammessages_contentsystem_steamclient::CContentServerDirectory_GetServersForSteamPipe_Request;

const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const BASE_BLACKLIST_TIMEOUT_SECONDS: u64 = 10;
const DEFAULT_CELL_ID: u32 = 0;

#[derive(Debug, Clone, Default)]
struct ServerHealth {
    consecutive_failures: u32,
    blacklisted: bool,
    blacklisted_at: Option<Instant>,
    blacklist_count: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct CdnServer {
    pub vhost: String,
    pub server_type: String,
    pub weighted_load: f32,
    pub load: i32,
    pub priority_class: u32,
}

#[derive(Debug, Clone)]
pub struct DepotContext {
    pub(crate) server_hosts: CdnServerList,
    pub depot_id: u32,
    pub depot_key: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub(crate) struct CdnServerList {
    cache: Vec<CdnServer>,
    cdn: Vec<CdnServer>,
    cursor: Arc<AtomicUsize>,
    health: Arc<Mutex<HashMap<String, ServerHealth>>>,
    server_mode: ServerMode,
}

pub(crate) struct RoundRobin<'a> {
    list: &'a CdnServerList,
}

fn sort_by_load(servers: &mut [CdnServer]) {
    if servers.iter().any(|s| s.weighted_load.is_nan()) {
        let nan_servers: Vec<&str> = servers
            .iter()
            .filter(|s| s.weighted_load.is_nan())
            .map(|s| s.vhost.as_str())
            .collect();
        tracing::warn!(
            "CDN servers have invalid weighted_load (NaN): {:?}",
            nan_servers
        );
    }
    servers.sort_by(|a, b| {
        a.weighted_load
            .partial_cmp(&b.weighted_load)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn blacklist_timeout(count: u32) -> Duration {
    let multiplier = 2u64.saturating_pow(count.saturating_sub(1));
    let seconds = BASE_BLACKLIST_TIMEOUT_SECONDS.saturating_mul(multiplier);
    Duration::from_secs(seconds)
}

impl CdnServerList {
    pub(crate) async fn discover(client: &SteamSession) -> Result<Self> {
        tracing::debug!("Finding CDN servers (cell_id={})", DEFAULT_CELL_ID);

        let srv_req = CContentServerDirectory_GetServersForSteamPipe_Request {
            cell_id: Some(DEFAULT_CELL_ID),
            max_servers: Some(20),
            ..Default::default()
        };

        let config = client.config();
        let max_retries = config.retry_policy.max_retries;
        let retry_delay = config.retry_policy.retry_delay;
        let server_mode = config.server_mode;

        let srv_resp = client
            .connection()
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

        Ok(Self::sorted(servers, server_mode))
    }

    pub(crate) fn new() -> Self {
        Self::with_server_mode(ServerMode::default())
    }

    fn with_server_mode(server_mode: ServerMode) -> Self {
        Self {
            cache: Vec::new(),
            cdn: Vec::new(),
            cursor: Arc::new(AtomicUsize::new(0)),
            health: Arc::new(Mutex::new(HashMap::new())),
            server_mode,
        }
    }

    pub(crate) fn sorted(mut servers: Vec<CdnServer>, server_mode: ServerMode) -> Self {
        sort_by_load(&mut servers);

        let (cache, cdn): (Vec<_>, Vec<_>) = servers
            .into_iter()
            .partition(|s| s.server_type == "SteamCache");

        for (i, s) in cache.iter().chain(cdn.iter()).enumerate() {
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

        let health = Arc::new(Mutex::new(
            cache
                .iter()
                .chain(cdn.iter())
                .map(|s| (s.vhost.clone(), ServerHealth::default()))
                .collect(),
        ));

        Self {
            cache,
            cdn,
            cursor: Arc::new(AtomicUsize::new(0)),
            health,
            server_mode,
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self.server_mode {
            ServerMode::Cdn => self.cdn.len(),
            ServerMode::Cache => self.cache.len(),
            ServerMode::All => self.cache.len() + self.cdn.len(),
        }
    }

    pub(crate) fn report_success(&self, server_host: &str) {
        let mut health = self.health.lock().unwrap();
        if let Some(h) = health.get_mut(server_host) {
            h.consecutive_failures = 0;
            h.blacklisted = false;
            h.blacklisted_at = None;
            h.blacklist_count = 0;
        }
    }

    pub(crate) fn report_failure(&self, server_host: &str) {
        let mut health = self.health.lock().unwrap();
        let h = health.entry(server_host.to_string()).or_default();
        h.consecutive_failures += 1;
        if !h.blacklisted && h.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            h.blacklisted = true;
            h.blacklisted_at = Some(Instant::now());
            h.blacklist_count += 1;
            tracing::warn!(
                "Server {} blacklisted after {} consecutive failures (count={}, timeout={:?})",
                server_host,
                h.consecutive_failures,
                h.blacklist_count,
                blacklist_timeout(h.blacklist_count)
            );
        }
    }

    pub(crate) fn is_healthy(&self, server_host: &str) -> bool {
        let health = self.health.lock().unwrap();
        self.is_server_healthy(&health, server_host)
    }

    pub(crate) fn healthy_count(&self) -> usize {
        let health = self.health.lock().unwrap();
        self.cache
            .iter()
            .chain(self.cdn.iter())
            .filter(|s| self.is_server_healthy(&health, &s.vhost))
            .count()
    }

    fn is_server_healthy(&self, health: &HashMap<String, ServerHealth>, server_host: &str) -> bool {
        health
            .get(server_host)
            .map(|h| {
                if !h.blacklisted {
                    return true;
                }
                if let Some(blacklisted_at) = h.blacklisted_at {
                    blacklisted_at.elapsed() >= blacklist_timeout(h.blacklist_count)
                } else {
                    true
                }
            })
            .unwrap_or(true)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &str> + '_ {
        let iter: Box<dyn Iterator<Item = &CdnServer> + '_> = match self.server_mode {
            ServerMode::Cdn => Box::new(self.cdn.iter()),
            ServerMode::Cache => Box::new(self.cache.iter()),
            ServerMode::All => Box::new(self.cache.iter().chain(self.cdn.iter())),
        };
        iter.map(|s| s.vhost.as_str())
    }
}

impl Default for CdnServerList {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> RoundRobin<'a> {
    pub(crate) fn new(list: &'a CdnServerList) -> Self {
        Self { list }
    }

    pub(crate) fn len(&self) -> usize {
        self.list.len()
    }

    pub(crate) fn next(&self) -> Result<&str> {
        let total = self.list.len();
        if total == 0 {
            return Err(SteamVentError::Cdn(CdnError::NoServers));
        }

        let cache = &self.list.cache;
        let cdn = &self.list.cdn;

        let (primary, fallback) = match self.list.server_mode {
            ServerMode::Cache => (cache, None),
            ServerMode::Cdn => (cdn, None),
            ServerMode::All => (cache, Some(cdn)),
        };

        let start = self.list.cursor.fetch_add(1, Ordering::Relaxed);

        for offset in 0..primary.len() {
            let idx = start.wrapping_add(offset) % primary.len();
            let vhost = &primary[idx].vhost;
            if self.list.is_healthy(vhost) {
                return Ok(vhost);
            }
        }

        if let Some(fallback) = fallback {
            for offset in 0..fallback.len() {
                let idx = start.wrapping_add(offset) % fallback.len();
                let vhost = &fallback[idx].vhost;
                if self.list.is_healthy(vhost) {
                    return Ok(vhost);
                }
            }
        }

        Err(SteamVentError::Cdn(CdnError::AllServersFailed))
    }

    pub(crate) fn report_success(&self, server_host: &str) {
        self.list.report_success(server_host);
    }

    pub(crate) fn report_failure(&self, server_host: &str) {
        self.list.report_failure(server_host);
    }

    pub(crate) fn healthy_count(&self) -> usize {
        self.list.healthy_count()
    }
}
