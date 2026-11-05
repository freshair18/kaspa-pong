//! Durable public inventory. A submitted transaction never makes a set idle;
//! only an accepted, validated successor does. No private keys are stored here.
use super::{Config, reservations, rpc::Node};
use crate::{
    engine::*,
    reusable::{self, Set},
};
use argent_runtime::TxBuilder;
use kaspa_consensus_core::{
    Hash,
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
        let a = reusable::artifact()?;
        let b = TxBuilder::new(&a)?;
        let mut ids = std::collections::HashSet::new();
        for r in &self.records {
            r.set.validate(&b)?;
            if r.set.lanes[0].state.queue.owner != owner || !ids.insert(r.set.id()?) {
                return Err("invalid or duplicate queue identity in inventory".into());
            }
            if r.phase == Phase::Available && !r.set.idle() {
                return Err("active set marked available".into());
            }
        }
        for p in &self.pending {
            if p.transaction.inputs.len() != p.inputs.len() {
                return Err("incomplete queue authorization".into());
            }
            if storage_mass(&p.transaction, p.inputs.clone())? != p.transaction.storage_mass() {
                return Err("queue authorization has an invalid storage commitment".into());
            }
            transaction_mass(&p.transaction)?;
            for i in 0..p.inputs.len() {
                argent_runtime::execute_input_with_covenants(&p.transaction, p.inputs.clone(), i)?;
            }
            for s in &p.successors {
                s.validate(&b)?;
                if s.lanes[0].state.queue.owner != owner {
                    return Err("queue successor belongs to another wallet".into());
                }
                for c in &s.lanes {
                    let output =
                        p.transaction.outputs.get(c.outpoint.index as usize).ok_or("queue successor index outside transaction")?;
                    if c.outpoint.transaction_id != p.transaction.id()
                        || c.utxo.is_coinbase
                        || c.utxo.amount != output.value
                        || c.utxo.script_public_key != output.script_public_key
                        || c.utxo.covenant_id != output.covenant.map(|binding| binding.covenant_id)
                    {
                        return Err("queue successor does not match signed authorization".into());
                    }
                }
            }
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
    pub fn first_creation(&self) -> bool {
        self.created_count == 0 && self.records.is_empty() && !self.pending.iter().any(|p| p.creates_set)
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
    PathBuf::from("games").join(format!("queues-{}-{}.json", config.network, owner.x_only_public_key().0))
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
    let rate = super::fee_policy::selected_rate(config);
    let mut last = None;
    for count in 0..=coins.len() {
        let selected = &coins[..count];
        let mut fee = 1;
        for _ in 0..32 {
            let (tx, result) = match make(selected, fee) {
                Ok(v) => v,
                Err(e) => {
                    last = Some(e.to_string());
                    break;
                }
            };
            let required = match super::client::relay_fee_at_rate(&tx, config, rate) {
                Ok(fee) => fee,
                Err(error) => {
                    last = Some(error.to_string());
                    break;
                }
            };
            if fee >= required {
                return Ok((tx, result, selected.to_vec(), fee));
            }
            fee = required;
        }
    }
    Err(format!(
        "No mature, unreserved funding selection can cover this operation and its full mass fee. {}",
        last.unwrap_or_default()
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
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
