use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Deserialize;

/// Configuration for distributing agent work to remote SSH worker hosts.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// List of SSH host strings (e.g. `["host1:22", "host2:2222"]`).
    /// When empty, all work runs locally.
    pub hosts: Vec<String>,

    /// Maximum number of concurrent agents per host. When `None`, the
    /// orchestrator's global `max_concurrent_agents` applies to each host.
    pub per_host_concurrency: Option<usize>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            hosts: Vec::new(),
            per_host_concurrency: None,
        }
    }
}

/// Resolved target for a piece of work: either run locally or on a specific
/// remote host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerTarget {
    /// Execute on the local machine (no SSH).
    Local,
    /// Execute on the given remote SSH host string.
    Remote(String),
}

/// Strategy for selecting which worker host to assign work to.
#[derive(Debug)]
pub struct WorkerSelector {
    hosts: Vec<String>,
    /// Round-robin counter for distributing work across hosts.
    next_index: AtomicUsize,
}

impl WorkerSelector {
    /// Create a new selector from the worker configuration.
    ///
    /// If no hosts are configured, `select()` always returns `WorkerTarget::Local`.
    pub fn new(config: &WorkerConfig) -> Self {
        Self {
            hosts: config.hosts.clone(),
            next_index: AtomicUsize::new(0),
        }
    }

    /// Select the next worker target using round-robin distribution.
    ///
    /// Returns `WorkerTarget::Local` when no remote hosts are configured.
    pub fn select(&self) -> WorkerTarget {
        if self.hosts.is_empty() {
            return WorkerTarget::Local;
        }

        let index = self.next_index.fetch_add(1, Ordering::Relaxed) % self.hosts.len();
        WorkerTarget::Remote(self.hosts[index].clone())
    }

    /// Returns `true` when there are no remote hosts configured and all work
    /// will be executed locally.
    pub fn is_local_only(&self) -> bool {
        self.hosts.is_empty()
    }

    /// Returns the list of configured remote hosts.
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }
}

/// Raw deserialization struct for reading worker config from YAML.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct RawWorkerConfig {
    pub hosts: Option<Vec<String>>,
    pub per_host_concurrency: Option<usize>,
}

impl RawWorkerConfig {
    /// Convert into the effective `WorkerConfig`, filtering out blank hosts.
    pub fn into_effective(self) -> WorkerConfig {
        let hosts = self
            .hosts
            .unwrap_or_default()
            .into_iter()
            .filter(|h| !h.trim().is_empty())
            .collect();

        WorkerConfig {
            hosts,
            per_host_concurrency: self.per_host_concurrency.filter(|&v| v > 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_worker_config_is_local() {
        let config = WorkerConfig::default();
        assert!(config.hosts.is_empty());
        assert!(config.per_host_concurrency.is_none());
    }

    #[test]
    fn selector_returns_local_when_no_hosts() {
        let config = WorkerConfig::default();
        let selector = WorkerSelector::new(&config);
        assert!(selector.is_local_only());
        assert_eq!(selector.select(), WorkerTarget::Local);
    }

    #[test]
    fn selector_round_robin_distribution() {
        let config = WorkerConfig {
            hosts: vec!["host-a".to_owned(), "host-b".to_owned(), "host-c".to_owned()],
            per_host_concurrency: None,
        };
        let selector = WorkerSelector::new(&config);

        assert!(!selector.is_local_only());
        assert_eq!(selector.select(), WorkerTarget::Remote("host-a".to_owned()));
        assert_eq!(selector.select(), WorkerTarget::Remote("host-b".to_owned()));
        assert_eq!(selector.select(), WorkerTarget::Remote("host-c".to_owned()));
        // Wraps around
        assert_eq!(selector.select(), WorkerTarget::Remote("host-a".to_owned()));
    }

    #[test]
    fn selector_single_host() {
        let config = WorkerConfig {
            hosts: vec!["only-host".to_owned()],
            per_host_concurrency: Some(4),
        };
        let selector = WorkerSelector::new(&config);

        assert_eq!(selector.select(), WorkerTarget::Remote("only-host".to_owned()));
        assert_eq!(selector.select(), WorkerTarget::Remote("only-host".to_owned()));
    }

    #[test]
    fn raw_config_filters_blank_hosts() {
        let raw = RawWorkerConfig {
            hosts: Some(vec![
                "host-a".to_owned(),
                "  ".to_owned(),
                "".to_owned(),
                "host-b".to_owned(),
            ]),
            per_host_concurrency: Some(0),
        };
        let effective = raw.into_effective();
        assert_eq!(effective.hosts, vec!["host-a", "host-b"]);
        assert_eq!(effective.per_host_concurrency, None); // 0 filtered out
    }

    #[test]
    fn raw_config_default_is_empty() {
        let raw = RawWorkerConfig::default();
        let effective = raw.into_effective();
        assert!(effective.hosts.is_empty());
        assert!(effective.per_host_concurrency.is_none());
    }
}
