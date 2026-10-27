//! Session fee policy and background node estimates.
use super::{Config, rpc::startup::Connection};
use std::{collections::HashMap, sync::{Mutex, OnceLock, atomic::{AtomicU8, Ordering}}, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeePolicy { Priority, Normal, Economy }
impl FeePolicy {
    pub fn label(self) -> &'static str { match self { Self::Priority => "Priority", Self::Normal => "Normal", Self::Economy => "Economy" } }
}
static POLICY: AtomicU8 = AtomicU8::new(0);
pub fn policy() -> FeePolicy { match POLICY.load(Ordering::Relaxed) { 0 => FeePolicy::Priority, 2 => FeePolicy::Economy, _ => FeePolicy::Normal } }
pub fn set_policy(policy: FeePolicy) { POLICY.store(match policy { FeePolicy::Priority => 0, FeePolicy::Normal => 1, FeePolicy::Economy => 2 }, Ordering::Relaxed); }
fn rates() -> &'static Mutex<HashMap<String, [u64; 3]>> { static RATES: OnceLock<Mutex<HashMap<String, [u64; 3]>>> = OnceLock::new(); RATES.get_or_init(|| Mutex::new(HashMap::new())) }
pub fn source(network: &str) -> &'static str {
    if rates().lock().is_ok_and(|rates| rates.contains_key(network)) { "cached node estimate" } else { "configured fallback" }
}
fn convert(rate: f64) -> Option<u64> {
    let rate = rate * 1000.0;
    (rate.is_finite() && rate > 0.0 && rate < u64::MAX as f64).then(|| rate.ceil() as u64)
}
pub fn selected_rate(config: &Config) -> u64 {
    rate_for(config, policy())
}
pub fn rate_for(config: &Config, policy: FeePolicy) -> u64 {
    let index = match policy { FeePolicy::Priority => 0, FeePolicy::Normal => 1, FeePolicy::Economy => 2 };
    rates().lock().ok().and_then(|rates| rates.get(&config.network).map(|r| r[index])).unwrap_or(config.relay_fee_per_kg)
        .max(config.relay_fee_per_kg)
}
pub fn start(mut connection: Connection, config: Config) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut peer = None;
        loop {
            let estimate = tokio::time::timeout(Duration::from_secs(5), async {
                if peer.is_none() { peer = Some(connection.get(&config).await.map_err(|e| e.to_string())?); }
                peer.as_ref().expect("node acquired").rpc.get_fee_estimate().await.map_err(|e| e.to_string())
            }).await;
            if let Ok(Ok(estimate)) = estimate {
                let buckets = [Some(estimate.priority_bucket), estimate.normal_buckets.first().copied(), estimate.low_buckets.first().copied()];
                if let [Some(priority), Some(normal), Some(economy)] = buckets.map(|bucket| bucket.and_then(|bucket| convert(bucket.feerate))) {
                    if let Ok(mut rates) = rates().lock() { rates.insert(config.network.clone(), [priority, normal, economy]); }
                }
            } else { peer = None; }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rates_round_up_and_reject_invalid_values() {
        assert_eq!(convert(100.0001), Some(100_001));
        assert_eq!(convert(f64::NAN), None);
        assert_eq!(convert(-1.0), None);
        assert_eq!(convert(f64::INFINITY), None);
    }
    #[test]
    fn game_fee_caps_do_not_reject_expensive_estimates() {
        use kaspa_consensus_core::tx::Transaction;
        let tx = Transaction::new(0, vec![], vec![], 0, Default::default(), 0, vec![]);
        let mut config = Config { fee_sompi: 10_000_000, game_fee_cap: true, ..Default::default() };
        assert_eq!(super::super::client::relay_fee_at_rate(&tx, &config, 1_000_000_000_000).unwrap(), 10_000_000);
        config.fee_sompi = 50_000_000;
        assert_eq!(super::super::client::relay_fee_at_rate(&tx, &config, 1_000_000_000_000).unwrap(), 50_000_000);
        config.game_fee_cap = false;
        assert!(super::super::client::relay_fee_at_rate(&tx, &config, 1_000_000_000_000).is_err());
    }
}
