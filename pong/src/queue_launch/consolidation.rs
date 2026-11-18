//! Explicit wallet consolidation. Queue closure is opt-in and never spends a lease.
use super::*;
use argent_runtime::{EntryCall, TxContext, args};
use kaspa_consensus_core::tx::{Transaction, TransactionOutpoint};
use secp256k1::Keypair;

pub async fn ordinary_coins(service: &Service) -> Result<Vec<Funding>> {
    let node = service.node().await?;
    let reserved = node::reservations::reserved()?;
    node.funding_candidates(&pubkey(&service.player()), 1, service.config().coinbase_maturity_daa, &reserved).await
}

pub fn closure_output(tx: &Transaction) -> Funding {
    let output = &tx.outputs[0];
    Funding {
        outpoint: TransactionOutpoint::new(tx.id(), 0),
        utxo: UtxoEntry::new(output.value, output.script_public_key.clone(), u64::MAX, false, None),
    }
}

pub async fn quote_merge(service: &Service, coins: Vec<Funding>) -> Result<Quote<()>> {
    let config = service.config().clone();
    let owner = service.player();
    let claim = node::reservations::Claim::inputs(coins.iter().map(|coin| coin.outpoint).collect())?;
    let (transaction, inputs, fee) = tokio::task::spawn_blocking(move || {
        price_merge(&config, &owner, &coins).map_err(|error| error.to_string())
    }).await?.map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    Ok(Quote { transaction, inputs, data: (), fee, claim })
}

fn price_merge(config: &Config, owner: &Keypair, coins: &[Funding]) -> Result<(Transaction, Vec<UtxoEntry>, u64)> {
    let artifact = reusable::artifact()?;
    let builder = argent_runtime::TxBuilder::new(&artifact)?;
    let (transaction, (), chosen, fee) = node::queue_sets::funded_transaction(config, coins, |selected, fee| {
        if selected.len() != coins.len() {
            return Err("manual consolidation requires all reviewed inputs".into());
        }
        let ctx = reusable::fund(TxContext::new(), selected, owner, fee)?;
        Ok((build(&builder, &ctx)?, ()))
    })?;
    Ok((transaction, chosen.into_iter().map(|coin| coin.utxo).collect(), fee))
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
        let closed = closure_output(&tx);
        let wallet = Funding {
            outpoint: TransactionOutpoint::new(Hash::from_bytes([56; 32]), 0),
            utxo: UtxoEntry::new(2_000_000_000, p2pk(&pubkey(&owner)), 100, false, None),
        };
        let (merge, entries, fee) = price_merge(&Config::default(), &owner, &[closed.clone(), wallet.clone()]).unwrap();
        assert_eq!(merge.outputs.len(), 1);
        assert_eq!(merge.outputs[0].value + fee, closed.utxo.amount + wallet.utxo.amount);
        assert_eq!(merge.inputs[0].previous_outpoint, wallet.outpoint);
        assert_eq!(merge.inputs[1].previous_outpoint, closed.outpoint);
        let mut accepted = entries;
        accepted[1].block_daa_score = 200;
        for input in 0..merge.inputs.len() {
            argent_runtime::execute_input_with_covenants(&merge, accepted.clone(), input).unwrap();
        }
        assert!(close_group(&b, &[first.clone(), first], &owner, 100_000).is_err());
        let mut active = second; active.lanes[0].state.active = true;
        assert!(close_group(&b, &[active], &owner, 100_000).is_err());
    }
}
