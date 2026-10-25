//! Local single-key wallets. No RPC, key registration or mining is involved.
use super::{Config, client::priced_transaction, rpc::Node};
use super::super::engine::{Result, Funding, p2pk, pubkey, sign};
use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::{constants::TX_VERSION_TOCCATA, network::{NetworkId, NetworkType}, tx::{MutableTransaction, Transaction, TransactionOutput}};
use kaspa_txscript::{pay_to_address_script, script_builder::ScriptBuilder};
use secp256k1::{Keypair, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use std::{io::Write, path::Path, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, time::Duration};

/// A standard P2PK input executes one Schnorr signature (100,000 script units).
/// Ten compute-budget units, plus the per-input allowance, cover that execution.
const P2PK_COMPUTE_BUDGET: u16 = 10;

/// Keeps ordinary fee coins warm while gameplay is idle. A cached candidate is
/// still atomically claimed immediately before construction; if it has become
/// unavailable, action building falls back to a fresh node lookup.
pub struct FeeCoinPrewarm {
    candidates: Arc<Mutex<Vec<Funding>>>,
    stopping: Arc<AtomicBool>,
    worker: tokio::task::JoinHandle<()>,
}
impl FeeCoinPrewarm {
    pub fn start(node: Node, player: Keypair, payer: Keypair, config: Config) -> Self {
        let candidates = Arc::new(Mutex::new(Vec::new()));
        let shared = candidates.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let worker = tokio::spawn(async move {
            let minimum = if config.game_fee_cap { config.fee_sompi.min(10_000_000) } else { config.fee_sompi }.saturating_add(1);
            while !stop.load(Ordering::Relaxed) {
                if let Ok(fresh) = funding_candidates(&node, &player, &payer, minimum, config.coinbase_maturity_daa, &[]).await {
                    *shared.lock().unwrap() = fresh;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        Self { candidates, stopping, worker }
    }
    pub fn candidates(&self) -> Vec<Funding> { self.candidates.lock().unwrap().clone() }
    pub async fn finish(self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.worker.abort();
        let _ = self.worker.await;
    }
}

pub fn reserve_prewarmed_fee(candidates: Vec<Funding>) -> Option<(Funding, super::reservations::Claim)> {
    let (mut candidates, claim) = super::reservations::available(candidates, false).ok()?;
    Some((candidates.remove(0), claim))
}

// Intentionally no Debug implementation: this record contains private keys.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredWallet {
    version: u32,
    player_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fee_key: Option<String>,
}

pub struct Wallet {
    pub player: Keypair,
}
/// All newly created wallets use their player key for payouts and fees.
/// Older files may retain a `fee_key`, but it is deliberately ignored.
pub fn funding_key<'a>(funding: &Funding, player: &'a Keypair, payer: &'a Keypair) -> Result<&'a Keypair> {
    if funding.utxo.script_public_key == p2pk(&pubkey(payer)) { Ok(payer) }
    else if funding.utxo.script_public_key == p2pk(&pubkey(player)) { Ok(player) }
    else { Err("funding coin does not belong to this wallet".into()) }
}
pub async fn funding_candidates(node: &super::rpc::Node, player: &Keypair, payer: &Keypair,
    minimum: u64, maturity: u64, reserved: &[kaspa_consensus_core::tx::TransactionOutpoint]) -> Result<Vec<Funding>> {
    let mut reservations = super::hosted::reserved()?;
    reservations.extend_from_slice(reserved);
    let reserved = reservations.as_slice();
    let mut coins = node.funding_candidates(&pubkey(payer), minimum, maturity, reserved).await?;
    if pubkey(player) != pubkey(payer) {
        coins.extend(node.funding_candidates(&pubkey(player), minimum, maturity, reserved).await?);
    }
    Ok(coins)
}

/// Gameplay needs one usable coin, not launch's exhaustive mass comparison.
/// An optional player-address lookup must not delay or invalidate payer success.
pub async fn fee_funding(node: &super::rpc::Node, player: &Keypair, payer: &Keypair,
    minimum: u64, maturity: u64, reserved: &[kaspa_consensus_core::tx::TransactionOutpoint]) -> Result<Funding> {
    let mut reservations = super::hosted::reserved()?;
    reservations.extend_from_slice(reserved);
    let reserved = reservations.as_slice();
    let primary = node.funding_candidates(&pubkey(payer), minimum, maturity, reserved).await?;
    first_fee_coin(primary, pubkey(player) != pubkey(payer), || async {
        node.funding_candidates(&pubkey(player), minimum, maturity, reserved).await
    }).await
}

async fn first_fee_coin<F: std::future::Future<Output = Result<Vec<Funding>>>>(
    primary: Vec<Funding>, has_alternate: bool, alternate: impl FnOnce() -> F,
) -> Result<Funding> {
    if let Some(coin) = primary.into_iter().next() { return Ok(coin); }
    if has_alternate {
        if let Some(coin) = alternate().await?.into_iter().next() { return Ok(coin); }
    }
    Err("no mature owned fee coin; fund the player or fee address".into())
}

/// Atomically select and reserve one fee coin against all client pipelines.
pub async fn reserved_fee_funding(node: &super::rpc::Node, player: &Keypair, payer: &Keypair,
    minimum: u64, maturity: u64, excluded: &[kaspa_consensus_core::tx::TransactionOutpoint],
) -> Result<(Funding, super::reservations::Claim)> {
    let mut blocked = super::reservations::reserved()?;
    blocked.extend_from_slice(excluded);
    let primary = node.funding_candidates(&pubkey(payer), minimum, maturity, &blocked).await?;
    if let Ok((mut coins, claim)) = super::reservations::available(primary, false) { return Ok((coins.remove(0), claim)); }
    if pubkey(player) != pubkey(payer) {
        let alternate = node.funding_candidates(&pubkey(player), minimum, maturity, &blocked).await?;
        let (mut coins, claim) = super::reservations::available(alternate, false)?;
        return Ok((coins.remove(0), claim));
    }
    Err("insufficient AVAILABLE fee funds: coins are missing or reserved by another game/refund".into())
}
impl Wallet {
    fn random_key() -> Result<Keypair> {
        loop {
            let mut bytes = [0u8; 32];
            getrandom::getrandom(&mut bytes).map_err(|_| "operating system randomness is unavailable")?;
            if let Ok(secret) = SecretKey::from_slice(&bytes) {
                return Ok(Keypair::from_secret_key(&Secp256k1::new(), &secret));
            }
        }
    }
    pub fn generate() -> Result<Self> {
        Ok(Self { player: Self::random_key()? })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let stored: StoredWallet = serde_json::from_slice(&std::fs::read(path)?).map_err(|_| "invalid wallet file")?;
        if stored.version != 1 {
            return Err("unsupported wallet file version".into());
        }
        fn parse(text: &str) -> Result<Keypair> {
            let secret: SecretKey = text.parse().map_err(|_| "invalid private key in wallet file")?;
            Ok(Keypair::from_secret_key(&Secp256k1::new(), &secret))
        }
        let player = parse(&stored.player_key)?;
        Ok(Self { player })
    }

    pub fn save_new(&self, path: &Path) -> Result<()> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let stored = StoredWallet {
            version: 1,
            player_key: self.player.secret_key().display_secret().to_string(),
            fee_key: None,
        };
        // NamedTempFile is created with owner-only permissions on Unix. Persist
        // without replacement so an existing funded key can never be overwritten.
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(&serde_json::to_vec_pretty(&stored)?)?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(path).map_err(|e| {
            if e.error.kind() == std::io::ErrorKind::AlreadyExists {
                "wallet already exists; reuse it with --wallet FILE address, or choose a different --out FILE".into()
            } else {
                Box::<dyn std::error::Error>::from(e.error)
            }
        })?;
        Ok(())
    }
}

pub fn public_identity(network: &str, player: &Keypair) -> Result<serde_json::Value> {
    let network: NetworkId = network.parse()?;
    let prefix = match network.network_type {
        NetworkType::Mainnet => Prefix::Mainnet,
        NetworkType::Testnet => Prefix::Testnet,
        NetworkType::Devnet => Prefix::Devnet,
        NetworkType::Simnet => Prefix::Simnet,
    };
    let player_public = player.x_only_public_key().0;
    let address = Address::new(prefix, Version::PubKey, &player_public.serialize()).to_string();
    Ok(serde_json::json!({
        "network": network.to_string(),
        "player": player_public.to_string(),
        "address": address,
        "player_address": address,
    }))
}

/// Spend one mature ordinary wallet UTXO. This intentionally does no game
/// recovery or state transition; it is only for returning ordinary funds.
pub async fn send(node: &Node, config: &Config, key: &Keypair, destination: &str, amount: u64) -> Result<kaspa_consensus_core::Hash> {
    let (transaction, fee) = prepare_send(node, config, key, destination, amount).await?;
    eprintln!("Transaction fee: {} KAS", super::super::menu::format_kas(fee));
    node.submit(&transaction).await
}

pub async fn prepare_send(node: &Node, config: &Config, key: &Keypair, destination: &str, amount: u64) -> Result<(Transaction, u64)> {
    if amount == 0 { return Err("send amount must be positive".into()); }
    let destination: Address = destination.try_into().map_err(|_| "invalid destination Kaspa address")?;
    if destination.prefix != node.prefix { return Err("destination address network differs from the selected node".into()); }
    let minimum = amount.checked_add(1).ok_or("send amount overflow")?;
    // Send deliberately overrides local game and recovery reservations.
    let coins = node.funding_candidates(&pubkey(key), minimum, config.coinbase_maturity_daa, &[]).await?;
    build_send(config, key, &destination, amount, &coins)
}

fn build_send(config: &Config, key: &Keypair, destination: &Address, amount: u64, coins: &[Funding]) -> Result<(Transaction, u64)> {
    let mut last_error = None;
    for funding in coins {
    let transaction = priced_transaction(config, |fee| {
        let change = funding.utxo.amount.checked_sub(amount).and_then(|v| v.checked_sub(fee))
            .ok_or("selected coin cannot cover the send amount and fee")?;
        let mut outputs = vec![TransactionOutput::new(amount, pay_to_address_script(destination))];
        if change > 0 { outputs.push(TransactionOutput::new(change, funding.utxo.script_public_key.clone())); }
        let mut transaction = Transaction::new(TX_VERSION_TOCCATA,
            vec![kaspa_consensus_core::tx::TransactionInput::new(funding.outpoint, vec![], 0, 0)], outputs, 0, Default::default(), 0, vec![]);
        // Ordinary P2PK spends do not execute a covenant, but v1 transactions
        // must still commit enough budget for their signature before pricing.
        transaction.inputs[0].compute_commit = kaspa_consensus_core::mass::ComputeBudget(P2PK_COMPUTE_BUDGET).into();
        let mut mutable = MutableTransaction::new(transaction.clone());
        mutable.entries = vec![Some(funding.utxo.clone())];
        transaction.inputs[0].signature_script = ScriptBuilder::new().add_data(&sign(&mutable, 0, key))?.drain();
        Ok(transaction)
    });
    match transaction {
        Ok(transaction) => {
            let fee = super::client::transaction_fee(&transaction, &[funding.utxo.clone()])?;
            return Ok((transaction, fee));
        }
        Err(error) => last_error = Some(error),
    }
    }
    Err(last_error.unwrap_or_else(|| "no single mature wallet coin can cover the send amount and fee".into()))
}

pub async fn prepare_send_all(node: &Node, config: &Config, key: &Keypair, destination: &str) -> Result<(Transaction, u64)> {
    let destination: Address = destination.try_into().map_err(|_| "invalid destination Kaspa address")?;
    if destination.prefix != node.prefix { return Err("destination address network differs from the selected node".into()); }
    // Include reserved coins, as in an ordinary Send. Immature and covenant
    // outputs cannot be spent by this wallet transaction.
    let coins = node.funding_candidates(&pubkey(key), 1, config.coinbase_maturity_daa, &[]).await?;
    let tx = build_send_all(config, key, &destination, &coins)?;
    let fee = super::client::transaction_fee(&tx, &coins.iter().map(|coin| coin.utxo.clone()).collect::<Vec<_>>())?;
    Ok((tx, fee))
}

fn build_send_all(config: &Config, key: &Keypair, destination: &Address, coins: &[Funding]) -> Result<Transaction> {
    if coins.is_empty() { return Err("no mature wallet coins available to send".into()); }
    let total = coins.iter().try_fold(0u64, |sum, coin| sum.checked_add(coin.utxo.amount).ok_or("wallet balance overflow"))?;
    priced_transaction(config, |fee| {
        let amount = total.checked_sub(fee).filter(|amount| *amount > 0).ok_or("wallet balance cannot cover the transaction fee")?;
        let inputs = coins.iter().map(|coin| {
            let mut input = kaspa_consensus_core::tx::TransactionInput::new(coin.outpoint, vec![], 0, 0);
            input.compute_commit = kaspa_consensus_core::mass::ComputeBudget(P2PK_COMPUTE_BUDGET).into();
            input
        }).collect();
        let mut transaction = Transaction::new(TX_VERSION_TOCCATA, inputs,
            vec![TransactionOutput::new(amount, pay_to_address_script(destination))], 0, Default::default(), 0, vec![]);
        let mut mutable = MutableTransaction::new(transaction.clone());
        mutable.entries = coins.iter().map(|coin| Some(coin.utxo.clone())).collect();
        for (index, input) in transaction.inputs.iter_mut().enumerate() {
            input.signature_script = ScriptBuilder::new().add_data(&sign(&mutable, index, key))?.drain();
        }
        Ok(transaction)
    })
}

#[cfg(test)]
mod fee_funding_tests {
    use super::*;
    use kaspa_consensus_core::{Hash, tx::{TransactionOutpoint, UtxoEntry}};
    fn coin() -> Funding {
        Funding { outpoint: TransactionOutpoint::new(Hash::from_bytes([1; 32]), 0),
            utxo: UtxoEntry::new(100_000_000, p2pk(&[2; 32]), 1, false, None) }
    }
    #[test]
    fn send_selects_coin_using_actual_fee_instead_of_cap() {
        let key = Keypair::from_secret_key(&secp256k1::Secp256k1::new(), &secp256k1::SecretKey::from_slice(&[3; 32]).unwrap());
        let destination = Address::new(Prefix::Testnet, Version::PubKey, &pubkey(&key));
        let config = Config::default();
        let amount = 100_000_000;
        let mut too_small = coin();
        too_small.utxo.script_public_key = p2pk(&pubkey(&key));
        too_small.utxo.amount = amount + 1;
        let mut enough = too_small.clone();
        enough.outpoint = TransactionOutpoint::new(Hash::from_bytes([2; 32]), 0);
        enough.utxo.amount = amount + 1_000_000;
        assert!(build_send(&config, &key, &destination, amount, &[]).is_err());
        assert!(build_send(&config, &key, &destination, amount, &[too_small.clone()]).is_err());
        let (tx, fee) = build_send(&config, &key, &destination, amount, &[too_small, enough.clone()]).unwrap();
        assert_eq!(tx.inputs[0].previous_outpoint, enough.outpoint);
        assert_eq!(tx.outputs[0].value, amount);
        assert!(fee <= 1_000_000 && fee < config.fee_sompi);
        assert_eq!(tx.outputs.iter().map(|output| output.value).sum::<u64>() + fee, enough.utxo.amount);
        use kaspa_consensus_core::hashing::{sighash::{calc_schnorr_signature_hash, SigHashReusedValuesUnsync}, sighash_type::SIG_HASH_ALL};
        let mut signed = MutableTransaction::new(tx.clone());
        signed.entries = vec![Some(enough.utxo)];
        let signature = secp256k1::schnorr::Signature::from_slice(&tx.inputs[0].signature_script[1..65]).unwrap();
        let hash = calc_schnorr_signature_hash(&signed.as_verifiable(), 0, SIG_HASH_ALL, &SigHashReusedValuesUnsync::new());
        secp256k1::Secp256k1::verification_only().verify_schnorr(&signature,
            &secp256k1::Message::from_digest_slice(&hash.as_bytes()).unwrap(), &key.x_only_public_key().0).unwrap();
    }
    #[test]
    fn send_all_combines_coins_and_deducts_fee() {
        let key = Keypair::from_secret_key(&secp256k1::Secp256k1::new(), &secp256k1::SecretKey::from_slice(&[3; 32]).unwrap());
        let destination = Address::new(Prefix::Testnet, Version::PubKey, &pubkey(&key));
        let mut first = coin();
        first.utxo.script_public_key = p2pk(&pubkey(&key));
        let mut second = first.clone();
        second.outpoint = TransactionOutpoint::new(Hash::from_bytes([2; 32]), 0);
        second.utxo.amount = 200_000_000;
        let coins = [first, second];
        let tx = build_send_all(&Config::default(), &key, &destination, &coins).unwrap();
        assert_eq!(tx.inputs.len(), 2);
        assert_eq!(tx.outputs.len(), 1);
        assert!(tx.outputs[0].value < 300_000_000 && tx.outputs[0].value > 0);
        assert!(tx.inputs.iter().all(|input| !input.signature_script.is_empty()));
        assert_eq!(300_000_000 - tx.outputs[0].value, super::super::client::relay_fee(&tx, &Config::default()).unwrap());
        let mut signed = MutableTransaction::new(tx.clone());
        signed.entries = coins.iter().map(|coin| Some(coin.utxo.clone())).collect();
        for (index, input) in tx.inputs.iter().enumerate() {
            use kaspa_consensus_core::hashing::{sighash::{calc_schnorr_signature_hash, SigHashReusedValuesUnsync}, sighash_type::SIG_HASH_ALL};
            assert_eq!(input.signature_script.len(), 66);
            assert_eq!(input.signature_script[0], 65);
            let hash = calc_schnorr_signature_hash(&signed.as_verifiable(), index, SIG_HASH_ALL, &SigHashReusedValuesUnsync::new());
            let signature = secp256k1::schnorr::Signature::from_slice(&input.signature_script[1..65]).unwrap();
            secp256k1::Secp256k1::verification_only().verify_schnorr(&signature,
                &secp256k1::Message::from_digest_slice(&hash.as_bytes()).unwrap(), &key.x_only_public_key().0).unwrap();
            assert_eq!(input.signature_script[65], SIG_HASH_ALL.to_u8());
        }
        assert!(build_send_all(&Config::default(), &key, &destination, &[]).is_err());
    }
    #[tokio::test]
    async fn usable_payer_never_queries_optional_player_even_if_it_would_fail() {
        let chosen = first_fee_coin(vec![coin()], true, || {
            panic!("optional player lookup must not run");
            #[allow(unreachable_code)]
            std::future::ready(Err("optional RPC failure".into()))
        }).await.unwrap();
        assert_eq!(chosen.outpoint, coin().outpoint);
    }
    #[tokio::test]
    async fn player_fallback_runs_only_when_needed_and_errors_remain_visible() {
        let chosen = first_fee_coin(vec![], true, || std::future::ready(Ok(vec![coin()]))).await.unwrap();
        assert_eq!(chosen.outpoint, coin().outpoint);
        let error = first_fee_coin(vec![], true, || std::future::ready(Err("player RPC failed".into()))).await.unwrap_err();
        assert!(error.to_string().contains("player RPC failed"));
        assert!(first_fee_coin(vec![], true, || std::future::ready(Ok(vec![]))).await.is_err());
    }
    #[tokio::test]
    async fn equal_keys_do_not_repeat_the_empty_address_lookup() {
        assert!(first_fee_coin(vec![], false, || {
            panic!("same address must not be queried twice");
            #[allow(unreachable_code)]
            std::future::ready(Ok(vec![]))
        }).await.is_err());
    }

    #[test]
    fn p2pk_budget_covers_one_signature() {
        use kaspa_consensus_core::{mass::ComputeBudget, tx::ComputeCommit};
        let budget: ComputeCommit = ComputeBudget(P2PK_COMPUTE_BUDGET).into();
        assert!(budget.allowed_script_units().0 >= 100_000);
        let too_small: ComputeCommit = ComputeBudget(P2PK_COMPUTE_BUDGET - 1).into();
        assert!(too_small.allowed_script_units().0 < 100_000);
    }
}
