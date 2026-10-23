//! Accepted network fees, attributed to the ordinary fee input's owner.
//! Joint launch has one guest input covering its stake and the external fee;
//! fees are input minus output value, never the guest's stake contribution.
use super::super::engine::Result;
use super::audit::{TransactionRecord, TransactionStatus};
use kaspa_consensus_core::{
    Hash,
    tx::{ScriptPublicKey, Transaction, UtxoEntry},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Payment {
    pub sompi: u64,
    pub payer: ScriptPublicKey,
}
impl Payment {
    pub fn from_transaction(tx: &Transaction, inputs: &[UtxoEntry]) -> Result<Option<Self>> {
        let ordinary: Vec<_> = inputs.iter().filter(|u| u.covenant_id.is_none()).collect();
        if ordinary.len() != 1 {
            return Ok(None);
        }
        Ok(Some(Self { sompi: super::client::transaction_fee(tx, inputs)?, payer: ordinary[0].script_public_key.clone() }))
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub version: u32,
    pub payments: BTreeMap<Hash, Payment>,
    /// Older caches lack historical payment data. Never label a partial sum total.
    pub complete: bool,
}
impl Ledger {
    pub fn new() -> Self {
        Self { version: 1, complete: true, ..Default::default() }
    }
    pub fn apply(&mut self, record: &TransactionRecord) {
        match record.status {
            TransactionStatus::Reverted => {
                self.payments.remove(&record.event.txid);
            }
            TransactionStatus::Accepted => {
                if let Some(payment) = &record.event.payment {
                    self.payments.insert(record.event.txid, payment.clone());
                } else {
                    self.complete = false;
                }
            }
        }
    }
    pub fn paid_by(&self, payer: &ScriptPublicKey) -> u128 {
        self.payments.values().filter(|p| &p.payer == payer).map(|p| p.sompi as u128).sum()
    }
    pub fn paid_by_wallet(&self, player: &ScriptPublicKey, payer: &ScriptPublicKey) -> u128 {
        self.payments.values().filter(|p| &p.payer == player || &p.payer == payer).map(|p| p.sompi as u128).sum()
    }
}

pub fn coins(sompi: u128) -> String {
    format!("{}.{:08}", sompi / 100_000_000, sompi % 100_000_000)
}
