//! Fee calculation and timestamps shared by the node client.
use super::Config;
use crate::engine::{Result, transaction_mass};
use kaspa_consensus_core::tx::{Transaction, UtxoEntry};

pub fn transaction_fee(tx: &Transaction, inputs: &[UtxoEntry]) -> Result<u64> {
    if inputs.len() != tx.inputs.len() {
        return Err("fee calculation lacks source entries".into());
    }
    let sum = |values: Vec<u64>| values.into_iter().try_fold(0u64, |total, v| total.checked_add(v).ok_or("amount overflow"));
    let input = sum(inputs.iter().map(|u| u.amount).collect())?;
    let output = sum(tx.outputs.iter().map(|o| o.value).collect())?;
    Ok(input.checked_sub(output).ok_or("outputs exceed inputs")?)
}

pub fn relay_fee(tx: &Transaction, config: &Config) -> Result<u64> {
    relay_fee_at_rate(tx, config, super::fee_policy::selected_rate(config))
}

pub(crate) fn relay_fee_at_rate(tx: &Transaction, config: &Config, rate: u64) -> Result<u64> {
    let masses = transaction_mass(tx)?;
    let price = |mass: u64, rate: u64| {
        ((u128::from(mass) * u128::from(rate)).div_ceil(1000).min(u128::from(u64::MAX)) as u64).max(if mass == 0 { rate } else { 1 })
    };
    let minimum = price(masses.relay, config.relay_fee_per_kg);
    let fee = price(masses.priority, rate).max(minimum);
    if fee > config.fee_sompi {
        if config.game_fee_cap && minimum <= config.fee_sompi {
            return Ok(config.fee_sompi);
        }
        return Err(format!("estimated fee {fee} exceeds fee_sompi cap {}; raise the cap explicitly", config.fee_sompi).into());
    }
    Ok(fee)
}

pub(crate) fn priced_transaction_at_rate(
    config: &Config,
    rate: u64,
    mut build: impl FnMut(u64) -> Result<Transaction>,
) -> Result<Transaction> {
    // A trial at the cap can create tiny change and fail storage validation,
    // although the actual relay-priced transaction is valid. Start low and
    // converge before authorizing/submitting; relay_fee still enforces the cap.
    let mut fee = 1;
    for _ in 0..32 {
        let tx = build(fee)?;
        let required = relay_fee_at_rate(&tx, config, rate)?;
        if fee >= required {
            return Ok(tx);
        }
        fee = required;
    }
    Err("transaction fee did not stabilize".into())
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().min(u128::from(u64::MAX)) as u64
}
