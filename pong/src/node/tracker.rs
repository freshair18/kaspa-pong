//! Accepted-chain data and covenant witness decoding.
use crate::engine::*;
use argent_runtime::{Artifact, TxBuilder, materialize_redeem_script};
use kaspa_consensus_core::{
    Hash,
    tx::{Transaction, UtxoEntry},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[expect(clippy::large_enum_variant, reason = "Keep replay state inline until indirection has a measured benefit")]
pub enum Stage {
    AwaitingLobby,
    Lobby(Cell<Lobby>),
    Live(Snapshot),
    Collecting(Collecting),
    Reclaimed(Finish),
    Cancelled,
}
#[derive(Clone, Debug)]
pub struct AcceptedTx {
    pub tx: Transaction,
    pub inputs: Vec<UtxoEntry>,
}
#[derive(Clone, Debug)]
pub struct ChainBlock {
    pub hash: Hash,
    pub daa: u64,
    pub transactions: Vec<AcceptedTx>,
}

pub fn same_utxo(a: &UtxoEntry, b: &UtxoEntry) -> bool {
    a.amount == b.amount
        && a.script_public_key == b.script_public_key
        && a.block_daa_score == b.block_daa_score
        && a.is_coinbase == b.is_coinbase
        && a.covenant_id == b.covenant_id
}

pub(crate) fn is_entry(a: &Artifact, actor: &str, entry: &str, pushes: &[Vec<u8>]) -> Result<bool> {
    let entry = a.sil_abi.contract(actor).and_then(|c| c.entry(entry)).ok_or("entry absent from artifact")?;
    Ok(pushes.last().is_some_and(|p| p.as_slice() == entry.dispatch_tag.as_bytes()))
}
pub fn redeem(b: &TxBuilder<'_>, a: &Artifact, actor: &str, state: State) -> Result<Vec<u8>> {
    let c = a.sil_abi.contract(actor).ok_or("unknown actor")?;
    Ok(materialize_redeem_script(&a.sil_abi, c, &b.materialize_actor_state(actor, state)?)?)
}
pub(crate) fn call(b: &TxBuilder<'_>, a: &Artifact, actor: &str, state: State, script: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut values = pushes(script)?;
    if values.pop().ok_or("missing redeem script")? != redeem(b, a, actor, state)? {
        return Err("actor redeem script does not match accepted state".into());
    }
    Ok(values)
}
pub fn script_int(bytes: &[u8]) -> Result<i64> {
    if bytes.len() > 8 {
        return Err("oversized script integer".into());
    }
    let mut value = 0_u64;
    for (i, byte) in bytes.iter().enumerate() {
        value |= u64::from(*byte) << (8 * i);
    }
    if let Some(last) = bytes.last() {
        let sign = 1_u64 << (bytes.len() * 8 - 1);
        if last & 0x80 != 0 {
            value &= !sign;
            return Ok(-(i64::try_from(value)?));
        }
    }
    Ok(i64::try_from(value)?)
}
pub fn pushes(script: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut values = Vec::new();
    let mut at = 0;
    while at < script.len() {
        let op = script[at];
        at += 1;
        let len = match op {
            0..=75 => usize::from(op),
            76..=78 => {
                let n = 1_usize << (op - 76);
                let end = at.checked_add(n).ok_or("push overflow")?;
                let bytes = script.get(at..end).ok_or("truncated push length")?;
                let mut len = 0;
                for (i, b) in bytes.iter().enumerate() {
                    len |= usize::from(*b) << (8 * i);
                }
                at = end;
                len
            }
            79 => {
                values.push(vec![0x81]);
                continue;
            }
            81..=96 => {
                values.push(vec![op - 80]);
                continue;
            }
            _ => return Err("non-push opcode in actor witness".into()),
        };
        let end = at.checked_add(len).ok_or("push overflow")?;
        values.push(script.get(at..end).ok_or("truncated witness push")?.to_vec());
        at = end;
    }
    Ok(values)
}
