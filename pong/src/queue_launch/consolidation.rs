//! Explicit wallet consolidation. Queue closure is opt-in and never spends a lease.
use super::*;
use argent_runtime::{EntryCall, TxContext, args};
use kaspa_consensus_core::tx::Transaction;
use secp256k1::Keypair;

pub const NOT_NEEDED: &str = "consolidation:not-needed";

pub fn eligible_sets(view: &View) -> Vec<Set> {
    let mut sets = Service::available(view).into_iter().cloned().collect::<Vec<_>>();
    // Legacy means an idle old set without old pending authorizations. Active
    // and uncertain previous sets are imported as Unavailable by the wallet.
    sets.extend(view.records.iter().filter(|r| {
        r.phase == node::queue_sets::Phase::Legacy && r.set.idle()
            && !view.pending.iter().any(|p| p.inputs.iter().any(|i| r.set.lanes.iter().any(|c| c.outpoint == *i)))
            && !view.offers.iter().any(|o| o.queue.id == r.set.id().unwrap() && o.queue.generation == r.set.generation())
    }).map(|r| r.set.clone()));
    sets
}

pub async fn ordinary_count(service: &Service) -> Result<usize> {
    let node = service.node().await?;
    let reserved = node::reservations::reserved()?;
    Ok(node.funding_candidates(&pubkey(&service.player()), 1, service.config().coinbase_maturity_daa, &reserved).await?.len())
}

/// Each independent Bank.close signs its own pool. A single owner output pays
/// every closed pool's withdrawal; transaction fees come from their balances.
pub fn close_group(b: &argent_runtime::TxBuilder<'_>, sets: &[Set], owner: &Keypair, fee: u64) -> Result<Transaction> {
    if sets.is_empty() { return Err("no idle queues selected".into()); }
    let signed_fee=i64::try_from(fee)?;
    let mut ctx = TxContext::new();
    let mut ids = std::collections::HashSet::new();
    let mut total = 0u64;
    for set in sets {
        set.validate(b)?;
        if !set.idle() || set.lanes[0].state.queue.owner != pubkey(owner) || !ids.insert(set.id()?) {
            return Err("only distinct idle queues owned by this wallet can be consolidated".into());
        }
        total = total.checked_add(set.balance()).ok_or("queue consolidation overflow")?;
        for (lane, cell) in set.lanes.iter().enumerate() {
            let key = *owner;
            let entry = if lane == 0 {
                EntryCall::new("close").args_with(move |tx, i| args![sign(tx, i, &key), signed_fee])
            } else {
                EntryCall::new("supply").args_with(move |tx, i| args![sign(tx, i, &key)])
            };
            ctx = ctx.actor_input(cell.state.actor(), cell.state.state(), entry, cell.outpoint, cell.utxo.clone(), 0);
        }
    }
    let amount = total.checked_sub(fee).filter(|v| *v > 0).ok_or("fee exceeds queue balances")?;
    build(b, &ctx.output(p2pk(&pubkey(owner)), None, amount))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaspa_consensus_core::tx::{TransactionOutpoint, UtxoEntry};
    use secp256k1::{Secp256k1, SecretKey};
    #[test]
    fn explicit_closure_consolidates_two_idle_sets_and_rejects_active_or_duplicate_sets() {
        let owner = Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[53; 32]).unwrap());
        let artifact = reusable::artifact().unwrap();
        let b = argent_runtime::TxBuilder::new(&artifact).unwrap();
        let make = |n| {
            let funding = Funding { outpoint: TransactionOutpoint::new(Hash::from_bytes([n; 32]), 0), utxo: UtxoEntry::new(2_000_000_000, p2pk(&pubkey(&owner)), 1, false, None) };
            let (tx, states) = reusable::create_with_nonce(&b, &owner, 800_000_000, 300_000_000, &[funding], 100_000, Hash::from_bytes([n; 32])).unwrap();
            reusable::accepted_set(&tx, [0, 1, 2], states, 100)
        };
        let first = make(54); let second = make(55);
        let tx = close_group(&b, &[first.clone(), second.clone()], &owner, 100_000).unwrap();
        assert_eq!(tx.outputs.len(), 1); assert_eq!(tx.outputs[0].value, 1_599_900_000);
        assert_eq!(tx.inputs.len(), 6);
        let sources = first.lanes.iter().chain(&second.lanes).map(|c| c.utxo.clone()).collect::<Vec<_>>();
        for input in 0..tx.inputs.len() { argent_runtime::execute_input_with_covenants(&tx, sources.clone(), input).unwrap(); }
        assert!(close_group(&b, &[first.clone(), first], &owner, 100_000).is_err());
        let mut active = second; active.lanes[0].state.active = true;
        assert!(close_group(&b, &[active], &owner, 100_000).is_err());
    }
}
