//! Durable public inventory. A submitted transaction never makes a set idle;
//! only an accepted, validated successor does. No private keys are stored here.
use super::{Config, reservations, rpc::Node};
use crate::{
    engine::*,
    reusable::{self, Set},
};
use argent_runtime::TxBuilder;
use kaspa_consensus_core::{
    Hash, config::params::MAINNET_PARAMS, mass::{UtxoCell, calc_storage_mass},
    tx::{Transaction, TransactionOutpoint, UtxoEntry},
};
use secp256k1::Keypair;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Phase {
    Available,
    Reserved(Hash),
    Active(Hash),
    Settling(Hash),
    Unavailable,
    /// Only read from v40 journals written before archived sets were dropped.
    #[serde(rename = "Legacy")]
    Archived,
    Closed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub set: Set,
    pub phase: Phase,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Authorization {
    pub transaction: Transaction,
    pub inputs: Vec<UtxoEntry>,
    pub purpose: String,
    pub successors: Vec<Set>,
    /// Queue creation is counted before submission, including uncertain sends.
    pub creates_set: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inventory {
    version: u32,
    pub network: String,
    pub owner: Vec<u8>,
    pub records: Vec<Record>,
    pub pending: Vec<Authorization>,
    pub created_count: u64,
}
impl Inventory {
    pub fn new(network: String, owner: Vec<u8>) -> Self {
        Self { version: 1, network, owner, records: vec![], pending: vec![], created_count: 0 }
    }
    pub fn load(path: &Path, network: &str, owner: &[u8]) -> Result<Self> {
        let result = if path.exists() { super::load(path)? } else { Self::new(network.into(), owner.to_vec()) };
        result.validate(network, owner)?;
        result.restore(path)?;
        Ok(result)
    }
    pub(crate) fn validate(&self, network: &str, owner: &[u8]) -> Result<()> {
        if self.version != 1 || self.network != network || self.owner != owner {
            return Err("queue inventory belongs to another wallet, network or version".into());
        }
        let (_, b) = reusable::current_runtime()?;
        let mut ids = std::collections::HashSet::new();
        for r in &self.records {
            if r.phase != Phase::Closed {
                r.set.validate(b)?;
            }
            if r.set.lanes[0].state.queue.owner != owner || !ids.insert(r.set.id()?) {
                return Err("invalid or duplicate queue identity in inventory".into());
            }
            if r.phase == Phase::Available && !r.set.idle() {
                return Err("active set marked available".into());
            }
        }
        for p in &self.pending {
            p.validate(b, owner)?;
        }
        Ok(())
    }
    pub fn available(&self) -> impl Iterator<Item = &Set> {
        self.records
            .iter()
            .filter(|r| {
                r.phase == Phase::Available
                    && r.set.idle()
                    && !self
                        .pending
                        .iter()
                        .any(|p| p.transaction.inputs.iter().any(|i| r.set.lanes.iter().any(|c| c.outpoint == i.previous_outpoint)))
            })
            .map(|r| &r.set)
    }
}

impl Authorization {
    pub(super) fn validate(&self, b: &TxBuilder<'_>, owner: &[u8]) -> Result<()> {
        if self.transaction.inputs.len() != self.inputs.len() {
            return Err("incomplete queue authorization".into());
        }
        if storage_mass(&self.transaction, self.inputs.clone())? != self.transaction.storage_mass() {
            return Err("queue authorization has an invalid storage commitment".into());
        }
        transaction_mass(&self.transaction)?;
        for i in 0..self.inputs.len() {
            argent_runtime::execute_input_with_covenants(&self.transaction, self.inputs.clone(), i)?;
        }
        for set in &self.successors {
            set.validate(b)?;
            if set.lanes[0].state.queue.owner != owner {
                return Err("queue successor belongs to another wallet".into());
            }
            for cell in &set.lanes {
                let output = self.transaction.outputs.get(cell.outpoint.index as usize)
                    .ok_or("queue successor index outside transaction")?;
                if cell.outpoint.transaction_id != self.transaction.id()
                    || cell.utxo.is_coinbase
                    || cell.utxo.amount != output.value
                    || cell.utxo.script_public_key != output.script_public_key
                    || cell.utxo.covenant_id != output.covenant.map(|binding| binding.covenant_id)
                {
                    return Err("queue successor does not match signed authorization".into());
                }
            }
        }
        Ok(())
    }
}

impl Inventory {
    pub fn first_creation(&self) -> bool {
        self.records.iter().all(|r| r.phase == Phase::Closed) && !self.pending.iter().any(|p| p.creates_set)
    }
    fn reservations(&self) -> Vec<TransactionOutpoint> {
        self.records
            .iter()
            .filter(|r| r.phase != Phase::Closed)
            .flat_map(|r| r.set.lanes.iter().map(|c| c.outpoint))
            .chain(self.pending.iter().flat_map(|p| p.transaction.inputs.iter().map(|i| i.previous_outpoint)))
            .collect()
    }
    pub fn restore(&self, path: &Path) -> Result<()> {
        reservations::restore(path, self.reservations())
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        super::save(path, self)?;
        self.restore(path)
    }
    /// Caller holds the inventory lock and the input claim across this write.
    /// An uncertain submission is resumed from this exact signed authorization.
    pub fn authorize(&mut self, path: &Path, p: Authorization) -> Result<()> {
        if self.pending.iter().any(|old| old.transaction.id() == p.transaction.id()) {
            return Ok(());
        }
        if self.pending.iter().any(|old| {
            old.transaction.inputs.iter().any(|i| p.transaction.inputs.iter().any(|j| i.previous_outpoint == j.previous_outpoint))
        }) {
            return Err("queue operation conflicts with a pending authorization".into());
        }
        let mut candidate = self.clone();
        if p.creates_set {
            candidate.created_count = candidate.created_count.checked_add(1).ok_or("queue count overflow")?;
        }
        candidate.pending.push(p);
        candidate.validate(&self.network, &self.owner)?;
        candidate.save(path)?;
        *self = candidate;
        Ok(())
    }
    /// Reconcile exact accepted outpoints and scripts. Missing cells stay busy;
    /// this deliberately cannot guess a successful reset from elapsed time.
    pub async fn reconcile(&mut self, node: &Node, path: &Path) -> Result<()> {
        let a = reusable::artifact()?;
        let b = TxBuilder::new(&a)?;
        let scripts = self
            .records
            .iter()
            .flat_map(|r| r.set.lanes.iter().map(|c| c.utxo.script_public_key.clone()))
            .chain(
                self.pending
                    .iter()
                    .flat_map(|p| p.successors.iter().flat_map(|s| s.lanes.iter().map(|c| c.utxo.script_public_key.clone()))),
            )
            .collect::<Vec<_>>();
        if scripts.is_empty() {
            return Ok(());
        }
        let coins = node.utxos(&scripts).await?;
        self.reconcile_coins(&b, &coins)?;
        self.save(path)
    }
    fn reconcile_coins(&mut self, b: &TxBuilder<'_>, coins: &[Funding]) -> Result<()> {
        fn accepted(set: &Set, coins: &[Funding]) -> Option<Set> {
            let mut set = set.clone();
            for c in &mut set.lanes {
                let actual = coins.iter().find(|f| {
                    f.outpoint == c.outpoint
                        && f.utxo.amount == c.utxo.amount
                        && f.utxo.covenant_id == c.utxo.covenant_id
                        && f.utxo.script_public_key == c.utxo.script_public_key
                        && f.utxo.block_daa_score != u64::MAX
                })?;
                c.utxo = actual.utxo.clone();
            }
            Some(set)
        }
        for r in &mut self.records {
            if r.phase == Phase::Closed {
                continue;
            }
            if let Some(s) = accepted(&r.set, coins) {
                r.set = s;
                if r.set.idle() {
                    r.phase = Phase::Available;
                }
            } else {
                r.phase = Phase::Unavailable;
            }
        }
        let mut completed = vec![];
        for p in &self.pending {
            if p.successors.is_empty() {
                continue;
            }
            let next = p.successors.iter().map(|s| accepted(s, coins)).collect::<Option<Vec<_>>>();
            let Some(next) = next else {
                continue;
            };
            for s in next {
                s.validate(b)?;
                let phase = if s.idle() { Phase::Available } else { Phase::Reserved(s.lanes[0].state.game) };
                if let Some(r) = self.records.iter_mut().find(|r| r.set.id().ok() == s.id().ok()) {
                    *r = Record { set: s, phase };
                } else {
                    self.records.push(Record { set: s, phase });
                }
            }
            completed.push(p.transaction.id());
        }
        self.pending.retain(|p| !completed.contains(&p.transaction.id()));
        Ok(())
    }
}
pub fn path(config: &Config, owner: &Keypair) -> PathBuf {
    PathBuf::from("games").join(format!("queues-v{}-{}-{}.json", reusable::VERSION, config.network, owner.x_only_public_key().0))
}
/// Price direct funding against each complete signed candidate. Prefixes use
/// largest coins first; no preliminary merge and no wallet-wide sweep.
pub fn funded_transaction<T>(
    config: &Config,
    coins: &[Funding],
    mut make: impl FnMut(&[Funding], u64) -> Result<(Transaction, T)>,
) -> Result<(Transaction, T, Vec<Funding>, u64)> {
    let mut coins = coins.to_vec();
    coins.sort_by(|a, b| b.utxo.amount.cmp(&a.utxo.amount));
    funded_prefixes(config, &coins, super::fee_policy::selected_rate(config), &mut make)
}

pub(super) fn funded_prefixes<T>(
    config: &Config,
    coins: &[Funding],
    rate: u64,
    make: &mut impl FnMut(&[Funding], u64) -> Result<(Transaction, T)>,
) -> Result<(Transaction, T, Vec<Funding>, u64)> {
    funded_prefixes_with_floor(config, coins, rate, make, |_| Ok(0))
}

pub(super) fn funded_prefixes_with_floor<T>(
    config: &Config,
    coins: &[Funding],
    rate: u64,
    make: &mut impl FnMut(&[Funding], u64) -> Result<(Transaction, T)>,
    fee_floor: impl Fn(&Transaction) -> Result<u64>,
) -> Result<(Transaction, T, Vec<Funding>, u64)> {
    let mut last = None;
    for count in 0..=coins.len() {
        let selected = &coins[..count];
        match price_candidate(config, rate, selected, &mut *make, &fee_floor) {
            Ok((tx, result, fee)) => return Ok((tx, result, selected.to_vec(), fee)),
            Err(error) => last = Some(error.to_string()),
        }
    }
    Err(format!(
        "No mature, unreserved funding selection can cover this operation and its full mass fee. {}",
        last.unwrap_or_default()
    )
    .into())
}

fn price_candidate<T>(
    config: &Config,
    rate: u64,
    selected: &[Funding],
    make: &mut impl FnMut(&[Funding], u64) -> Result<(Transaction, T)>,
    fee_floor: impl Fn(&Transaction) -> Result<u64>,
) -> Result<(Transaction, T, u64)> {
    let mut fee = 1;
    for _ in 0..32 {
        let (tx, result) = make(selected, fee)?;
        let required = super::client::relay_fee_at_rate(&tx, config, rate)?.max(fee_floor(&tx)?);
        if required > config.fee_sompi {
            return Err("replacement fee exceeds the configured fee cap".into());
        }
        if fee >= required {
            return Ok((tx, result, fee));
        }
        fee = required;
    }
    Err("transaction fee did not stabilize".into())
}

/// Start and collection have fixed covenant outputs for a quote. Rank ordinary
/// funding and change without rebuilding the covenant witnesses.
pub(crate) fn funded_by_mass<T>(
    config: &Config,
    coins: &[Funding],
    sources: &[(TransactionOutpoint, UtxoEntry)],
    topup: u64,
    mut make: impl FnMut(&[Funding], u64) -> Result<(Transaction, T)>,
) -> Result<(Transaction, T, Vec<Funding>, u64)> {
    const SIGNED_ALTERNATIVES: usize = 3;
    let rate = super::fee_policy::selected_rate(config);
    let mut ordered = coins.to_vec();
    ordered.sort_by(|a, b| b.utxo.amount.cmp(&a.utxo.amount));
    let mut best = match funded_prefixes(config, &ordered, rate, &mut make) {
        Ok(quote) => quote,
        Err(error) => {
            // Near-exact funding can fail the ordinary fee iteration because
            // its first trial leaves an over-limit change output.
            for coin in ordered.iter().rev().filter(|c| {
                c.utxo.amount.checked_sub(topup).is_some_and(|budget| budget > 0 && budget <= config.fee_sompi)
            }).take(SIGNED_ALTERNATIVES) {
                let fee = coin.utxo.amount - topup;
                let selected = std::slice::from_ref(coin);
                if let Ok((tx, data)) = make(selected, fee)
                    && super::client::relay_fee_at_rate(&tx, config, rate).is_ok_and(|required| required <= fee)
                {
                    return Ok((tx, data, selected.to_vec(), fee));
                }
            }
            return Err(error);
        }
    };
    let (baseline, _, chosen, fee) = &best;
    if ordered.len() <= 1 {
        return Ok(best);
    }
    let mut non_contextual = baseline.clone();
    non_contextual.set_storage_mass(0);
    let floor = super::client::relay_fee_at_rate(&non_contextual, config, rate)?;
    let source_cells = baseline.inputs.iter().filter_map(|input| {
        sources.iter().find(|(out, _)| *out == input.previous_outpoint).map(|(_, utxo)| UtxoCell::from(utxo))
    }).collect::<Vec<_>>();
    if chosen.is_empty() || source_cells.len() + chosen.len() != baseline.inputs.len() {
        return Ok(best);
    }
    let fixed_outputs = baseline.outputs.iter().filter(|o| o.covenant.is_some()).map(UtxoCell::from).collect::<Vec<_>>();
    // Both builders append standard wallet inputs after the actors. Their signatures
    // and the wallet change script have fixed size, so keep those shapes.
    non_contextual.inputs.truncate(source_cells.len() + 1);
    let projected = non_contextual.clone();
    let project_change = projected.outputs.iter().any(|output| output.covenant.is_none()) && (*fee > floor || chosen.len() > 1);
    let change_floor = super::client::relay_fee_at_rate(&projected, config, rate)?;
    non_contextual.outputs.retain(|output| output.covenant.is_some());
    let exact_floor = super::client::relay_fee_at_rate(&non_contextual, config, rate)?;
    let baseline_cost = (*fee, baseline.storage_mass(), chosen.len());
    let mut alternatives = Vec::new();
    for coin in &ordered {
        if chosen.len() == 1 && chosen[0].outpoint == coin.outpoint { continue; }
        let Some(budget) = coin.utxo.amount.checked_sub(topup) else { continue; };
        let input = UtxoCell::from(&coin.utxo);
        let inputs = source_cells.iter().copied().chain(std::iter::once(input)).collect::<Vec<_>>();
        let storage = |change| {
            let outputs = fixed_outputs.iter().copied().chain((change > 0).then(|| UtxoCell::new(input.plurality, change)));
            calc_storage_mass(false, inputs.iter().copied(), outputs, MAINNET_PARAMS.storage_mass_parameter)
        };
        if budget >= exact_floor && budget <= *fee
            && (*fee > floor || chosen.len() > 1 || budget < *fee)
            && let Some(mass) = storage(0)
        {
            non_contextual.set_storage_mass(mass);
            if (budget, mass, 1) < baseline_cost
                && super::client::relay_fee_at_rate(&non_contextual, config, rate).is_ok_and(|required| required <= budget)
            {
                alternatives.push((budget, mass, coin));
            }
        }
        if !project_change { continue; }
        let mut trial_fee = change_floor;
        for _ in 0..8 {
            let Some(change) = budget.checked_sub(trial_fee).filter(|&amount| amount > 0) else { break; };
            let Some(mass) = storage(change) else { break; };
            if (trial_fee, mass, 1) >= baseline_cost { break; }
            projected.set_storage_mass(mass);
            let Ok(required) = super::client::relay_fee_at_rate(&projected, config, rate) else { break; };
            if trial_fee >= required {
                alternatives.push((trial_fee, mass, coin));
                break;
            }
            trial_fee = required;
        }
    }
    alternatives.sort_by_key(|(fee, mass, _)| (*fee, *mass));
    for (fee, _, coin) in alternatives.into_iter().take(SIGNED_ALTERNATIVES) {
        let selected = std::slice::from_ref(coin);
        if let Ok((tx, data)) = make(selected, fee)
            && super::client::relay_fee_at_rate(&tx, config, rate).is_ok_and(|required| required <= fee)
            && (fee, tx.storage_mass(), 1) < (best.3, best.0.storage_mass(), best.2.len())
        {
            best = (tx, data, selected.to_vec(), fee);
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn high_wager_start_prefers_a_single_exact_coin_to_dust_change() {
        use secp256k1::{Secp256k1, SecretKey};
        let key = |n| Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap());
        let coin = |n, owner: &Keypair, amount| Funding {
            outpoint: TransactionOutpoint::new(Hash::from_bytes([n; 32]), 0),
            utxo: UtxoEntry::new(amount, p2pk(&pubkey(owner)), 1, false, None),
        };
        let a = reusable::artifact().unwrap();
        let b = TxBuilder::new(&a).unwrap();
        let guest = key(71);
        let host = key(72);
        let wager = 25_400_000_000u64;
        let reserve = 300_000_000;
        let capital = wager + reserve;
        let create = |owner: &Keypair, n, balance| {
            let (tx, states) = reusable::create(&b, owner, balance, reserve, &[coin(n, owner, balance + 500_000_000)], 100_000).unwrap();
            reusable::accepted_set(&tx, [0, 1, 2], states, 100)
        };
        let left = create(&guest, 73, capital);
        let right = create(&host, 74, 800_000_000);
        let offer = reusable::Offer {
            lobby: Lobby {
                left: pubkey(&guest), right: pubkey(&host), refund: pubkey(&guest), control_value: 0,
                ball_tick_daa: 6, stake_per_player: i64::try_from(wager).unwrap(), ball_count: 3, win_score: 6,
            },
            pools: [left.id().unwrap(), right.id().unwrap()],
            generations: [left.generation(), right.generation()],
        };
        let reserve_tx = reusable::reserve(&b, &left, &offer, &guest, capital, &[coin(75, &guest, 500_000_000)], 100_000).unwrap();
        let (lobby, left) = reusable::reserved(&reserve_tx, &left, offer, capital, 110);
        let make = |coins: &[Funding], fee| {
            let (tx, _, _) = reusable::start(&b, &lobby, &left, &right, &host, capital, coins, fee)?;
            Ok((tx, ()))
        };
        let topup = capital - right.balance();
        let config = Config::default();
        let rate = super::super::fee_policy::selected_rate(&config);
        let mut exact_fee = 1;
        for _ in 0..3 {
            let (tx, ()) = make(&[coin(76, &host, topup + exact_fee)], exact_fee).unwrap();
            exact_fee = super::super::client::relay_fee_at_rate(&tx, &config, rate).unwrap();
        }
        let exact = coin(76, &host, topup + exact_fee);
        let near_dust = coin(77, &host, topup + exact_fee + 20_000);
        let sources = std::iter::once((lobby.outpoint, lobby.utxo.clone()))
            .chain(right.lanes.iter().map(|lane| (lane.outpoint, lane.utxo.clone())))
            .collect::<Vec<_>>();
        let coins = [near_dust, exact.clone()];
        let (baseline, (), _, baseline_fee) = funded_transaction(&config, &coins, make).unwrap();
        let (tx, (), selected, fee) = funded_by_mass(&config, &coins, &sources, topup, make).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].outpoint, exact.outpoint);
        assert!(tx.outputs.iter().all(|o| o.covenant.is_some()));
        assert!(fee < baseline_fee);
        assert!(tx.storage_mass() < baseline.storage_mass());
    }

    #[test]
    fn uncertain_creation_remains_reserved_and_disables_first_set_auto_approval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queues.json");
        let key = Keypair::from_secret_key(&secp256k1::Secp256k1::new(), &secp256k1::SecretKey::from_slice(&[19; 32]).unwrap());
        let a = reusable::artifact().unwrap();
        let b = TxBuilder::new(&a).unwrap();
        let funding = Funding {
            outpoint: TransactionOutpoint::new(Hash::from_bytes([42; 32]), 0),
            utxo: UtxoEntry::new(10_000_000_000, p2pk(&pubkey(&key)), 1, false, None),
        };
        let (tx, states) = reusable::create(&b, &key, 800_000_000, reusable::MIN_RESERVE, &[funding.clone()], 100_000).unwrap();
        let set = reusable::accepted_set(&tx, [0, 1, 2], states, u64::MAX);
        let mut i = Inventory::load(&path, "testnet-10", &pubkey(&key)).unwrap();
        assert!(i.first_creation());
        i.authorize(
            &path,
            Authorization {
                transaction: tx,
                inputs: vec![funding.utxo],
                purpose: "create queue set".into(),
                successors: vec![set.clone()],
                creates_set: true,
            },
        )
        .unwrap();
        let mut i = Inventory::load(&path, "testnet-10", &pubkey(&key)).unwrap();
        assert!(!i.first_creation());
        assert_eq!(i.available().count(), 0);
        i.reconcile_coins(&b, &[]).unwrap();
        assert_eq!(i.pending.len(), 1);
        let coins = set
            .lanes
            .iter()
            .map(|c| {
                let mut utxo = c.utxo.clone();
                utxo.block_daa_score = 100;
                Funding { outpoint: c.outpoint, utxo }
            })
            .collect::<Vec<_>>();
        i.reconcile_coins(&b, &coins).unwrap();
        assert!(i.pending.is_empty());
        assert_eq!(i.available().count(), 1);
        i.reconcile_coins(&b, &[]).unwrap();
        assert_eq!(i.available().count(), 0, "a reorg cannot make missing outputs spendable");
    }
}
