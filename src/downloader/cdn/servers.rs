use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::config::ServerMode;
use crate::connection::{ConnectionExt, SteamSession};
use crate::error::{DownloadError, Result};
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
pub(crate) struct ContentServer {
    pub vhost: String,
    pub server_type: String,
    pub weighted_load: f32,
    pub load: i32,
    pub priority_class: u32,
}

#[derive(Debug, Clone)]
pub struct DepotContext {
    pub(crate) servers: ContentServerList,
    pub depot_id: u32,
    pub depot_key: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ContentServerList {
    cache_servers: Vec<ContentServer>,
    cdn_servers: Vec<ContentServer>,
    cursor: Arc<AtomicUsize>,
    health: Arc<Mutex<HashMap<String, ServerHealth>>>,
    server_mode: ServerMode,
}

pub(crate) struct RoundRobin<'a> {
    list: &'a ContentServerList,
}

fn sort_by_load(servers: &mut [ContentServer]) {
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

impl ContentServerList {
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
            return Err(DownloadError::NoServers)?;
        }

        let servers: Vec<ContentServer> = srv_resp
            .servers
            .into_iter()
            .map(|s| ContentServer {
                vhost: s.vhost().to_string(),
                server_type: s.type_().to_string(),
                weighted_load: s.weighted_load(),
                load: s.load(),
                priority_class: s.priority_class(),
            })
            .collect();

        Ok(Self::from_servers(servers, server_mode))
    }

    pub(crate) fn from_servers(mut servers: Vec<ContentServer>, server_mode: ServerMode) -> Self {
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
            cache_servers: cache,
            cdn_servers: cdn,
            cursor: Arc::new(AtomicUsize::new(0)),
            health,
            server_mode,
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self.server_mode {
            ServerMode::Cdn => self.cdn_servers.len(),
            ServerMode::Cache => self.cache_servers.len(),
            ServerMode::CachePreferred => self.cache_servers.len() + self.cdn_servers.len(),
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
        self.cache_servers
            .iter()
            .chain(self.cdn_servers.iter())
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
        let iter: Box<dyn Iterator<Item = &ContentServer> + '_> = match self.server_mode {
            ServerMode::Cdn => Box::new(self.cdn_servers.iter()),
            ServerMode::Cache => Box::new(self.cache_servers.iter()),
            ServerMode::CachePreferred => {
                Box::new(self.cache_servers.iter().chain(self.cdn_servers.iter()))
            }
        };
        iter.map(|s| s.vhost.as_str())
    }
}

impl<'a> RoundRobin<'a> {
    pub(crate) fn new(list: &'a ContentServerList) -> Self {
        Self { list }
    }

    pub(crate) fn next(&self) -> Result<&str> {
        let total = self.list.len();
        if total == 0 {
            return Err(DownloadError::NoServers)?;
        }

        let cache = &self.list.cache_servers;
        let cdn = &self.list.cdn_servers;

        let (primary, fallback) = match self.list.server_mode {
            ServerMode::Cache => (cache, None),
            ServerMode::Cdn => (cdn, None),
            ServerMode::CachePreferred => (cache, Some(cdn)),
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

        Err(DownloadError::AllServersFailed)?
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SteamVentError;

    fn server(vhost: &str, server_type: &str, weighted_load: f32) -> ContentServer {
        ContentServer {
            vhost: vhost.to_string(),
            server_type: server_type.to_string(),
            weighted_load,
            load: 0,
            priority_class: 0,
        }
    }

    #[test]
    fn sort_by_load_orders_ascending() {
        let mut servers = vec![
            server("c", "SteamCache", 3.0),
            server("a", "SteamCache", 1.0),
            server("b", "SteamCache", 2.0),
        ];
        sort_by_load(&mut servers);
        let names: Vec<&str> = servers.iter().map(|s| s.vhost.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn sort_by_load_handles_nan_without_panic() {
        let mut servers = vec![
            server("a", "SteamCache", f32::NAN),
            server("b", "SteamCache", 1.0),
        ];
        sort_by_load(&mut servers);
        // Must not panic; exact order for NaN is unspecified but stable
    }

    #[test]
    fn blacklist_timeout_exponential_backoff() {
        assert_eq!(blacklist_timeout(1), Duration::from_secs(10));
        assert_eq!(blacklist_timeout(2), Duration::from_secs(20));
        assert_eq!(blacklist_timeout(3), Duration::from_secs(40));
        assert_eq!(blacklist_timeout(4), Duration::from_secs(80));
    }

    #[test]
    fn from_servers_partitions_cdn_and_cache() {
        let servers = vec![
            server("cdn1", "CDN", 5.0),
            server("cache1", "SteamCache", 1.0),
            server("cache2", "SteamCache", 3.0),
            server("cdn2", "CDN", 2.0),
        ];
        let list = ContentServerList::from_servers(servers, ServerMode::CachePreferred);

        assert_eq!(list.len(), 4);
        // Verify cache servers are sorted by load
        let cache_hosts: Vec<&str> = list
            .cache_servers
            .iter()
            .map(|s| s.vhost.as_str())
            .collect();
        assert_eq!(cache_hosts, vec!["cache1", "cache2"]);
    }

    #[test]
    fn report_failure_blacklists_after_three() {
        let list = ContentServerList::from_servers(
            vec![server("s1", "SteamCache", 1.0)],
            ServerMode::CachePreferred,
        );

        assert!(list.is_healthy("s1"));
        list.report_failure("s1");
        assert!(list.is_healthy("s1"));
        list.report_failure("s1");
        assert!(list.is_healthy("s1"));
        list.report_failure("s1");
        assert!(!list.is_healthy("s1"));
    }

    #[test]
    fn report_success_resets_blacklist() {
        let list = ContentServerList::from_servers(
            vec![server("s1", "SteamCache", 1.0)],
            ServerMode::CachePreferred,
        );

        list.report_failure("s1");
        list.report_failure("s1");
        list.report_failure("s1");
        assert!(!list.is_healthy("s1"));

        list.report_success("s1");
        assert!(list.is_healthy("s1"));
    }

    #[test]
    fn round_robin_skips_blacklisted_servers() {
        let list = ContentServerList::from_servers(
            vec![
                server("bad", "SteamCache", 1.0),
                server("good1", "SteamCache", 2.0),
                server("good2", "SteamCache", 3.0),
            ],
            ServerMode::CachePreferred,
        );

        // Blacklist "bad"
        list.report_failure("bad");
        list.report_failure("bad");
        list.report_failure("bad");

        let rr = RoundRobin::new(&list);
        let picks: Vec<&str> = (0..4).filter_map(|_| rr.next().ok()).collect();
        // Should only pick good1 and good2, alternating
        for pick in &picks {
            assert_ne!(*pick, "bad");
        }
        assert!(picks.len() == 4);
    }

    #[test]
    fn round_robin_all_blacklisted_returns_error() {
        let list = ContentServerList::from_servers(
            vec![server("s1", "SteamCache", 1.0)],
            ServerMode::CachePreferred,
        );
        list.report_failure("s1");
        list.report_failure("s1");
        list.report_failure("s1");

        let rr = RoundRobin::new(&list);
        assert!(matches!(
            rr.next(),
            Err(SteamVentError::Download(DownloadError::AllServersFailed))
        ));
    }

    #[test]
    fn round_robin_empty_returns_no_servers() {
        let list = ContentServerList::from_servers(vec![], ServerMode::CachePreferred);
        let rr = RoundRobin::new(&list);
        assert!(matches!(
            rr.next(),
            Err(SteamVentError::Download(DownloadError::NoServers))
        ));
    }

    #[test]
    fn round_robin_cache_preferred_falls_back_to_cdn() {
        let list = ContentServerList::from_servers(
            vec![
                server("cache", "SteamCache", 1.0),
                server("cdn", "CDN", 2.0),
            ],
            ServerMode::CachePreferred,
        );

        // Blacklist cache
        list.report_failure("cache");
        list.report_failure("cache");
        list.report_failure("cache");

        let rr = RoundRobin::new(&list);
        let picks: Vec<&str> = (0..3).filter_map(|_| rr.next().ok()).collect();
        // Should only pick CDN server
        for pick in &picks {
            assert_eq!(*pick, "cdn");
        }
    }

    #[test]
    fn server_mode_cdn_ignores_cache_servers() {
        let list = ContentServerList::from_servers(
            vec![
                server("cache", "SteamCache", 1.0),
                server("cdn", "CDN", 2.0),
            ],
            ServerMode::Cdn,
        );

        assert_eq!(list.len(), 1);
        let rr = RoundRobin::new(&list);
        let picks: Vec<&str> = (0..3).filter_map(|_| rr.next().ok()).collect();
        for pick in &picks {
            assert_eq!(*pick, "cdn");
        }
    }

    #[test]
    fn server_mode_cache_ignores_cdn_servers() {
        let list = ContentServerList::from_servers(
            vec![
                server("cache", "SteamCache", 1.0),
                server("cdn", "CDN", 2.0),
            ],
            ServerMode::Cache,
        );

        assert_eq!(list.len(), 1);
        let rr = RoundRobin::new(&list);
        let picks: Vec<&str> = (0..3).filter_map(|_| rr.next().ok()).collect();
        for pick in &picks {
            assert_eq!(*pick, "cache");
        }
    }

    #[test]
    fn round_robin_rotates() {
        let list = ContentServerList::from_servers(
            vec![
                server("a", "SteamCache", 1.0),
                server("b", "SteamCache", 2.0),
            ],
            ServerMode::CachePreferred,
        );

        let rr = RoundRobin::new(&list);
        assert_eq!(rr.next().unwrap(), "a");
        assert_eq!(rr.next().unwrap(), "b");
        assert_eq!(rr.next().unwrap(), "a");
    }
}
