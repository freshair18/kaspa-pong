//! Node client modules. Game caches/logs contain public data; wallets hold keys separately.
pub mod audit;
pub mod client;
pub mod fee_policy;
pub mod fees;
pub mod invitations;
pub mod queue_book;
pub mod queue_codes;
pub mod queue_service;
pub mod queue_sets;
pub mod queue_tracker;
pub mod reservations;
pub mod rpc;
pub mod tracker;
pub mod wallet;
use super::engine::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// RPC endpoints dedicated to one network. Switching networks never carries
/// resolver or fallback endpoints from another network implicitly.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkEndpoints {
    pub endpoint: String,
    pub resolver_urls: Vec<String>,
    pub fallback_endpoints: Vec<String>,
}
impl Default for NetworkEndpoints {
    fn default() -> Self {
        Self { endpoint: "resolver".into(), resolver_urls: Vec::new(), fallback_endpoints: Vec::new() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub endpoint: String,
    pub network: String,
    /// Optional endpoint sets keyed by Kaspa network name.
    pub network_endpoints: HashMap<String, NetworkEndpoints>,
    /// Optional explicit resolver services. Empty uses the upstream PNN list.
    pub resolver_urls: Vec<String>,
    /// Only these explicitly configured endpoints may be used as fallbacks.
    pub fallback_endpoints: Vec<String>,
    /// Concurrent RPC submission. Observation still uses one qualified node.
    pub broadcast: rpc::broadcast::Settings,
    pub poll_ms: u64,
    pub fee_sompi: u64,
    /// Runtime-only: cap game fees without blocking submission.
    #[serde(skip)]
    pub game_fee_cap: bool,
    /// Relay rate in sompi per 1,000 normalized mass units. fee_sompi is a cap.
    pub relay_fee_per_kg: u64,
    pub fee_budget_sompi: u64,
    /// Local refill target per reusable queue set, in whole KAS.
    pub queue_reserve_target_kas: u64,
    pub coinbase_maturity_daa: u64,
    pub allow_unsynced: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            endpoint: "resolver".into(),
            network: "mainnet".into(),
            network_endpoints: HashMap::new(),
            resolver_urls: Vec::new(),
            fallback_endpoints: Vec::new(),
            broadcast: Default::default(),
            poll_ms: 100,
            fee_sompi: 25_000_000,
            game_fee_cap: false,
            relay_fee_per_kg: 100_000,
            fee_budget_sompi: 1_000_000_000,
            queue_reserve_target_kas: crate::reusable::DEFAULT_QUEUE_RESERVE / 100_000_000,
            coinbase_maturity_daa: 1000,
            allow_unsynced: false,
        }
    }
}
impl Config {
    pub fn queue_reserve_target(&self) -> Result<u64> {
        Self::reserve_target_sompi(self.queue_reserve_target_kas)
    }
    fn reserve_target_sompi(kas: u64) -> Result<u64> {
        kas.checked_mul(100_000_000)
            .filter(|&target| (300_000_000..=i64::MAX as u64 - 50_800_000_000).contains(&target))
            .ok_or_else(|| "queue_reserve_target_kas must be at least 3 KAS and fit in the supported balance range".into())
    }
    pub fn validate(&self) -> Result<()> {
        self.broadcast.validate()?;
        self.queue_reserve_target()?;
        let valid_endpoint =
            |e: &str| e == "resolver" || e.starts_with("grpc://") || e.starts_with("wss://") || e.starts_with("ws://");
        if !valid_endpoint(&self.endpoint)
            || self.fallback_endpoints.iter().any(|e| !valid_endpoint(e) || e == "resolver")
            || self.resolver_urls.iter().any(|u| !u.starts_with("https://") && !u.starts_with("http://"))
            || self.poll_ms < 25
            || self.poll_ms > 60_000
            || self.fee_sompi == 0
            || self.relay_fee_per_kg == 0
            || self.fee_budget_sompi < self.fee_sompi
        {
            return Err("invalid RPC endpoint, polling interval or fees".into());
        }
        self.network.parse::<kaspa_consensus_core::network::NetworkId>()?;
        for (network, endpoints) in &self.network_endpoints {
            network.parse::<kaspa_consensus_core::network::NetworkId>()?;
            if !valid_endpoint(&endpoints.endpoint)
                || endpoints.fallback_endpoints.iter().any(|e| !valid_endpoint(e) || e == "resolver")
                || endpoints.resolver_urls.iter().any(|u| !u.starts_with("https://") && !u.starts_with("http://"))
            {
                return Err(format!("invalid RPC endpoints configured for network {network}").into());
            }
        }
        Ok(())
    }
}
pub fn load<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
fn save_queue_reserve_target(path: &Path, kas: u64) -> Result<u64> {
    let target = Config::reserve_target_sompi(kas)?;
    let mut document: serde_json::Value = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::from_str(include_str!("../../config/public-node.example.json"))?
        }
        Err(error) => return Err(error.into()),
    };
    document
        .as_object_mut()
        .ok_or("node configuration must be a JSON object")?
        .insert("queue_reserve_target_kas".into(), serde_json::Value::from(kas));
    save(path, &document)?;
    Ok(target)
}
pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    save_json(path, &serde_json::to_vec_pretty(value)?)
}
fn save_json(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
pub fn lock(path: &Path) -> Result<File> {
    let mut name = PathBuf::from(path);
    name.set_extension("lock");
    if let Some(parent) = name.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().write(true).create(true).truncate(false).open(name)?;
    file.try_lock().map_err(|_| "another client is using this state file")?;
    Ok(file)
}
pub fn public(text: &str) -> Result<Vec<u8>> {
    Ok(text.parse::<secp256k1::XOnlyPublicKey>()?.serialize().to_vec())
}

#[cfg(test)]
mod reserve_config_tests {
    use super::*;

    #[test]
    fn saving_the_target_preserves_other_settings_and_rejects_invalid_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.json");
        assert_eq!(save_queue_reserve_target(&path, 8).unwrap(), 800_000_000);
        let config: Config = load(&path).unwrap();
        assert_eq!(config.network, "mainnet");
        assert_eq!(config.queue_reserve_target().unwrap(), 800_000_000);

        let original = serde_json::json!({"network": "devnet", "poll_ms": 120});
        save(&path, &original).unwrap();
        assert!(save_queue_reserve_target(&path, 2).is_err());
        assert_eq!(load::<serde_json::Value>(&path).unwrap(), original);
        save_queue_reserve_target(&path, 10).unwrap();
        assert_eq!(
            load::<serde_json::Value>(&path).unwrap(),
            serde_json::json!({"network": "devnet", "poll_ms": 120, "queue_reserve_target_kas": 10})
        );
    }
}
