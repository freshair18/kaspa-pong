//! Durable same-wallet preparation for wagers spread across multiple outputs.
use super::{Config, reservations::{self, Claim}, rpc::Node, wallet};
use crate::{engine::{Funding, Result, p2pk, pubkey}, network_ui::Setup};
use kaspa_consensus_core::{tx::{Transaction, TransactionOutpoint, UtxoEntry}};
use secp256k1::Keypair;
use serde::{Deserialize, Serialize};
use std::{path::{Path, PathBuf}, sync::Arc, time::{Duration, Instant}};

type FundingCheck = Arc<dyn Fn(Vec<Funding>) -> std::result::Result<bool, String> + Send + Sync>;

fn host_check(a: &argent_runtime::Artifact, invite: &super::tracker::Invitation,
    proposal: &super::setup::StartPackage, player: &Keypair, config: &Config) -> FundingCheck {
    let (a, invite, proposal, player, config) = (a.clone(), invite.clone(), proposal.clone(), *player, config.clone());
    Arc::new(move |coins| super::setup::approve_if_funded(&a, &invite, proposal.clone(), coins, &player, &player, &config)
        .map(|package| package.is_some()).map_err(|error| error.to_string()))
}
async fn usable(check: &Option<FundingCheck>, coins: Vec<Funding>) -> Result<bool> {
    let Some(check) = check.clone() else { return Ok(true); };
    Ok(tokio::task::spawn_blocking(move || check(coins)).await??)
}

#[derive(Serialize, Deserialize)]
struct Journal {
    version: u32,
    network: String,
    player: Vec<u8>,
    transaction: Transaction,
    inputs: Vec<UtxoEntry>,
    fee: u64,
}
impl Journal {
    fn output(&self) -> TransactionOutpoint { TransactionOutpoint::new(self.transaction.id(), 0) }
    fn reservations(&self) -> Vec<TransactionOutpoint> {
        self.transaction.inputs.iter().map(|input| input.previous_outpoint).chain([self.output()]).collect()
    }
    fn validate(&self, config: &Config, player: &Keypair) -> Result<()> {
        let tx = &self.transaction;
        let script = p2pk(&pubkey(player));
        let distinct: std::collections::HashSet<_> = tx.inputs.iter().map(|input| input.previous_outpoint).collect();
        if self.version != 1 || self.network != config.network || self.player != pubkey(player)
            || tx.inputs.len() < 2 || distinct.len() != tx.inputs.len() || self.inputs.len() != tx.inputs.len()
            || tx.outputs.len() != 1 || tx.outputs[0].value == 0 || tx.outputs[0].script_public_key != script
            || tx.outputs[0].covenant.is_some() || !tx.payload.is_empty()
            || self.inputs.iter().any(|input| input.covenant_id.is_some() || input.script_public_key != script)
            || self.fee == 0 || super::client::transaction_fee(tx, &self.inputs)? != self.fee {
            return Err("invalid wallet preparation journal; funds remain reserved".into());
        }
        for index in 0..self.inputs.len() {
            argent_runtime::execute_input_with_covenants(tx, self.inputs.clone(), index)?;
        }
        Ok(())
    }
    fn accepted_output(&self, coins: Vec<Funding>) -> Option<Funding> {
        coins.into_iter().find(|coin| coin.outpoint == self.output()
            && coin.utxo.amount == self.transaction.outputs[0].value
            && coin.utxo.covenant_id.is_none() && coin.utxo.block_daa_score != u64::MAX
            && coin.utxo.script_public_key == self.transaction.outputs[0].script_public_key)
    }
}
fn path(config: &Config, player: &Keypair) -> PathBuf {
    PathBuf::from("games").join(format!("funding-{}-{}.json", config.network, player.x_only_public_key().0))
}
pub fn pending(config: &Config, player: &Keypair) -> bool { path(config, player).is_file() }
/// Called under the signer locks, before any game or refund can select coins.
pub fn restore(config: &Config, player: &Keypair) -> Result<()> {
    let path = path(config, player);
    if path.is_file() {
        let journal: Journal = super::load(&path)?;
        journal.validate(config, player)?;
        reservations::restore(&path, journal.reservations())?;
    }
    Ok(())
}

trait Backend {
    async fn coins(&self, player: &Keypair, maturity: u64) -> Result<Vec<Funding>>;
    async fn unspent(&self, journal: &Journal) -> Result<bool>;
    async fn submit(&self, tx: &Transaction) -> Result<()>;
}
impl Backend for Node {
    async fn coins(&self, player: &Keypair, maturity: u64) -> Result<Vec<Funding>> {
        self.funding_candidates(&pubkey(player), 1, maturity, &[]).await
    }
    async fn unspent(&self, journal: &Journal) -> Result<bool> {
        self.sources_unspent(&journal.transaction, &journal.inputs).await
    }
    async fn submit(&self, tx: &Transaction) -> Result<()> { self.submit(tx).await.map(|_| ()) }
}

/// Reads are cancellable; durable authorization and exposed submissions finish.
pub async fn fund(node: &Node, config: &Config, player: &Keypair, minimum: u64,
    all: bool, approve: bool, ui: &Setup) -> Result<Option<(Vec<Funding>, Claim)>> {
    fund_with(node, config, player, minimum, all, approve, ui, &path(config, player)).await
}
/// Test the actual start transaction, including its fee and contextual mass,
/// before choosing an existing output or authorizing a same-wallet merge.
pub async fn fund_start(node: &Node, config: &Config, player: &Keypair,
    a: &argent_runtime::Artifact, invite: &super::tracker::Invitation,
    proposal: &super::setup::StartPackage, ui: &Setup) -> Result<Option<(Vec<Funding>, Claim)>> {
    let minimum = crate::engine::stake_amount(invite.lobby.stake_per_player)?.checked_add(1).ok_or("stake overflow")?;
    fund_with_check(node, config, player, minimum, true, false, ui, &path(config, player),
        Some(host_check(a, invite, proposal, player, config))).await
}
async fn fund_with(backend: &impl Backend, config: &Config, player: &Keypair, minimum: u64,
    all: bool, approve: bool, ui: &Setup, path: &Path) -> Result<Option<(Vec<Funding>, Claim)>> {
    fund_with_check(backend, config, player, minimum, all, approve, ui, path, None).await
}
async fn fund_with_check(backend: &impl Backend, config: &Config, player: &Keypair, minimum: u64,
    all: bool, approve: bool, ui: &Setup, path: &Path, check: Option<FundingCheck>) -> Result<Option<(Vec<Funding>, Claim)>> {
    let _lock = super::lock(path)?;
    // An accepted preparation may be too small for a later, larger wager.
    // Keep its output claimed while selecting the additional funding needed.
    let mut carried: Option<(TransactionOutpoint, Claim)> = None;
    'prepare: loop {
        let journal = if path.is_file() {
            let journal: Journal = super::load(path)?;
            journal.validate(config, player)?;
            reservations::restore(path, journal.reservations())?;
            journal
        } else {
            let Some(coins) = ui.read(backend.coins(player, config.coinbase_maturity_daa)).await else { return Ok(None); };
            let mut coins = coins?;
            let blocked = reservations::reserved()?;
            coins.retain(|coin| !blocked.contains(&coin.outpoint) || carried.as_ref().is_some_and(|(outpoint, _)| *outpoint == coin.outpoint));
            let eligible: Vec<_> = coins.iter().filter(|coin| coin.utxo.amount >= minimum).cloned().collect();
            if !eligible.is_empty() {
                let Some(suitable) = ui.read(usable(&check, eligible.clone())).await else { return Ok(None); };
                if suitable? { return claim_candidates(eligible, all, carried.take()).map(Some); }
            }
            let (coins, mut claim) = claim_candidates(coins, true, carried.take())?;
            let (config_copy, player_copy, check_copy) = (config.clone(), *player, check.clone());
            let build = tokio::task::spawn_blocking(move || {
                let result = match check_copy {
                    None => wallet::build_wager_consolidation(&config_copy, &player_copy, minimum, &coins),
                    Some(check) => wallet::build_wager_consolidation_checked(&config_copy, &player_copy, minimum, &coins,
                        |coin| check(vec![coin.clone()]).map_err(Into::into)),
                };
                result.map_err(|error| error.to_string())
            });
            let Some(built) = ui.read(build).await else { return Ok(None); };
            let (tx, chosen, fee) = built??;
            claim.restrict(chosen.iter().map(|coin| coin.outpoint).collect())?;
            ui.set_status(&format!("Preparing wallet funds: combine {} outputs at your own address.\nPreparation fee: {} KAS. Game transaction fee allowance: up to {} KAS.\nYour wager is not locked yet.", chosen.len(), super::fees::coins(u128::from(fee)), super::fees::coins(u128::from(config.fee_sompi))));
            super::rpc::startup::diagnostic(&serde_json::json!({"type":"wallet_preparation_ready", "inputs":chosen.len(), "fee_sompi":fee, "output_sompi":tx.outputs[0].value}).to_string());
            if approve && !ui.confirm().await? { return Ok(None); }
            if ui.cancelled() { return Ok(None); }
            let journal = Journal { version: 1, network: config.network.clone(), player: pubkey(player),
                transaction: tx, inputs: chosen.into_iter().map(|coin| coin.utxo).collect(), fee };
            journal.validate(config, player)?;
            let Some(unspent) = ui.read(backend.unspent(&journal)).await else { return Ok(None); };
            if !unspent? { return Err("wallet preparation inputs changed; retry setup with available funds".into()); }
            if ui.cancelled() { return Ok(None); }
            // Preserve uncertain inputs and the predicted output before exposure.
            super::save(path, &journal)?;
            reservations::restore(path, journal.reservations())?;
            drop(claim);
            journal
        };
        let mut submitted = false;
        let started = Instant::now();
        loop {
            let Some(coins) = ui.read(backend.coins(player, config.coinbase_maturity_daa)).await else { return Ok(None); };
            let coins = coins?;
            if let Some(output) = journal.accepted_output(coins.clone()) {
                let suitable = if output.utxo.amount >= minimum {
                    let Some(suitable) = ui.read(usable(&check, vec![output.clone()])).await else { return Ok(None); };
                    suitable?
                } else { false };
                let (outputs, claim) = handoff(path, output)?;
                if suitable { return Ok(Some((outputs, claim))); }
                carried = Some((outputs[0].outpoint, claim));
                continue 'prepare;
            }
            // A pending preparation must not gate independent, usable funds.
            // Its journal and reservations remain intact for background recovery.
            if let Some(funding) = independent_funding(coins, minimum, all)? {
                let Some(suitable) = ui.read(usable(&check, funding.0.clone())).await else { return Ok(None); };
                if suitable? { return Ok(Some(funding)); }
            }
            if ui.cancelled() { return Ok(None); }
            if !submitted {
                ui.set_status(&format!("Combining wallet outputs at your own address.\nPreparation fee: {} KAS. Waiting for acceptance.\nEsc: return; the authorized transfer may still finish.", super::fees::coins(u128::from(journal.fee))));
                if let Err(error) = backend.submit(&journal.transaction).await {
                    super::rpc::startup::diagnostic(&serde_json::json!({"type":"wallet_preparation_submit", "message":error.to_string()}).to_string());
                }
                submitted = true;
                continue;
            }
            if started.elapsed() >= Duration::from_secs(config.pending_timeout_secs.clamp(10, 120)) {
                return Err("Wallet preparation is still pending. Its funds remain reserved; the same transaction will be resumed automatically.".into());
            }
            if ui.read(tokio::time::sleep(Duration::from_millis(config.poll_ms.max(25)))).await.is_none() { return Ok(None); }
        }
    }
}
fn independent_funding(coins: Vec<Funding>, minimum: u64, all: bool) -> Result<Option<(Vec<Funding>, Claim)>> {
    let blocked = reservations::reserved()?;
    let eligible = coins.into_iter().filter(|coin| coin.utxo.amount >= minimum && !blocked.contains(&coin.outpoint)).collect::<Vec<_>>();
    if eligible.is_empty() { Ok(None) } else { reservations::available(eligible, all).map(Some) }
}
fn claim_candidates(mut coins: Vec<Funding>, all: bool, carried: Option<(TransactionOutpoint, Claim)>) -> Result<(Vec<Funding>, Claim)> {
    match carried {
        None => reservations::available(coins, all),
        Some((_, mut claim)) => {
            if coins.is_empty() { return Err("not enough mature, unreserved funds for the current wager".into()); }
            if !all { coins.truncate(1); }
            claim.restrict(coins.iter().map(|coin| coin.outpoint).collect())?;
            Ok((coins, claim))
        }
    }
}

fn handoff(path: &Path, output: Funding) -> Result<(Vec<Funding>, Claim)> {
    // Acceptance is proven before retiring the authorization. A crash after
    // removal leaves a normal, accepted wallet output, never an unknown spend.
    std::fs::remove_file(path)?;
    let claim = reservations::claim_journal_output(path, output.outpoint)?;
    Ok((vec![output], claim))
}

/// Finish an already-authorized transfer after leaving setup or restarting.
pub async fn recover_once(node: &Node, config: &Config, player: &Keypair) -> Result<()> {
    let path = path(config, player);
    if !path.is_file() { return Ok(()); }
    let _lock = match super::lock(&path) {
        Ok(lock) => lock,
        Err(error) if error.to_string().contains("another client") => return Ok(()),
        Err(error) => return Err(error),
    };
    let journal: Journal = super::load(&path)?;
    journal.validate(config, player)?;
    reservations::restore(&path, journal.reservations())?;
    let coins = Backend::coins(node, player, config.coinbase_maturity_daa).await?;
    if journal.accepted_output(coins).is_some() {
        std::fs::remove_file(&path)?;
        reservations::restore(&path, vec![])?;
    } else {
        if let Err(error) = node.submit(&journal.transaction).await {
            super::rpc::startup::diagnostic(&serde_json::json!({"type":"wallet_preparation_retry", "message":error.to_string()}).to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::accepted_cell;
    use kaspa_consensus_core::Hash;
    use std::sync::{Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}};

    struct Fake {
        coins: Vec<Funding>,
        path: PathBuf,
        tx: Mutex<Option<Transaction>>,
        submits: AtomicUsize,
        fail_ack: bool,
        fail_read: AtomicBool,
        accepted: AtomicBool,
        extra: Vec<Funding>,
    }
    impl Backend for Fake {
        async fn coins(&self, _: &Keypair, _: u64) -> Result<Vec<Funding>> {
            if let Some(tx) = &*self.tx.lock().unwrap() {
                if self.fail_read.load(Ordering::Relaxed) { return Err("synthetic read outage".into()); }
                if self.accepted.load(Ordering::Relaxed) {
                    let mut coins = vec![Funding { outpoint: TransactionOutpoint::new(tx.id(), 0), utxo: accepted_cell(tx, 0, 100, ()).utxo }];
                    coins.extend(self.extra.iter().filter(|coin| !tx.inputs.iter().any(|input| input.previous_outpoint == coin.outpoint)).cloned());
                    return Ok(coins);
                }
            }
            Ok(self.coins.iter().chain(&self.extra).cloned().collect())
        }
        async fn unspent(&self, _: &Journal) -> Result<bool> { Ok(true) }
        async fn submit(&self, tx: &Transaction) -> Result<()> {
            let journal: Journal = super::super::load(&self.path)?;
            assert_eq!(journal.transaction.id(), tx.id(), "authorization must be durable before RPC exposure");
            let blocked = reservations::reserved()?;
            assert!(journal.reservations().iter().all(|input| blocked.contains(input)), "inputs and future output are reserved before submission");
            self.submits.fetch_add(1, Ordering::Relaxed);
            *self.tx.lock().unwrap() = Some(tx.clone());
            if self.fail_ack { Err("synthetic ambiguous submission".into()) } else { Ok(()) }
        }
    }
    fn key() -> Keypair {
        Keypair::from_secret_key(&secp256k1::SECP256K1, &secp256k1::SecretKey::from_slice(&[17; 32]).unwrap())
    }
    fn coin(label: &str, amount: u64) -> Funding {
        let id: [u8;32] = blake2b_simd::Params::new().hash_length(32).hash(label.as_bytes()).as_bytes().try_into().unwrap();
        Funding { outpoint: TransactionOutpoint::new(Hash::from_bytes(id), 0),
            utxo: UtxoEntry::new(amount, p2pk(&pubkey(&key())), 1, false, None) }
    }
    fn backend(path: &Path, label: &str) -> Fake {
        Fake { coins: vec![coin(&format!("{label}-192"), 19_200_000_000), coin(&format!("{label}-10"), 1_000_000_000)],
            path: path.into(), tx: Mutex::new(None), submits: AtomicUsize::new(0), fail_ack: false, fail_read: AtomicBool::new(false), accepted: AtomicBool::new(true), extra: Vec::new() }
    }
    fn host_fixture() -> (argent_runtime::Artifact, super::super::tracker::Invitation, super::super::setup::StartPackage, Config) {
        use crate::engine::{Lobby, SIDE_DEPOSIT, DEFAULT_STAKE, DEFAULT_BALL_TICK_DAA};
        let a = super::super::load(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/artifacts/artifact.json"))).unwrap();
        let guest = Keypair::from_secret_key(&secp256k1::SECP256K1, &secp256k1::SecretKey::from_slice(&[18; 32]).unwrap());
        let lobby = Lobby { left: pubkey(&guest), right: pubkey(&key()), refund: pubkey(&guest),
            control_value: SIDE_DEPOSIT as i64, ball_tick_daa: DEFAULT_BALL_TICK_DAA,
            stake_per_player: DEFAULT_STAKE, ball_count: 1, win_score: 3 };
        let mut funding = coin("host-fixture-guest", 100_000_000_000);
        funding.utxo.script_public_key = p2pk(&pubkey(&guest));
        let invite = super::super::invitations::create(&a, "devnet", Hash::from_bytes([0; 32]), &lobby, &funding, &guest, &guest, 100_000).unwrap();
        let accepted = accepted_cell(&invite.genesis, 0, 100, lobby);
        let proposal = super::super::setup::prepare(&a, &invite, &accepted).unwrap();
        (a, invite, proposal, Config { network: "devnet".into(), ..Default::default() })
    }
    #[tokio::test]
    async fn host_consolidates_when_existing_coin_cannot_cover_start_fee_or_storage_mass() {
        let (a, invite, proposal, config) = host_fixture();
        let stake = crate::engine::stake_amount(invite.lobby.stake_per_player).unwrap();
        let pilot = super::super::setup::approve(&a, &invite, proposal.clone(),
            vec![coin("host-pilot", 100_000_000_000)], &key(), &key(), &config).unwrap();
        for (label, remainder) in [("host-fee", 1), ("host-mass", pilot.fee + 1)] {
            let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("preparation.json");
            let mut backend = backend(&path, label);
            backend.coins = vec![coin(&format!("{label}-near-stake"), stake + remainder), coin(&format!("{label}-extra"), 100_000_000)];
            assert!(super::super::setup::approve_if_funded(&a, &invite, proposal.clone(), backend.coins.clone(), &key(), &key(), &config).unwrap().is_none());
            let ui = Setup::new(false).unwrap();
            let (coins, claim) = fund_with_check(&backend, &config, &key(), stake + 1, true, false, &ui, &path,
                Some(host_check(&a, &invite, &proposal, &key(), &config))).await.unwrap().unwrap();
            assert_eq!(backend.submits.load(Ordering::Relaxed), 1);
            assert_eq!(backend.tx.lock().unwrap().as_ref().unwrap().inputs.len(), 2);
            let package = super::super::setup::approve(&a, &invite, proposal.clone(), coins.clone(), &key(), &key(), &config).unwrap();
            super::super::setup::validate(&a, &invite, &package, config.fee_sompi).unwrap();
            assert!(Claim::inputs(vec![coins[0].outpoint]).is_err());
            assert!(!path.exists()); drop(claim);
        }
    }
    #[tokio::test]
    async fn host_uses_valid_existing_coin_without_consolidation() {
        let (a, invite, proposal, config) = host_fixture();
        let stake = crate::engine::stake_amount(invite.lobby.stake_per_player).unwrap();
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("preparation.json");
        let mut backend = backend(&path, "host-valid");
        backend.coins = vec![coin("host-valid-tiny", stake + 1), coin("host-valid-large", 100_000_000_000)];
        let ui = Setup::new(false).unwrap();
        let (coins, claim) = fund_with_check(&backend, &config, &key(), stake + 1, true, false, &ui, &path,
            Some(host_check(&a, &invite, &proposal, &key(), &config))).await.unwrap().unwrap();
        assert_eq!(backend.submits.load(Ordering::Relaxed), 0);
        assert!(!path.exists());
        super::super::setup::approve(&a, &invite, proposal, coins, &key(), &key(), &config).unwrap();
        drop(claim);
    }
    #[tokio::test]
    async fn host_does_not_submit_preparation_that_still_cannot_fund_start() {
        let (a, invite, proposal, config) = host_fixture();
        let stake = crate::engine::stake_amount(invite.lobby.stake_per_player).unwrap();
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("preparation.json");
        let mut backend = backend(&path, "host-insufficient");
        backend.coins = vec![coin("host-insufficient-near-stake", stake + 1), coin("host-insufficient-extra", 100)];
        let ui = Setup::new(false).unwrap();
        assert!(fund_with_check(&backend, &config, &key(), stake + 1, true, false, &ui, &path,
            Some(host_check(&a, &invite, &proposal, &key(), &config))).await.is_err());
        assert_eq!(backend.submits.load(Ordering::Relaxed), 0);
        assert!(!path.exists());
        assert!(backend.coins.iter().all(|coin| !reservations::reserved().unwrap().contains(&coin.outpoint)));
    }
    #[tokio::test]
    async fn fragmented_202_kas_funds_201_kas_wager_and_holds_output_for_launch() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("preparation.json");
        let mut backend = backend(&path, "merge-success");
        backend.coins.push(coin("merge-success-small", 100_000_000));
        let unselected = backend.coins[2].outpoint;
        let ui = Setup::new(false).unwrap();
        let (coins, claim) = fund_with(&backend, &Config::default(), &key(), 20_150_000_000, false, false, &ui, &path).await.unwrap().unwrap();
        assert_eq!(backend.submits.load(Ordering::Relaxed), 1);
        assert!(!path.exists());
        assert_eq!(coins.len(), 1);
        assert!(coins[0].utxo.amount >= 20_150_000_000);
        let tx = backend.tx.lock().unwrap().clone().unwrap();
        assert_eq!(tx.inputs.len(), 2, "small unrelated outputs are left alone");
        assert_eq!(tx.outputs[0].script_public_key, p2pk(&pubkey(&key())));
        assert!(tx.outputs[0].value < 20_200_000_000, "the consolidation fee is deducted once");
        assert!(Claim::inputs(vec![coins[0].outpoint]).is_err(), "background workers cannot grab the consolidated output");
        assert!(!reservations::reserved().unwrap().contains(&unselected));
        drop(claim);
        assert!(Claim::inputs(vec![coins[0].outpoint]).is_ok());
    }
    #[tokio::test]
    async fn enough_single_output_skips_preparation_and_reserved_outputs_are_excluded() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("preparation.json");
        let mut backend = backend(&path, "merge-reserved");
        let large = coin("merge-reserved-large", 30_000_000_000);
        backend.coins.push(large.clone());
        let ui = Setup::new(false).unwrap();
        let (coins, claim) = fund_with(&backend, &Config::default(), &key(), 20_150_000_000, false, false, &ui, &path).await.unwrap().unwrap();
        assert_eq!(coins[0].outpoint, large.outpoint);
        assert_eq!(backend.submits.load(Ordering::Relaxed), 0);
        assert!(!path.exists());
        drop(claim);
        let reserved = Claim::inputs(vec![large.outpoint]).unwrap();
        let (_, claim) = fund_with(&backend, &Config::default(), &key(), 20_150_000_000, false, false, &ui, &path).await.unwrap().unwrap();
        assert!(!backend.tx.lock().unwrap().as_ref().unwrap().inputs.iter().any(|input| input.previous_outpoint == large.outpoint));
        drop((claim, reserved));
    }
    #[tokio::test]
    async fn ambiguous_ack_uses_acceptance_and_restart_reuses_the_saved_transaction() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("preparation.json");
        let mut backend = backend(&path, "merge-restart");
        backend.fail_ack = true; backend.fail_read.store(true, Ordering::Relaxed);
        let ui = Setup::new(false).unwrap();
        assert!(fund_with(&backend, &Config::default(), &key(), 20_150_000_000, false, false, &ui, &path).await.is_err());
        assert!(path.is_file());
        let saved: Journal = super::super::load(&path).unwrap();
        assert!(saved.reservations().iter().all(|input| reservations::reserved().unwrap().contains(input)));
        assert!(Claim::inputs(vec![backend.coins[0].outpoint]).is_err());
        reservations::restore(&path, vec![]).unwrap(); // Simulate a fresh process's empty book.
        backend.fail_read.store(false, Ordering::Relaxed);
        let (coins, claim) = fund_with(&backend, &Config::default(), &key(), 20_150_000_000, false, false, &ui, &path).await.unwrap().unwrap();
        assert_eq!(coins[0].outpoint.transaction_id, saved.transaction.id());
        assert_eq!(backend.submits.load(Ordering::Relaxed), 1, "accepted preparation is not rebuilt or sent twice");
        assert!(!path.exists()); drop(claim);
    }
    #[tokio::test]
    async fn pending_preparation_does_not_block_fresh_funds_or_release_uncertain_inputs() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("preparation.json");
        let mut backend = backend(&path, "pending-independent");
        backend.accepted.store(false, Ordering::Relaxed);
        backend.fail_read.store(true, Ordering::Relaxed);
        let ui = Setup::new(false).unwrap();
        assert!(fund_with(&backend, &Config::default(), &key(), 20_150_000_000, false, false, &ui, &path).await.is_err());
        let saved: Journal = super::super::load(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let fresh = coin("pending-independent-fresh", 30_000_000_000);
        backend.extra.push(fresh.clone());
        backend.fail_read.store(false, Ordering::Relaxed);
        let result = tokio::time::timeout(Duration::from_secs(1), fund_with(&backend, &Config::default(), &key(), 20_150_000_000, false, false, &ui, &path)).await.unwrap().unwrap().unwrap();
        assert_eq!(result.0[0].outpoint, fresh.outpoint);
        assert_eq!(backend.submits.load(Ordering::Relaxed), 1, "fresh funding does not wait on or resubmit the old transaction");
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "uncertain authorization remains durable");
        assert!(saved.reservations().iter().all(|input| reservations::reserved().unwrap().contains(input)));
        assert!(Claim::inputs(vec![backend.coins[0].outpoint]).is_err());
        assert!(Claim::inputs(vec![fresh.outpoint]).is_err(), "fresh output is claimed for launch");
        drop(result);
        reservations::restore(&path, vec![]).unwrap();
    }
    #[tokio::test]
    async fn larger_wager_reconsolidates_accepted_output_with_additional_funds() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("preparation.json");
        let mut backend = backend(&path, "resume-larger");
        backend.fail_read.store(true, Ordering::Relaxed);
        let ui = Setup::new(false).unwrap();
        assert!(fund_with(&backend, &Config::default(), &key(), 20_150_000_000, false, false, &ui, &path).await.is_err());
        let old: Journal = super::super::load(&path).unwrap();
        backend.fail_read.store(false, Ordering::Relaxed);
        let extra = coin("resume-larger-extra", 6_000_000_000);
        backend.extra.push(extra.clone());
        let (coins, claim) = fund_with(&backend, &Config::default(), &key(), 25_450_000_000, false, false, &ui, &path).await.unwrap().unwrap();
        assert!(coins[0].utxo.amount >= 25_450_000_000);
        assert_eq!(backend.submits.load(Ordering::Relaxed), 2, "the larger wager needs one new consolidation after the old one is accepted");
        let tx = backend.tx.lock().unwrap().clone().unwrap();
        assert!(tx.inputs.iter().any(|input| input.previous_outpoint == old.output()));
        assert!(tx.inputs.iter().any(|input| input.previous_outpoint == extra.outpoint));
        assert!(!tx.inputs.iter().any(|input| old.transaction.inputs.iter().any(|old| old.previous_outpoint == input.previous_outpoint)));
        assert!(!path.exists()); drop(claim);
    }
    #[tokio::test]
    async fn larger_wager_never_receives_an_undersized_resumed_output() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("preparation.json");
        let backend = backend(&path, "resume-insufficient");
        backend.fail_read.store(true, Ordering::Relaxed);
        let ui = Setup::new(false).unwrap();
        assert!(fund_with(&backend, &Config::default(), &key(), 20_150_000_000, false, false, &ui, &path).await.is_err());
        let old: Journal = super::super::load(&path).unwrap();
        backend.fail_read.store(false, Ordering::Relaxed);
        assert!(fund_with(&backend, &Config::default(), &key(), 25_450_000_000, false, false, &ui, &path).await.is_err());
        assert_eq!(backend.submits.load(Ordering::Relaxed), 1, "no unnecessary new transaction when the actual balance is insufficient");
        assert!(!path.exists(), "accepted authorization is retired");
        assert!(!reservations::reserved().unwrap().contains(&old.output()), "accepted funds stay usable after setup fails");
    }

    #[tokio::test]
    async fn insufficient_total_including_fee_does_not_submit_or_leave_reservations() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("preparation.json");
        let mut backend = backend(&path, "merge-insufficient");
        backend.coins[0].utxo.amount = 19_150_000_000;
        let ui = Setup::new(false).unwrap();
        assert!(fund_with(&backend, &Config::default(), &key(), 20_150_000_000, false, false, &ui, &path).await.is_err());
        assert!(!path.exists());
        assert_eq!(backend.submits.load(Ordering::Relaxed), 0);
        assert!(backend.coins.iter().all(|coin| !reservations::reserved().unwrap().contains(&coin.outpoint)));
    }
    #[test]
    fn journals_reject_modified_fees_destinations_and_networks() {
        let config = Config::default();
        let coins = vec![coin("merge-validate-192", 19_200_000_000), coin("merge-validate-10", 1_000_000_000)];
        let (tx, chosen, fee) = wallet::build_wager_consolidation(&config, &key(), 20_150_000_000, &coins).unwrap();
        let mut journal = Journal { version: 1, network: config.network.clone(), player: pubkey(&key()),
            transaction: tx.clone(), inputs: chosen.into_iter().map(|coin| coin.utxo).collect(), fee };
        journal.validate(&config, &key()).unwrap();
        journal.transaction.outputs[0].value -= 1; journal.fee += 1;
        assert!(journal.validate(&config, &key()).is_err(), "changing the fee invalidates signatures");
        journal.transaction = tx; journal.fee = fee;
        journal.network = "mainnet".into(); assert!(journal.validate(&config, &key()).is_err());
        journal.network = config.network.clone(); journal.transaction.outputs[0].script_public_key = p2pk(&[2;32]);
        assert!(journal.validate(&config, &key()).is_err());
    }
}
