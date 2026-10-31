//! Node client modules. Game caches/logs contain public data; wallets hold keys separately.
pub mod audit;
pub mod client;
pub mod consolidation;
pub mod fees;
pub mod fee_policy;
pub mod hosted;
pub mod invitations;
pub mod pipeline;
pub mod reservations;
pub mod rpc;
pub mod setup;
pub mod tracker;
pub mod wallet;
use super::engine::*;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub endpoint: String,
    pub network: String,
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
    pub pending_timeout_secs: u64,
    pub coinbase_maturity_daa: u64,
    pub allow_unsynced: bool,
    /// Explicit public invitation directory; no third-party service is implicit.
    pub invitation_index: Option<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            endpoint: "grpc://127.0.0.1:16610".into(),
            network: "devnet".into(),
            resolver_urls: Vec::new(),
            fallback_endpoints: Vec::new(),
            broadcast: Default::default(),
            poll_ms: 100,
            fee_sompi: 25_000_000,
            game_fee_cap: false,
            relay_fee_per_kg: 100_000,
            fee_budget_sompi: 1_000_000_000,
            pending_timeout_secs: 30,
            coinbase_maturity_daa: 1000,
            allow_unsynced: false,
            invitation_index: None,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        self.broadcast.validate()?;
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
            || self.pending_timeout_secs < 5
        {
            return Err("invalid RPC endpoint, polling interval, fees or pending timeout".into());
        }
        self.network.parse::<kaspa_consensus_core::network::NetworkId>()?;
        Ok(())
    }
}
pub fn load<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(&serde_json::to_vec_pretty(value)?)?;
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
