// Explicit, bounded testnet-10 acceptance-path experiment.
use clap::Parser;
use kaspa_consensus_core::{
    Hash,
    constants::TX_VERSION_TOCCATA,
    tx::{MutableTransaction, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput},
};
use kaspa_pong::{
    engine::*,
    node::{
        self, Config,
        rpc::{Node, VirtualChainFeed},
    },
};
use kaspa_rpc_core::RpcTransaction;
use kaspa_txscript::script_builder::ScriptBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};
const NETWORK: &str = "testnet-10";
const MAX_FEES: u64 = 500_000_000;
const COMMAND_COMPUTE: u64 = 7_000;
const COMMAND_TRANSIENT: u64 = 16_000;
const COMMAND_STORAGE: u64 = 50_000;
const SMALL_OUTPUT: u64 = 25_000_000; // Preserve the storage floor as the funded test coin shrinks.

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Mass {
    bytes: u64,
    compute: u64,
    transient: u64,
    storage: u64,
    relay: u64,
}
fn mass(tx: &Transaction) -> Mass {
    use kaspa_consensus_core::{
        config::params::MAINNET_PARAMS,
        mass::{MassCalculator, transaction_estimated_serialized_size},
    };
    let m = MassCalculator::new_with_consensus_params(&MAINNET_PARAMS).calc_non_contextual_masses(tx);
    Mass {
        bytes: transaction_estimated_serialized_size(tx),
        compute: m.compute_mass,
        transient: m.transient_mass,
        storage: tx.storage_mass(),
        relay: m.normalized_max(&MAINNET_PARAMS.mempool_block_mass_cofactors().after()),
    }
}

fn charge(total: u64, fee: u64, maximum: u64) -> Result<u64> {
    total
        .checked_add(fee)
        .filter(|next| *next <= maximum && *next <= MAX_FEES)
        .ok_or_else(|| "fee budget exhausted; halting BEFORE another submission".into())
}

fn self_transfer(config: &Config, key: &secp256k1::Keypair, coins: &[Funding]) -> Result<Transaction> {
    let script = p2pk(&pubkey(key));
    if coins.is_empty() || coins.iter().any(|coin| coin.utxo.script_public_key != script || coin.utxo.covenant_id.is_some()) {
        return Err("benchmark source is not an ordinary coin owned by this wallet".into());
    }
    let entries: Vec<_> = coins.iter().map(|coin| coin.utxo.clone()).collect();
    let total = coins.iter().try_fold(0u64, |sum, coin| sum.checked_add(coin.utxo.amount).ok_or("amount overflow"))?;
    let tx = node::client::priced_transaction(config, |fee| {
        let amount = total
            .checked_sub(fee)
            .and_then(|a| a.checked_sub(2 * SMALL_OUTPUT))
            .filter(|a| *a >= 100_000_000)
            .ok_or("benchmark main coin would fall below 1 KAS")?;
        let inputs = coins
            .iter()
            .map(|coin| {
                let mut input = TransactionInput::new(coin.outpoint, vec![0; 66], 0, 0);
                input.compute_commit = kaspa_consensus_core::mass::ComputeBudget(10).into();
                input
            })
            .collect();
        let mut tx = Transaction::new(
            TX_VERSION_TOCCATA,
            inputs,
            [amount, SMALL_OUTPUT, SMALL_OUTPUT].into_iter().map(|v| TransactionOutput::new(v, script.clone())).collect(),
            0,
            Default::default(),
            0,
            vec![],
        );
        // Payload raises compute/transient mass; three unequal self outputs
        // retain positive storage mass across repeated three-input transfers.
        let initial = mass(&tx);
        let extra =
            COMMAND_COMPUTE.saturating_sub(initial.compute).max(COMMAND_TRANSIENT.saturating_sub(initial.transient).div_ceil(4));
        tx.payload = (0..extra.div_ceil(32))
            .flat_map(|i| blake2b_simd::Params::new().hash_length(32).hash(&i.to_le_bytes()).as_bytes().to_vec())
            .take(extra as usize)
            .collect();
        tx.finalize(); // Payload participates in the cached transaction ID.
        let mut mutable = MutableTransaction::new(tx.clone());
        mutable.entries = entries.iter().cloned().map(Some).collect();
        for i in 0..tx.inputs.len() {
            tx.inputs[i].signature_script = ScriptBuilder::new().add_data(&sign(&mutable, i, key))?.drain();
        }
        commit_storage_mass(&tx, entries.clone())?;
        tx.finalize();
        Ok(tx)
    })?;
    if tx.outputs.len() != 3 || tx.outputs.iter().any(|o| o.script_public_key != script || o.covenant.is_some()) {
        return Err("benchmark destination invariant failed".into());
    }
    let m = mass(&tx);
    if m.compute < COMMAND_COMPUTE || m.transient < COMMAND_TRANSIENT || m.storage < COMMAND_STORAGE {
        return Err(format!("benchmark mass fell below command floors: {m:?}; no transaction sent").into());
    }
    Ok(tx)
}

fn outputs(tx: &Transaction, daa: u64) -> Vec<Funding> {
    tx.outputs
        .iter()
        .enumerate()
        .map(|(i, o)| Funding {
            outpoint: TransactionOutpoint::new(tx.id(), i as u32),
            utxo: kaspa_consensus_core::tx::UtxoEntry::new(o.value, o.script_public_key.clone(), daa, false, None),
        })
        .collect()
}

/// Read reservation journals without starting recovery or queuing cancellations.
fn reserved(network: &str, key: &secp256k1::Keypair) -> Result<HashSet<TransactionOutpoint>> {
    let public = key.x_only_public_key().0;
    let index = PathBuf::from(format!("games/pending-{network}-{public}.json"));
    let paths: Vec<PathBuf> = if index.exists() { node::load(&index)? } else { vec![] };
    let mut blocked = HashSet::new();
    for path in paths {
        if path.exists() {
            let value: Value = node::load(&path)?;
            if let Some(pending) = value["pending"].as_array() {
                for p in pending {
                    let tx: Transaction = serde_json::from_value(p["transaction"].clone())?;
                    blocked.extend(tx.inputs.into_iter().map(|i| i.previous_outpoint));
                }
            }
        }
        let invitation = path.with_file_name("invitation.json");
        if invitation.exists() {
            let value: Value = node::load(&invitation)?;
            let tx: Transaction = serde_json::from_value(value["genesis"].clone())?;
            blocked.extend(tx.inputs.into_iter().map(|i| i.previous_outpoint));
        }
    }
    let hosted = PathBuf::from(format!("games/hosted-{network}-{public}.json"));
    if hosted.exists() {
        let value: Value = node::load(&hosted)?;
        for record in value["records"].as_array().ok_or("invalid hosted reservation registry")? {
            let path: PathBuf = serde_json::from_value(record["invitation"].clone())?;
            let value: Value = node::load(&path)?;
            let tx: Transaction = serde_json::from_value(value["genesis"].clone())?;
            blocked.extend(tx.inputs.into_iter().map(|i| i.previous_outpoint));
        }
    }
    Ok(blocked)
}

#[derive(Parser)]
struct Options {
    #[arg(long, default_value = "wss://vector-10.kaspa.green/kaspa/testnet-10/wrpc/borsh")]
    endpoint: String,
    #[arg(long, default_value_t=30, value_parser=clap::value_parser!(u16).range(1..=100))]
    blocks: u16,
    #[arg(long, default_value_t=8, value_parser=clap::value_parser!(u16).range(2..=8))]
    nodes: u16,
    #[arg(long, default_value_t = 500_000_000)]
    budget_sompi: u64,
    #[arg(long, default_value_t = 1_000_000)]
    fee_cap_sompi: u64,
    #[arg(long, default_value = "target/testnet-right-acceptance-20261027")]
    out: PathBuf,
    /// Discover/qualify nodes without loading a wallet.
    #[arg(long)]
    probe: bool,
    /// Without this flag, only print a cost quote.
    #[arg(long)]
    execute: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Receipt {
    endpoint: String,
    primary: bool,
    arrival_us: u64,
    validated_us: u64,
    daa: u64,
    block: Hash,
    chain_adjustments: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Sample {
    block: usize,
    mode: String,
    txid: Hash,
    submitted_unix_ms: u64,
    fee_sompi: u64,
    mass: Mass,
    nodes: usize,
    acknowledgement_us: Option<u64>,
    acknowledgement_error: Option<String>,
    primary_arrival_us: u64,
    primary_validated_us: u64,
    earliest_arrival_us: u64,
    earliest_validated_us: u64,
    effective_validated_us: u64,
    metadata_conflict: bool,
    accepting_block_variation: bool,
    receipts: Vec<Receipt>,
    observer_errors: Vec<String>,
}
fn order() -> Result<Vec<&'static str>> {
    let mut modes = vec!["single", "broadcast_single", "broadcast_multi"];
    for i in (1..modes.len()).rev() {
        let mut bytes = [0; 8];
        getrandom::getrandom(&mut bytes).map_err(|_| "OS randomness unavailable")?;
        modes.swap(i, (u64::from_le_bytes(bytes) % (i as u64 + 1)) as usize);
    }
    Ok(modes)
}
fn median(mut values: Vec<u64>) -> f64 {
    values.sort_unstable();
    let n = values.len();
    if n % 2 == 0 { (values[n / 2 - 1] as f64 + values[n / 2] as f64) / 2.0 } else { values[n / 2] as f64 }
}
fn summarize(samples: &[Sample], charged: u64) -> Value {
    let groups: Vec<_> = ["single", "broadcast_single", "broadcast_multi"]
        .into_iter()
        .map(|mode| {
            let rows: Vec<_> = samples.iter().filter(|s| s.mode == mode).collect();
            let values: Vec<_> = rows.iter().map(|s| s.effective_validated_us).collect();
            let mut sorted = values.clone();
            sorted.sort_unstable();
            json!({"mode":mode,"n":values.len(),"median_ms":if values.is_empty(){None}else{Some(median(values.clone())/1000.0)},
            "mean_ms":if values.is_empty(){None}else{Some(values.iter().map(|v|*v as f64).sum::<f64>()/values.len() as f64/1000.0)},
            "p95_ms":sorted.get((sorted.len()*95).div_ceil(100).saturating_sub(1)).map(|v|*v as f64/1000.0),
            "maximum_ms":sorted.last().map(|v|*v as f64/1000.0),
            "metadata_conflicts":rows.iter().filter(|r|r.metadata_conflict).count()})
        })
        .collect();
    json!({"network":NETWORK,"groups":groups,"fees_sompi":charged,"fees_tkas":charged as f64/100_000_000.0,
        "measurement":"command-mass P2PK self-transfers: Full-chain observation, signature validation and current output checks; not game replay/render latency"})
}

async fn observe(
    node: Node,
    primary: bool,
    feed: Option<std::sync::Arc<VirtualChainFeed>>,
    mut cursor: Hash,
    expected: std::sync::Arc<Transaction>,
    known: std::sync::Arc<Vec<kaspa_consensus_core::tx::UtxoEntry>>,
    started: Instant,
) -> Result<Receipt> {
    let mut adjustments = 0;
    let mut pending: Option<(u64, u64, Hash)> = None;
    loop {
        let delta = node.delta(cursor).await?;
        adjustments += delta.removed.len();
        if pending.is_some_and(|(_, _, block)| delta.removed.contains(&block)) {
            pending = None;
        }
        cursor = delta.added.last().map(|b| b.hash).or(delta.common).unwrap_or(cursor);
        for block in delta.added {
            for accepted in block.transactions.iter().filter(|a| a.tx.id() == expected.id()) {
                let arrival_us = started.elapsed().as_micros() as u64;
                if kaspa_consensus_core::hashing::tx::hash(&accepted.tx) != kaspa_consensus_core::hashing::tx::hash(&expected) {
                    return Err("accepted signed contents differ".into());
                }
                if accepted.inputs.len() != known.len()
                    || accepted.inputs.iter().zip(known.iter()).any(|(a, b)| {
                        a.amount != b.amount
                            || a.script_public_key != b.script_public_key
                            || a.covenant_id != b.covenant_id
                            || a.is_coinbase != b.is_coinbase
                    })
                {
                    return Err("accepted input source values/scripts differ".into());
                }
                let tx = accepted.tx.clone();
                let entries = accepted.inputs.clone();
                tokio::task::spawn_blocking(move || {
                    for i in 0..tx.inputs.len() {
                        argent_runtime::execute_input_with_covenants(&tx, entries.clone(), i).map_err(|e| e.to_string())?;
                    }
                    Ok::<(), String>(())
                })
                .await??;
                pending = Some((arrival_us, block.daa, block.hash));
            }
        }
        if let Some((arrival_us, daa, block)) = pending {
            let found = node.utxos(&[expected.outputs[0].script_public_key.clone()]).await?;
            if expected.outputs.iter().enumerate().all(|(i, o)| {
                found.iter().any(|f| {
                    f.outpoint == TransactionOutpoint::new(expected.id(), i as u32)
                        && f.utxo.amount == o.value
                        && f.utxo.script_public_key == o.script_public_key
                        && f.utxo.block_daa_score == daa
                })
            }) {
                return Ok(Receipt {
                    endpoint: node.endpoint,
                    primary,
                    arrival_us,
                    validated_us: started.elapsed().as_micros() as u64,
                    daa,
                    block,
                    chain_adjustments: adjustments,
                });
            }
        }
        if let Some(feed) = &feed {
            let _ = tokio::time::timeout(Duration::from_millis(100), feed.changed()).await;
        } else {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn trial(
    node: &Node,
    observers: &[(String, Node)],
    feed: Option<std::sync::Arc<VirtualChainFeed>>,
    mode: &str,
    tx: &Transaction,
    entries: Vec<kaspa_consensus_core::tx::UtxoEntry>,
    block: usize,
    fee: u64,
) -> Result<Sample> {
    let mut readers = vec![node.clone()];
    if mode == "broadcast_multi" {
        readers.extend(observers.iter().map(|(_, n)| n.clone()));
    }
    let mut cursors = tokio::task::JoinSet::new();
    for (i, peer) in readers.iter().cloned().enumerate() {
        cursors.spawn(async move { (i, peer.accepted_cursor().await.map_err(|e| e.to_string())) });
    }
    let mut anchors = vec![Hash::from_bytes([0; 32]); readers.len()];
    while let Some(result) = cursors.join_next().await {
        let (i, cursor) = result?;
        anchors[i] = cursor?;
    }
    let started = Instant::now();
    let submitted_unix_ms = node::client::now_ms();
    let tx = std::sync::Arc::new(tx.clone());
    let entries = std::sync::Arc::new(entries);
    let mut watches = tokio::task::JoinSet::new();
    for (i, peer) in readers.into_iter().enumerate() {
        let tx = tx.clone();
        let entries = entries.clone();
        let feed = if i == 0 { feed.clone() } else { None };
        let cursor = anchors[i];
        watches.spawn(async move {
            let endpoint = peer.endpoint.clone();
            let result =
                tokio::time::timeout(Duration::from_secs(15), observe(peer, i == 0, feed, cursor, tx, entries, started)).await;
            (
                i,
                endpoint,
                match result {
                    Ok(result) => result.map_err(|e| e.to_string()),
                    Err(_) => Err("acceptance observer timed out after 15 seconds".into()),
                },
            )
        });
    }
    let submit = async {
        let result = if mode == "single" {
            match tokio::time::timeout(Duration::from_secs(5), node.rpc.submit_transaction(RpcTransaction::from(tx.as_ref()), false))
                .await
            {
                Ok(Ok(id)) if id == tx.id() => Ok(id),
                Ok(Ok(_)) => Err("different submitted txid".to_string()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("submission timed out".into()),
            }
        } else {
            node.submit(&tx).await.map_err(|e| e.to_string())
        };
        (started.elapsed().as_micros() as u64, result)
    };
    let collect = async {
        let mut receipts = vec![];
        let mut errors = vec![];
        while let Some(result) = watches.join_next().await {
            let (i, endpoint, result) = result.map_err(|e| e.to_string())?;
            match result {
                Ok(receipt) => receipts.push(receipt),
                Err(error) => errors.push(format!("observer {i} {endpoint}: {error}")),
            }
        }
        Ok::<_, String>((receipts, errors))
    };
    let ((ack_us, ack), reports) = tokio::join!(submit, collect);
    let (receipts, observer_errors) = reports?;
    let primary = receipts
        .iter()
        .find(|r| r.primary)
        .ok_or_else(|| format!("primary acceptance not validated; HALTED with authorization retained: {observer_errors:?}"))?;
    let metadata_conflict = receipts.iter().any(|r| r.daa != primary.daa);
    let accepting_block_variation = receipts.iter().any(|r| r.block != primary.block);
    let earliest_arrival_us = receipts.iter().map(|r| r.arrival_us).min().unwrap();
    let earliest_validated_us = receipts.iter().map(|r| r.validated_us).min().unwrap();
    let effective_validated_us =
        if mode == "broadcast_multi" && !metadata_conflict { earliest_validated_us } else { primary.validated_us };
    Ok(Sample {
        block,
        mode: mode.into(),
        txid: tx.id(),
        submitted_unix_ms,
        fee_sompi: fee,
        mass: mass(&tx),
        nodes: if mode == "single" { 1 } else { observers.len() + 1 },
        acknowledgement_us: ack.as_ref().ok().map(|_| ack_us),
        acknowledgement_error: ack.err(),
        primary_arrival_us: primary.arrival_us,
        primary_validated_us: primary.validated_us,
        earliest_arrival_us,
        earliest_validated_us,
        effective_validated_us,
        metadata_conflict,
        accepting_block_variation,
        receipts,
        observer_errors,
    })
}

async fn run(o: &Options, node: &Node) -> Result<()> {
    if node.network.to_string() != NETWORK {
        return Err("testnet-10 ONLY; refusing a different network".into());
    }
    let pool = node.broadcast.as_ref().ok_or("broadcast pool disabled")?;
    let until = Instant::now() + Duration::from_secs(30);
    while Instant::now() < until && pool.status()["connected_nodes"].as_u64().unwrap_or(0) < o.nodes as u64 {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let observers = pool.observers();
    if observers.is_empty() {
        return Err("no secondary submission node; no transactions sent".into());
    }
    for (_, peer) in &observers {
        peer.qualify_acceptance().await?;
    }
    let identities: HashSet<_> = observers.iter().map(|(id, _)| id.clone()).collect();
    let capability = json!({"network":NETWORK,"primary":node.endpoint,"target_nodes":o.nodes,"actual_nodes":observers.len()+1,
        "secondary_endpoints":observers.iter().map(|(_,n)|&n.endpoint).collect::<Vec<_>>(),"transactions_submitted":0});
    if o.probe {
        println!("{capability}");
        return Ok(());
    }
    let wallet_path = kaspa_pong::menu::named_wallet(std::path::Path::new("."), "right")?;
    let wallet = node::wallet::Wallet::load(&wallet_path)?;
    let _locks = node::hosted::signer_locks(NETWORK, &wallet.player, &wallet.player)?;
    let blocked: Vec<_> = reserved(NETWORK, &wallet.player)?.into_iter().collect();
    let estimate = tokio::time::timeout(Duration::from_secs(5), node.rpc.get_fee_estimate()).await??;
    let rate = estimate.priority_bucket.feerate * 1000.0;
    if !rate.is_finite() || rate <= 0.0 || rate >= u64::MAX as f64 {
        return Err("invalid priority fee estimate".into());
    }
    let config = Config {
        network: NETWORK.into(),
        fee_sompi: o.fee_cap_sompi,
        relay_fee_per_kg: (rate.ceil() as u64).max(1000),
        ..Default::default()
    };
    let mut candidates = node.funding_candidates(&pubkey(&wallet.player), 400_000_000, config.coinbase_maturity_daa, &blocked).await?;
    let source = candidates.pop().ok_or("right has no unreserved mature coin of at least 4 TKAS; no transaction sent")?;
    let mut coins = vec![source];
    let bootstrap = self_transfer(&config, &wallet.player, &coins)?;
    let bootstrap_fee = node::client::transaction_fee(&bootstrap, &coins.iter().map(|c| c.utxo.clone()).collect::<Vec<_>>())?;
    let projected = outputs(&bootstrap, 0);
    let quote = self_transfer(&config, &wallet.player, &projected)?;
    let fee = node::client::transaction_fee(&quote, &projected.iter().map(|c| c.utxo.clone()).collect::<Vec<_>>())?;
    let blocks = (o.blocks as u64).min(o.budget_sompi.saturating_sub(bootstrap_fee) / fee / 3) as usize;
    if blocks == 0 {
        return Err("budget cannot fund a complete three-arm block".into());
    }
    // Verify the entire planned amount/mass trajectory before exposing anything.
    let mut offline = projected;
    for _ in 0..blocks * 3 {
        let tx = self_transfer(&config, &wallet.player, &offline)?;
        offline = outputs(&tx, 0);
    }
    let plan = json!({"capabilities":capability,"wallet":"right","destination":node::wallet::public_identity(NETWORK,&wallet.player)?["address"],
        "blocks":blocks,"transactions":blocks*3+1,"bootstrap_fee_sompi":bootstrap_fee,"fee_per_trial_sompi":fee,
        "planned_total_fee_sompi":bootstrap_fee+fee*blocks as u64*3,"maximum_fee_sompi":o.budget_sompi,"mass":mass(&quote),
        "priority_rate_per_kg":config.relay_fee_per_kg,"execute":o.execute});
    println!("{plan}");
    if !o.execute {
        return Ok(());
    }
    let budget_path = PathBuf::from("target/testnet-right-acceptance-budget-20261027.json");
    if budget_path.exists() {
        return Err("a retained budget journal already exists; refusing an automatic second funded run".into());
    }
    std::fs::create_dir(&o.out)?;
    node::save(&o.out.join("plan.json"), &plan)?;
    let mut samples = vec![];
    let mut charged = 0u64;
    let feed = node.virtual_chain_feed().await.ok().map(std::sync::Arc::new);
    let mut log = std::fs::OpenOptions::new().write(true).create_new(true).open(o.out.join("samples.jsonl"))?;
    let result=async {
        let mut trials=vec![(usize::MAX,"single")];
        for block in 0..blocks {for mode in order()? {trials.push((block,mode));}}
        for (block,mode) in trials {
            if pool.observers().iter().map(|(id,_)|id.clone()).collect::<HashSet<_>>()!=identities{return Err("pool membership changed; halting before another submission".into());}
            let tx=if block==usize::MAX{bootstrap.clone()}else{self_transfer(&config,&wallet.player,&coins)?};
            let entries:Vec<_>=coins.iter().map(|c|c.utxo.clone()).collect();
            let fee=node::client::transaction_fee(&tx,&entries)?;
            let next=charge(charged,fee,o.budget_sompi)?;
            if !node.sources_available(&tx,&entries,&[]).await?{return Err("source coin changed; no new submission".into());}
            node::save(&o.out.join(format!("authorization-{}.json",tx.id())),&json!({"transaction":tx,"inputs":entries,"fee_sompi":fee,"mode":mode,"block":block}))?;
            charged=next;
            let budget=json!({"network":NETWORK,"wallet":"right","maximum_sompi":o.budget_sompi,"charged_sompi":charged,"last_authorized":tx.id(),"run":o.out});
            node::save(&budget_path,&budget)?;node::save(&o.out.join("budget.json"),&budget)?;
            let sample=trial(node,&observers,feed.clone(),mode,&tx,entries,block,fee).await?;
            if block==usize::MAX {node::save(&o.out.join("bootstrap.json"),&sample)?;}
            else {serde_json::to_writer(&mut log,&sample)?;log.write_all(b"\n")?;log.sync_data()?;println!("{}",serde_json::to_string(&sample)?);samples.push(sample);}
            coins=node.utxos(&[tx.outputs[0].script_public_key.clone()]).await?.into_iter().filter(|c|c.outpoint.transaction_id==tx.id()).collect();
            coins.sort_by_key(|c|c.outpoint.index);
            if coins.len()!=3{return Err("accepted self outputs missing; halting".into());}
        }
        Ok::<(),Box<dyn std::error::Error>>(())
    }.await;
    let mut summary = summarize(&samples, charged);
    summary["error"] = result.as_ref().err().map(|e| e.to_string()).into();
    node::save(&o.out.join("summary.json"), &summary)?;
    println!("{summary}");
    result
}

#[tokio::main]
async fn main() -> Result<()> {
    let o = Options::parse();
    if o.budget_sompi == 0 || o.budget_sompi > MAX_FEES || o.fee_cap_sompi == 0 || o.fee_cap_sompi > o.budget_sompi {
        return Err("fee budget exceeds the authorized 5 TKAS or is invalid".into());
    }
    let mut config = Config { network: NETWORK.into(), endpoint: o.endpoint.clone(), ..Default::default() };
    config.broadcast.nodes = o.nodes as usize;
    config.broadcast.log = o.out.with_extension("broadcast.jsonl");
    let node = Node::connect(&config).await?;
    let outcome = run(&o, &node).await;
    let closed = node.disconnect().await;
    outcome?;
    closed
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn random_blocks_contain_exactly_each_arm_and_medians_are_conventional() {
        for _ in 0..100 {
            let mut arms = order().unwrap();
            arms.sort();
            assert_eq!(arms, vec!["broadcast_multi", "broadcast_single", "single"]);
        }
        assert_eq!(median(vec![1, 9, 3, 5]), 4.0);
    }
    #[test]
    fn the_budget_cannot_exceed_five_tkas_or_overflow() {
        assert_eq!(charge(MAX_FEES - 1, 1, MAX_FEES).unwrap(), MAX_FEES);
        assert!(charge(MAX_FEES, 1, u64::MAX).is_err());
        assert!(charge(u64::MAX, 1, u64::MAX).is_err());
    }
    #[test]
    fn bootstrap_and_ninety_trials_preserve_mass_signatures_and_nonfee_funds() {
        let key =
            secp256k1::Keypair::from_secret_key(&secp256k1::Secp256k1::new(), &secp256k1::SecretKey::from_slice(&[1; 32]).unwrap());
        let config = Config { fee_sompi: 1_000_000, relay_fee_per_kg: 1000, ..Default::default() };
        let mut coins = vec![Funding {
            outpoint: TransactionOutpoint::new(Hash::from_bytes([2; 32]), 0),
            utxo: kaspa_consensus_core::tx::UtxoEntry::new(1_000_000_000, p2pk(&pubkey(&key)), 100, false, None),
        }];
        let mut spent = 0;
        for _ in 0..91 {
            let tx = self_transfer(&config, &key, &coins).unwrap();
            let entries: Vec<_> = coins.iter().map(|c| c.utxo.clone()).collect();
            for i in 0..tx.inputs.len() {
                argent_runtime::execute_input_with_covenants(&tx, entries.clone(), i).unwrap();
            }
            let mut finalized = tx.clone();
            finalized.finalize();
            assert_eq!(tx.id(), finalized.id());
            spent = charge(spent, node::client::transaction_fee(&tx, &entries).unwrap(), MAX_FEES).unwrap();
            coins = outputs(&tx, 101);
        }
        assert_eq!(coins.iter().map(|c| c.utxo.amount).sum::<u64>() + spent, 1_000_000_000);
        assert!(spent < 1_000_000);
    }
}
