//! Public host registry and durable, host-only recovery of unjoined invitations.
use super::{Config, client::{Cache, now_ms, priced_transaction}, rpc::Node, tracker::{Invitation, Stage}, wallet};
use crate::engine::*;
use argent_runtime::{Artifact, EntryCall, TxBuilder, TxContext};
use kaspa_consensus_core::tx::{Transaction, TransactionOutpoint};
use secp256k1::Keypair;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const EXPIRY_MS: u64 = 60 * 60 * 1000;
const PURPOSE: &str = "unjoined invitation cancel";
static STATUS: Mutex<String> = Mutex::new(String::new());
static WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();
pub fn status() -> String { STATUS.lock().map(|s| s.clone()).unwrap_or_else(|_| "Recovery: deferred (status unavailable)".into()) }
fn progress(phase: &str, detail: &str, started: std::time::Instant) {
    if let Ok(mut s) = STATUS.lock() {
        *s = match phase {
            "recovery_idle" => String::new(),
            "recovery_deferred" => "Stake refund delayed—retrying automatically. See the startup log for details or required action.".into(),
            "recovery_scan" => "Checking pending stake refunds…".into(),
            "recovery_submit_request" => "Stake refund pending—sending transaction.".into(),
            "recovery_submit_ack" => "Stake refund submitted—waiting for chain acceptance.".into(),
            "recovery_reconciled" if detail == "refund accepted" => "Stake refund accepted on chain.".into(),
            _ => "Stake refund pending—checking the chain. You can continue using the client.".into(),
        };
    }
    super::rpc::startup::phase(phase, started);
}
/// Shares transport, not connection ownership or a wallet-wide signing gate.
pub fn background(a: Artifact, c: Config, player: Keypair, payer: Keypair,
    mut shared: super::rpc::startup::Connection,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let started = std::time::Instant::now();
            progress("recovery_scan", "checking abandoned/expired invitations", started);
            let result = async {
                // Reservation restoration is done once, before funding is
                // enabled. Later scans must not overwrite live journal state.
                let due = recovery_records(&a, &c, &player, &payer, false)?;
                let preparation = super::consolidation::pending(&c, &player);
                if due.is_empty() && !preparation { progress("recovery_idle", "no queued invitation refunds", started); return Ok(()); }
                let n = shared.get(&c).await?;
                if preparation {
                    if let Err(error) = super::consolidation::recover_once(&n, &c, &player).await {
                        super::rpc::startup::diagnostic(&serde_json::json!({"type":"wallet_preparation_recovery", "message":error.to_string()}).to_string());
                    }
                }
                for record in due {
                    if let Err(error) = recover_records(&a, &c, &player, &payer, &n, vec![record]).await {
                        progress("recovery_deferred", "deferred; see startup log", started);
                        super::rpc::startup::diagnostic(&serde_json::json!({"type":"recovery_error","message":error.to_string()}).to_string());
                    }
                    tokio::task::yield_now().await;
                }
                Ok::<(), Box<dyn std::error::Error>>(())
            }.await.map_err(|e| e.to_string());
            if let Err(error) = result {
                progress("recovery_deferred", "deferred; see startup log", started);
                super::rpc::startup::diagnostic(&serde_json::json!({"type":"recovery_error","message":error.to_string()}).to_string());
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {},
                _ = WAKE.notified() => {},
            }
        }
    })
}
#[derive(Default, Serialize, Deserialize)]
struct Registry { records: Vec<Record> }
#[derive(Serialize, Deserialize)]
struct Record {
    invitation: PathBuf,
    created_ms: u64,
    #[serde(default)]
    cancel_requested: bool,
}
impl Record {
    fn due(&self, now: u64) -> bool { self.cancel_requested || expired(self.created_ms, now) }
}
/// Durable intent only: the background worker independently verifies that the
/// exact Lobby is still accepted and unspent before authorizing any refund.
pub fn request_cancel(invitation: &Path, network: &str, player: &[u8]) -> Result<()> {
    let path = registry_path(network, player);
    let _lock = super::lock(&path)?;
    let mut registry = load_registry(&path)?;
    let invitation = invitation.canonicalize()?;
    let Some(record) = registry.records.iter_mut().find(|r| r.invitation == invitation) else {
        let state = invitation.with_file_name("left-state.json");
        if state.is_file() {
            let invite: Invitation = super::load(&invitation)?;
            let cache: Cache = super::load(&state)?;
            if completed_invitation(&invite, &cache, network, player) { return Ok(()); }
        }
        return Err("hosted invitation is not registered for cancellation".into());
    };
    record.cancel_requested = true;
    super::save(&path, &registry)?;
    WAKE.notify_one();
    Ok(())
}

fn completed_invitation(invite: &Invitation, cache: &Cache, network: &str, player: &[u8]) -> bool {
    invite.network == network && invite.lobby.left == player && cache.network == network
        && cache.game == invite.genesis.id() && cache.artifact == invite.artifact_id
        && terminal(&cache.tracker.stage) && cache.pending.is_empty()
}

/// A node explicitly rejected the Lobby creation before it entered its pool.
/// The saved invitation therefore cannot represent an uncertain broadcast and
/// must not reserve its original funding coin on a later startup.
pub fn discard_rejected_creation(invitation: &Path, state: &Path, network: &str, player: &Keypair, payer: &Keypair) -> Result<()> {
    forget(invitation, network, &pubkey(player))?;
    forget_state_keys(state, network, vec![pubkey(player), pubkey(payer)])?;
    for path in [invitation.to_path_buf(), invitation.with_file_name("creation-authorization.json")] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn forget(invitation: &Path, network: &str, player: &[u8]) -> Result<()> {
    let path = registry_path(network, player);
    let _lock = super::lock(&path)?;
    let mut registry = load_registry(&path)?;
    let invitation = invitation.canonicalize()?;
    registry.records.retain(|record| record.invitation != invitation);
    super::save(&path, &registry)
}
fn registry_path(network: &str, player: &[u8]) -> PathBuf {
    PathBuf::from("games").join(format!("hosted-{network}-{}.json", secp256k1::XOnlyPublicKey::from_slice(player).expect("validated player")))
}
fn state_registry_path(network: &str, key: &[u8]) -> PathBuf {
    PathBuf::from("games").join(format!("pending-{network}-{}.json", secp256k1::XOnlyPublicKey::from_slice(key).expect("validated signer")))
}
/// Register public journal locations before signing, including custom guest
/// directories. Index both keys so a shared fee key protects every session.
pub fn remember_state(path: &Path, network: &str, player: &Keypair, payer: &Keypair) -> Result<()> {
    remember_state_keys(path, network, vec![pubkey(player), pubkey(payer)])
}
fn remember_state_keys(path: &Path, network: &str, mut keys: Vec<Vec<u8>>) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new(".")).canonicalize()?;
    let state = parent.join(path.file_name().ok_or("state path has no filename")?);
    keys.sort(); keys.dedup();
    for key in keys {
        let registry = state_registry_path(network, &key);
        let _lock = super::lock(&registry)?;
        let mut paths: Vec<PathBuf> = if registry.exists() { super::load(&registry)? } else { vec![] };
        if !paths.contains(&state) { paths.push(state.clone()); super::save(&registry, &paths)?; }
    }
    Ok(())
}

fn forget_state_keys(path: &Path, network: &str, mut keys: Vec<Vec<u8>>) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new(".")).canonicalize()?;
    let state = parent.join(path.file_name().ok_or("state path has no filename")?);
    keys.sort(); keys.dedup();
    for key in keys {
        let registry = state_registry_path(network, &key);
        if !registry.exists() { continue; }
        let _lock = super::lock(&registry)?;
        let mut paths: Vec<PathBuf> = super::load(&registry)?;
        let before = paths.len();
        paths.retain(|known| known != &state);
        if paths.len() != before { super::save(&registry, &paths)?; }
    }
    Ok(())
}
fn terminal(stage: &Stage) -> bool { matches!(stage, Stage::Reclaimed(_) | Stage::Cancelled) }

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use kaspa_consensus_core::{Hash, tx::UtxoEntry};
    use kaspa_txscript::script_builder::ScriptBuilder;
    use secp256k1::{Secp256k1, SecretKey};
    #[test]
    fn abandonment_bypasses_expiry_and_survives_registry_serialization() {
        let mut record = Record { invitation: "synthetic/invitation.json".into(), created_ms: 100, cancel_requested: false };
        assert!(!record.due(101));
        record.cancel_requested = true;
        let restored: Record = serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        assert!(restored.due(101));
    }
    #[test]
    fn obsolete_invitation_is_filtered_before_missing_fields_are_parsed() {
        let a: Artifact = serde_json::from_str(include_str!("../../artifacts/artifact.json")).unwrap();
        let path = Path::new("old-game/invitation.json");
        let mut value = serde_json::json!({"version": GAME_VERSION - 1, "network": "devnet", "artifact_id": "old"});
        assert!(decode_current_invitation(value.clone(), path, &a, "devnet").unwrap().is_none());
        value["version"] = GAME_VERSION.into();
        assert!(decode_current_invitation(value.clone(), path, &a, "devnet").unwrap().is_none());
        value["artifact_id"] = a.id.to_string().into();
        let error = decode_current_invitation(value, path, &a, "devnet").unwrap_err().to_string();
        assert!(error.contains("old-game/invitation.json"), "malformed CURRENT data still fails with its path");
    }
    #[test]
    fn old_cache_with_current_schema_is_filtered_by_artifact_before_game_fields() {
        let a: Artifact = serde_json::from_str(include_str!("../../artifacts/artifact.json")).unwrap();
        let mut value = serde_json::json!({"version": 2, "network": "devnet", "artifact": "old", "tracker": {"stage": {"Lobby": {}}}});
        assert!(!current_cache(&value, &a, "devnet"));
        value["artifact"] = a.id.to_string().into();
        assert!(current_cache(&value, &a, "devnet"));
        assert!(!current_cache(&value, &a, "testnet-10"));
        assert!(serde_json::from_value::<Cache>(value).is_err(), "do not default missing current fields");
    }

    fn key(n: u8) -> Keypair {
        Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
    }
    #[test]
    fn missing_cancellation_record_is_safe_only_for_matching_completed_game() {
        let (_, invite, _, _) = fixture();
        let mut cache = Cache::new(&invite);
        assert!(!completed_invitation(&invite, &cache, "devnet", &pubkey(&key(1))));
        cache.tracker.stage = Stage::Cancelled;
        assert!(completed_invitation(&invite, &cache, "devnet", &pubkey(&key(1))));
        assert!(!completed_invitation(&invite, &cache, "testnet-10", &pubkey(&key(1))));
        assert!(!completed_invitation(&invite, &cache, "devnet", &pubkey(&key(2))));
        cache.game = Hash::from_bytes([42; 32]);
        assert!(!completed_invitation(&invite, &cache, "devnet", &pubkey(&key(1))));
    }
    #[test]
    fn prior_rules_pending_sources_remain_reserved_without_loading_old_game_state() {
        let (a, invite, lobby, mut fee) = fixture();
        fee.outpoint = TransactionOutpoint::new(Hash::from_bytes([241;32]), 0);
        let tx = cancel_transaction(&a, &lobby, &key(3), &fee, &key(3), 100_000).unwrap();
        let mut cache = Cache::new(&invite);
        cache.authorize(tx, vec![lobby.utxo.clone(), fee.utxo.clone()], PURPOSE.into(), &Config::default()).unwrap();
        let mut value = serde_json::to_value(&cache).unwrap();
        value["artifact"] = "prior-rules".into();
        value["tracker"] = serde_json::json!({"unreadable_old_game_schema": true});
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prior-state.json");
        super::super::save(&path, &value).unwrap();
        reserve_prior_pending(&path, &value, &Config::default(), &key(1), &key(3)).unwrap();
        assert!(reserved().unwrap().contains(&fee.outpoint));
        value["network"] = "testnet-10".into();
        reserve_prior_pending(&path, &value, &Config::default(), &key(1), &key(3)).unwrap();
        assert!(reserved().unwrap().contains(&fee.outpoint), "unrelated data never releases an unknown spend");
        reserve(&path, std::iter::empty()).unwrap();
    }
    #[test]
    fn prior_pending_null_or_absent_is_empty_but_malformed_records_fail() {
        let path = Path::new("synthetic-null-pending-state.json");
        let mut value = serde_json::json!({"network": Config::default().network});
        reserve_prior_pending(path, &value, &Config::default(), &key(1), &key(3)).unwrap();
        value["pending"] = serde_json::Value::Null;
        reserve_prior_pending(path, &value, &Config::default(), &key(1), &key(3)).unwrap();
        value["pending"] = serde_json::json!({"malformed": true});
        assert!(reserve_prior_pending(path, &value, &Config::default(), &key(1), &key(3)).is_err());
    }
    fn fixture() -> (Artifact, Invitation, Cell<Lobby>, Funding) {
        let a: Artifact = serde_json::from_str(include_str!("../../artifacts/artifact.json")).unwrap();
        let lobby = Lobby { left: pubkey(&key(1)), right: pubkey(&key(2)), refund: pubkey(&key(3)),
            control_value: SIDE_DEPOSIT as i64, ball_tick_daa: DEFAULT_BALL_TICK_DAA, stake_per_player: DEFAULT_STAKE, ball_count: 1, win_score: 3 };
        let funding = Funding { outpoint: TransactionOutpoint::new(Hash::from_bytes([90; 32]), 0),
            utxo: UtxoEntry::new(2_000_000_000, p2pk(&pubkey(&key(3))), 1, false, None) };
        let invite = super::super::invitations::create(&a, "devnet", Hash::from_bytes([0; 32]), &lobby, &funding, &key(1), &key(3), 100_000).unwrap();
        let cell = accepted_cell(&invite.genesis, 0, 100, lobby);
        let fee = Funding { outpoint: TransactionOutpoint::new(Hash::from_bytes([91; 32]), 0),
            utxo: UtxoEntry::new(100_000_000, p2pk(&pubkey(&key(3))), 1, false, None) };
        (a, invite, cell, fee)
    }

    #[tokio::test]
    async fn same_opponent_invitation_can_be_created_while_unknown_refund_is_stalled() {
        let (a, _, template, mut fee) = fixture();
        let coin = |n| Funding { outpoint: TransactionOutpoint::new(Hash::from_bytes([n;32]), 0),
            utxo: UtxoEntry::new(2_000_000_000, p2pk(&pubkey(&key(3))), 1, false, None) };
        let old = super::super::invitations::create(&a, "devnet", Hash::from_bytes([231;32]), &template.state,
            &coin(231), &key(1), &key(3), 100_000).unwrap();
        let lobby = accepted_cell(&old.genesis, 0, 100, template.state.clone());
        fee.outpoint = coin(232).outpoint;
        let cancel = cancel_transaction(&a, &lobby, &key(3), &fee, &key(3), 100_000).unwrap();
        let mut cache = Cache::new(&old);
        bootstrap_lobby(&mut cache, &old, Hash::from_bytes([234;32]), &lobby, 200).unwrap();
        cache.authorize(cancel, vec![lobby.utxo.clone(), fee.utxo.clone()], PURPOSE.into(), &Config::default()).unwrap();
        cache.pending[0].attempted_ms = 1;
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("state.json");
        super::super::save(&journal, &cache).unwrap();
        reserve_cache(&journal, &cache).unwrap();
        let stalled = tokio::spawn(async { std::future::pending::<()>().await });
        let started = std::time::Instant::now();
        let (coins, claim) = super::super::reservations::available(vec![fee.clone(), coin(233)], false).unwrap();
        assert_eq!(coins[0].outpoint, coin(233).outpoint);
        let new = super::super::invitations::create(&a, "devnet", Hash::from_bytes([235;32]), &template.state,
            &coins[0], &key(1), &key(3), 100_000).unwrap();
        assert_eq!(old.lobby.right, new.lobby.right);
        assert_ne!(old.genesis.id(), new.genesis.id());
        assert!(!stalled.is_finished());
        super::super::rpc::startup::phase("synthetic_same_opponent_invitation_during_stalled_refund", started);
        let restored: Cache = super::super::load(&journal).unwrap();
        restored.validate(&a, &old).unwrap();
        assert_eq!(restored.pending[0].transaction, cache.pending[0].transaction);
        assert!(super::super::reservations::Claim::inputs(vec![fee.outpoint]).is_err(), "unknown submit retains exact fee input");
        drop(claim);
        stalled.abort(); let _ = stalled.await;
        reserve(&journal, std::iter::empty()).unwrap();
    }

    #[test]
    fn saved_cancel_requires_exact_abi_tag_metadata_and_fee_policy() {
        let (a, _, lobby, fee) = fixture();
        let tx = cancel_transaction(&a, &lobby, &key(3), &fee, &key(3), 100_000).unwrap();
        let inputs = vec![lobby.utxo.clone(), fee.utxo.clone()];
        validate_cancel(&a, &lobby, &tx, &inputs, 100_000).unwrap();
        assert!(validate_cancel(&a, &lobby, &tx, &inputs, 99_999).is_err());
        let mut pushes = super::super::tracker::pushes(&tx.inputs[0].signature_script).unwrap();
        let tag = pushes.len() - 2;
        pushes[tag] = a.sil_abi.contract("Lobby").unwrap().entry("start").unwrap().dispatch_tag.as_bytes().to_vec();
        let mut builder = ScriptBuilder::with_flags(kaspa_txscript::EngineFlags { covenants_enabled: true, ..Default::default() });
        for value in pushes { builder.add_data(&value).unwrap(); }
        let mut wrong_entry = tx.clone();
        wrong_entry.inputs[0].signature_script = builder.drain();
        assert!(validate_cancel(&a, &lobby, &wrong_entry, &inputs, 100_000).unwrap_err().to_string().contains("not Lobby.cancel"));
        for tamper in 0..4 {
            let mut changed = inputs.clone();
            match tamper {
                0 => changed[0].block_daa_score += 1,
                1 => changed[0].is_coinbase = true,
                2 => changed[1].covenant_id = lobby.utxo.covenant_id,
                _ => changed[1].amount += 1,
            }
            assert!(validate_cancel(&a, &lobby, &tx, &changed, 100_000).is_err(), "tamper {tamper}");
        }
    }

    #[test]
    fn durable_journal_snapshots_preserve_unknown_submits_and_other_games() {
        let (a, invite, lobby, fee) = fixture();
        let tx = cancel_transaction(&a, &lobby, &key(3), &fee, &key(3), 100_000).unwrap();
        let mut cache = Cache::new(&invite);
        let fresh = cache.authorize_unsubmitted(tx, vec![lobby.utxo.clone(), fee.utxo.clone()], PURPOSE.into(), &Config::default()).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.json");
        let second = directory.path().join("second.json");
        super::super::save(&first, &cache).unwrap();
        let restored: Cache = super::super::load(&first).unwrap();
        restored.validate(&a, &invite).unwrap();
        assert_eq!(restored.pending[0].attempted_ms, 0);
        assert_eq!(pending_reservations(&restored), vec![lobby.outpoint, fee.outpoint]);
        reserve_cache(&first, &restored).unwrap();
        super::super::save(&second, &cache).unwrap();
        reserve_cache(&second, &cache).unwrap();
        let mut ambiguous = restored;
        ambiguous.pending[0].attempted_ms = 1;
        ambiguous.pending[0].branch_changed = true;
        assert_eq!(pending_reservations(&ambiguous), vec![lobby.outpoint, fee.outpoint]);
        cache.begin_withdrawal(&fresh).unwrap();
        // In-memory withdrawal is not published before its marker is durable.
        assert!(reserved().unwrap().contains(&fee.outpoint));
        super::super::save(&first, &cache).unwrap();
        let mut withdrawn: Cache = super::super::load(&first).unwrap();
        withdrawn.recover_withdrawals().unwrap();
        super::super::save(&first, &withdrawn).unwrap();
        reserve_cache(&first, &withdrawn).unwrap();
        assert!(reserved().unwrap().contains(&fee.outpoint), "other journal still owns the input");
        assert!(reserve(&directory.path().join("missing.json"), std::iter::empty()).is_err());
        assert!(reserved().unwrap().contains(&fee.outpoint), "failed snapshot must not release inputs");
        reserve(&second, std::iter::empty()).unwrap();
        assert!(!reserved().unwrap().contains(&fee.outpoint));
        assert_eq!(withdrawn.authorized_fees, 0);
    }

    #[test]
    fn cold_bootstrap_seeds_creation_fee_without_resetting_existing_history() {
        let (_, invite, lobby, _) = fixture();
        let mut cache = Cache::new(&invite);
        let cursor = Hash::from_bytes([5; 32]);
        bootstrap_lobby(&mut cache, &invite, cursor, &lobby, 200).unwrap();
        assert_eq!(cache.fee_ledger.payments[&invite.genesis.id()].sompi, 100_000);
        assert_eq!(cache.tracker.cursor, cursor);
        cache.tracker.stage = Stage::Cancelled;
        cache.tracker.reorgs = 3;
        bootstrap_lobby(&mut cache, &invite, Hash::from_bytes([6; 32]), &lobby, 300).unwrap();
        assert!(matches!(cache.tracker.stage, Stage::Cancelled));
        assert_eq!(cache.tracker.cursor, cursor);
        assert_eq!(cache.tracker.reorgs, 3);
        assert_eq!(cache.fee_ledger.payments.len(), 1);
    }
    #[test]
    fn confirmed_open_lobby_skips_old_empty_history_but_not_pending_authorizations() {
        let (a, invite, lobby, fee) = fixture();
        let mut cache = Cache::new(&invite);
        bootstrap_lobby(&mut cache, &invite, Hash::from_bytes([5;32]), &lobby, 200).unwrap();
        let empty = |n| super::super::tracker::ChainBlock { hash: Hash::from_bytes([n;32]), daa: 210, transactions: vec![] };
        cache.tracker.apply(&a, &invite, &[], &[empty(6)]).unwrap();
        cache.tracker.reorgs = 3;
        bootstrap_lobby(&mut cache, &invite, Hash::from_bytes([7;32]), &lobby, 220).unwrap();
        assert!(cache.tracker.history.is_empty());
        assert_eq!(cache.tracker.cursor, Hash::from_bytes([7;32]));
        assert_eq!(cache.tracker.reorgs, 3);
        let tx = cancel_transaction(&a, &lobby, &key(3), &fee, &key(3), 100_000).unwrap();
        cache.authorize(tx, vec![lobby.utxo.clone(), fee.utxo], PURPOSE.into(), &Config::default()).unwrap();
        cache.tracker.apply(&a, &invite, &[], &[empty(8)]).unwrap();
        bootstrap_lobby(&mut cache, &invite, Hash::from_bytes([9;32]), &lobby, 230).unwrap();
        assert_eq!(cache.tracker.cursor, Hash::from_bytes([8;32]));
        assert_eq!(cache.tracker.history.len(), 1);
        assert_eq!(cache.pending.len(), 1);
    }
}
fn load_registry(path: &Path) -> Result<Registry> {
    if path.exists() { super::load(path) } else { Ok(Registry::default()) }
}
fn current_invitation(path: &Path, a: &Artifact, network: &str) -> Result<Option<Invitation>> {
    let value: serde_json::Value = super::load(path).map_err(|e| format!("{}: {e}", path.display()))?;
    decode_current_invitation(value, path, a, network)
}
fn decode_current_invitation(value: serde_json::Value, path: &Path, a: &Artifact, network: &str) -> Result<Option<Invitation>> {
    // Inspect the envelope BEFORE deserializing version-specific game fields.
    // Old public records are not migrated, interpreted, overwritten or deleted.
    if value["version"] != GAME_VERSION || value["network"] != network || value["artifact_id"] != a.id.to_string() {
        return Ok(None);
    }
    serde_json::from_value(value).map(Some).map_err(|e| format!("{}: {e}", path.display()).into())
}
fn current_cache(value: &serde_json::Value, a: &Artifact, network: &str) -> bool {
    value["version"] == 2 && value["network"] == network && value["artifact"] == a.id.to_string()
}
/// Rules-independent safety only: do not replay/migrate an old game, but never
/// reuse an owned source of a possibly exposed transaction from its journal.
fn reserve_prior_pending(path: &Path, value: &serde_json::Value, c: &Config, player: &Keypair, payer: &Keypair) -> Result<()> {
    if value["network"] != c.network { return Ok(()); }
    let Some(pending) = value.get("pending") else { return Ok(()); };
    // Older journals used an optional pending record: null means no exposed
    // transaction, not a malformed nonempty reservation list.
    if pending.is_null() { return Ok(()); }
    let pending: Vec<super::client::Pending> = serde_json::from_value(pending.clone())
        .map_err(|e| format!("{}: cannot inspect pending source reservations: {e}", path.display()))?;
    let mut inputs = Vec::new();
    for p in pending {
        if p.inputs.iter().any(|u| u.covenant_id.is_none() &&
            (u.script_public_key == p2pk(&pubkey(player)) || u.script_public_key == p2pk(&pubkey(payer)))) {
            inputs.extend(p.transaction.inputs.iter().map(|i| i.previous_outpoint));
        }
    }
    reserve(path, inputs.into_iter())
}
pub fn remember(invitation: &Path, invite: &Invitation, created_ms: u64) -> Result<()> {
    let path = registry_path(&invite.network, &invite.lobby.left);
    let _lock = super::lock(&path)?;
    let mut registry = load_registry(&path)?;
    let invitation = invitation.canonicalize()?;
    if !registry.records.iter().any(|r| r.invitation == invitation) {
        registry.records.push(Record { invitation: invitation.clone(), created_ms, cancel_requested: false });
        super::save(&path, &registry)?;
    }
    reserve(&invitation, invite.genesis.inputs.iter().map(|i| i.previous_outpoint))?;
    remember_state_keys(&invitation.with_file_name("left-state.json"), &invite.network,
        vec![invite.lobby.left.clone(), invite.lobby.refund.clone()])?;
    Ok(())
}
pub fn expired(created_ms: u64, now: u64) -> bool {
    now.checked_sub(created_ms).is_some_and(|age| age >= EXPIRY_MS)
}
// Kept for the entire signing session, also excluding another game with the
// same fee key. Read-only CLI commands never acquire these locks.
pub fn signer_locks(network: &str, player: &Keypair, payer: &Keypair) -> Result<Vec<std::fs::File>> {
    let mut keys = vec![pubkey(player), pubkey(payer)]; keys.sort(); keys.dedup();
    keys.iter().map(|key| super::lock(&PathBuf::from("games").join(format!("signer-{network}-{}.json", secp256k1::XOnlyPublicKey::from_slice(key)?))))
        .collect()
}
/// Pending cancellation fee inputs remain reserved after ambiguous submission,
/// including while other games are played. No predicted fee change is used.
pub fn reserved() -> Result<Vec<TransactionOutpoint>> {
    super::reservations::reserved()
}
// Each journal owns its snapshot. Replace only after reading a durable journal
// or successfully saving reconciliation, never on an RPC absence/error.
fn reserve(path: &Path, inputs: impl Iterator<Item = TransactionOutpoint>) -> Result<()> {
    // Existing records must remain readable; a missing record cannot release
    // an earlier durable reservation.
    path.canonicalize()?;
    super::reservations::restore(path, inputs.collect())
}
fn reserve_cache(path: &Path, cache: &Cache) -> Result<()> {
    path.canonicalize()?;
    super::reservations::persisted(path, cache)
}
#[cfg(test)]
fn pending_reservations(cache: &Cache) -> Vec<TransactionOutpoint> {
    cache.pending.iter().filter(|p| !cache.withdrawals.contains(&p.transaction.id()))
        .flat_map(|p| p.transaction.inputs.iter().map(|i| i.previous_outpoint)).collect()
}
fn bootstrap_lobby(cache: &mut Cache, invite: &Invitation, cursor: kaspa_consensus_core::Hash, lobby: &Cell<Lobby>, daa: u64) -> Result<()> {
    // The caller has twice verified this exact original Lobby is still unspent
    // on our node. With no pending authorization or observed game transition,
    // old empty global-chain history cannot affect the refund. Retain the new
    // pre-snapshot cursor so subsequent Full validation still has no gap.
    let open = cache.pending.is_empty()
        && matches!(&cache.tracker.stage, Stage::Lobby(old) if old.outpoint == lobby.outpoint && super::tracker::same_utxo(&old.utxo, &lobby.utxo))
        && cache.tracker.history.iter().all(|undo| undo.transactions.iter().all(|id| *id == invite.genesis.id()));
    if (matches!(cache.tracker.stage, Stage::AwaitingLobby) && cache.tracker.history.is_empty()) || open {
        if open { cache.tracker.history.clear(); }
        cache.tracker.cursor = cursor;
        cache.tracker.stage = Stage::Lobby(lobby.clone());
        cache.tracker.daa = daa;
        cache.fee_ledger.payments.entry(invite.genesis.id()).or_insert(super::invitations::creation_payment(invite)?);
    }
    Ok(())
}
pub fn cancel_transaction(a: &Artifact, lobby: &Cell<Lobby>, refund: &Keypair, funding: &Funding, fee_key: &Keypair, fee: u64) -> Result<Transaction> {
    if pubkey(refund) != lobby.state.refund { return Err("wallet does not own the committed refund key".into()); }
    if funding.outpoint == lobby.outpoint || funding.utxo.covenant_id.is_some() { return Err("cancellation needs an ordinary external fee coin".into()); }
    let b = TxBuilder::new(a)?;
    let ctx = TxContext::new().actor_input(
        "Lobby", lobby.state.state(), EntryCall::new("cancel").args_with(|tx, i| argent_runtime::args![sign(tx, i, refund)]),
        lobby.outpoint, lobby.utxo.clone(), 0,
    ).output(p2pk(&lobby.state.refund), None, lobby.utxo.amount);
    let tx = build(&b, &funded(ctx, funding, fee_key, fee)?)?;
    let inputs = vec![lobby.utxo.clone(), funding.utxo.clone()];
    validate_cancel(a, lobby, &tx, &inputs, fee)?;
    Ok(tx)
}
fn validate_cancel(a: &Artifact, lobby: &Cell<Lobby>, tx: &Transaction, inputs: &[kaspa_consensus_core::tx::UtxoEntry], max_fee: u64) -> Result<()> {
    if tx.inputs.len() != 2 || inputs.len() != 2 || tx.inputs[0].previous_outpoint != lobby.outpoint
        || tx.inputs[1].previous_outpoint == lobby.outpoint
        || !super::tracker::same_utxo(&inputs[0], &lobby.utxo) || inputs[1].covenant_id.is_some()
        || tx.outputs.len() > 2
        || tx.outputs.first().is_none_or(|o| o.value != lobby.utxo.amount || o.script_public_key != p2pk(&lobby.state.refund) || o.covenant.is_some()) {
        return Err("saved recovery transaction is not an exact full-principal Lobby refund".into());
    }
    let pushes = super::tracker::pushes(&tx.inputs[0].signature_script)?;
    let tag = a.sil_abi.contract("Lobby").and_then(|c| c.entry("cancel")).ok_or("missing Lobby cancel entry")?;
    if pushes.len() < 2 || pushes[pushes.len() - 2].as_slice() != tag.dispatch_tag.as_bytes() {
        return Err("saved recovery authorization is not Lobby.cancel".into());
    }
    let fee = super::client::transaction_fee(tx, inputs)?;
    if fee == 0 || fee > max_fee { return Err("saved cancellation exceeds the current fee cap".into()); }
    let change = inputs[1].amount.checked_sub(fee).ok_or("cancellation fee exceeds external funding")?;
    if tx.outputs.len() != if change == 0 { 1 } else { 2 }
        || tx.outputs.get(1).is_some_and(|o| o.value != change || o.script_public_key != inputs[1].script_public_key || o.covenant.is_some()) {
        return Err("saved cancellation changes the external fee coin destination".into());
    }
    for i in 0..inputs.len() { argent_runtime::execute_input_with_covenants(tx, inputs.to_vec(), i)?; }
    Ok(())
}
/// Standalone recovery entry; the interactive client uses the lifetime pipeline.
pub async fn recover(a: &Artifact, c: &Config, player: &Keypair, payer: &Keypair) -> Result<Vec<String>> {
    super::consolidation::restore(c, player)?;
    if pubkey(player) != pubkey(payer) { super::consolidation::restore(c, payer)?; }
    let due = recovery_records(a, c, player, payer, true)?;
    if due.is_empty() { return Ok(vec![]); }
    let n = Node::connect(c).await?;
    let result = recover_records(a, c, player, payer, &n, due).await;
    let _ = n.disconnect().await;
    result
}
/// Reuse the menu's qualified connection; its owner controls disconnect/retry.
pub async fn recover_with_node(a: &Artifact, c: &Config, player: &Keypair, payer: &Keypair, n: &Node) -> Result<Vec<String>> {
    let due = recovery_records(a, c, player, payer, false)?;
    recover_records(a, c, player, payer, n, due).await
}
/// Local preparation also restores reservations when no invitation is expired.
/// Call under both signer locks before enabling any wallet funding path.
pub fn needs_recovery(a: &Artifact, c: &Config, player: &Keypair, payer: &Keypair) -> Result<bool> {
    let started = std::time::Instant::now();
    progress("recovery_restore_begin", "loading pending input reservations", started);
    super::consolidation::restore(c, player)?;
    if pubkey(player) != pubkey(payer) { super::consolidation::restore(c, payer)?; }
    let due = !recovery_records(a, c, player, payer, true)?.is_empty();
    super::rpc::startup::phase("recovery_restore_complete", started);
    Ok(due)
}
fn recovery_records(a: &Artifact, c: &Config, player: &Keypair, payer: &Keypair, restore: bool) -> Result<Vec<Record>> {
    // Recovery reads only paths explicitly registered while a game was active.
    // Historical directories under `games/` are never walked.
    let registry = load_registry(&registry_path(&c.network, &pubkey(player)))?;
    // Capture old authorizations once, not on every movement preparation.
    // Never spend a coin merely because its earlier submit outcome is unknown.
    let mut current_records = Vec::new();
    for mut record in registry.records {
        let Some(invite) = current_invitation(&record.invitation, a, &c.network)? else { continue; };
        invite.validate(a)?;
        if invite.network != c.network || invite.lobby.left != pubkey(player) { return Err("host registry identity mismatch".into()); }
        let state = record.invitation.with_file_name("left-state.json");
        if state.is_file() {
            let value: serde_json::Value = super::load(&state)?;
            if current_cache(&value, a, &c.network) {
                let cache: Cache = serde_json::from_value(value).map_err(|e| format!("{}: {e}", state.display()))?;
                if terminal(&cache.tracker.stage) && cache.pending.is_empty() {
                    reserve(&state, std::iter::empty())?;
                    forget_state_keys(&state, &c.network, vec![invite.lobby.left.clone(), invite.lobby.refund.clone()])?;
                    forget(&record.invitation, &c.network, &pubkey(player))?;
                    continue;
                }
            }
        }
        reserve(&record.invitation, invite.genesis.inputs.iter().map(|i| i.previous_outpoint))?;
        // A new client session never resumes a prior hosted match. Recover an
        // abandoned Lobby immediately; joined matches remain uncancellable.
        if restore && !record.cancel_requested {
            request_cancel(&record.invitation, &c.network, &pubkey(player))?;
            record.cancel_requested = true;
        }
        current_records.push(record);
    }
    let mut state_paths = Vec::new();
    for key in [pubkey(player), pubkey(payer)] {
        let index = state_registry_path(&c.network, &key);
        if index.exists() {
            let paths: Vec<PathBuf> = super::load(&index)?;
            state_paths.extend(paths);
        }
    }
    state_paths.sort(); state_paths.dedup();
    for path in state_paths.into_iter().filter(|_| restore) {
            // A host can share its fee key with another selected player. Its
            // creation may be exposed before a state cache exists, so also
            // recover the public creation reservation from indexed directories.
            let invitation_path = path.with_file_name("invitation.json");
            if invitation_path.is_file() {
                if let Some(invite) = current_invitation(&invitation_path, a, &c.network)? {
                    invite.validate(a)?;
                    let payment = super::invitations::creation_payment(&invite)?;
                    if payment.payer == p2pk(&pubkey(player)) || payment.payer == p2pk(&pubkey(payer)) {
                        reserve(&invitation_path, invite.genesis.inputs.iter().map(|i| i.previous_outpoint))?;
                    }
                } else {
                    let value: serde_json::Value = super::load(&invitation_path)?;
                    if value["network"] == c.network &&
                        (value["lobby"]["left"] == serde_json::to_value(pubkey(player))? || value["lobby"]["refund"] == serde_json::to_value(pubkey(payer))?) {
                        let tx: Transaction = serde_json::from_value(value["genesis"].clone())?;
                        reserve(&invitation_path, tx.inputs.iter().map(|i| i.previous_outpoint))?;
                    }
                }
            }
            if !path.is_file() { continue; }
            let value: serde_json::Value = super::load(&path)?;
            if !current_cache(&value, a, &c.network) {
                reserve_prior_pending(&path, &value, c, player, payer)?;
                continue;
            }
            let cache: Cache = serde_json::from_value(value).map_err(|e| format!("{}: {e}", path.display()))?;
            if terminal(&cache.tracker.stage) && cache.pending.is_empty() {
                reserve(&path, std::iter::empty())?;
                forget_state_keys(&path, &c.network, vec![pubkey(player), pubkey(payer)])?;
                continue;
            }
            if cache.pending.is_empty() || cache.pending.iter().any(|pending| pending.inputs.iter().any(|u| u.covenant_id.is_none() &&
                (u.script_public_key == p2pk(&pubkey(player)) || u.script_public_key == p2pk(&pubkey(payer))))) {
                reserve_cache(&path, &cache)?;
            }
    }
    Ok(current_records.into_iter().filter(|r| r.due(now_ms())).collect())
}
async fn recover_records(a: &Artifact, c: &Config, player: &Keypair, payer: &Keypair, n: &Node, due: Vec<Record>) -> Result<Vec<String>> {
        let mut messages = Vec::new();
        for record in due {
            let started = std::time::Instant::now();
            let invite: Invitation = super::load(&record.invitation)?;
            invite.validate(a)?;
            if invite.network != c.network || invite.lobby.left != pubkey(player) || invite.lobby.refund != pubkey(payer) {
                return Err("host recovery registry does not belong to the selected player/refund key".into());
            }
            let path = record.invitation.with_file_name("left-state.json");
            let _game_lock = match super::lock(&path) {
                Ok(lock) => lock,
                Err(_) => { progress("recovery_busy", "deferred: invitation is open in this client", started); continue; }
            };
            let mut cache: Cache = if path.exists() { super::load(&path)? } else { Cache::new(&invite) };
            cache.validate(a, &invite)?;
            cache.recover_withdrawals()?;
            let output = &invite.genesis.outputs[0];
            progress("recovery_lobby_lookup", "checking invitation on chain", started);
            let coins = n.utxos(&[output.script_public_key.clone()]).await?;
            super::rpc::startup::phase("recovery_lobby_lookup_complete", started);
            let outpoint = TransactionOutpoint::new(invite.genesis.id(), 0);
            if !coins.iter().any(|f| f.outpoint == outpoint) {
                // Absence alone does not prove refund acceptance. Let the
                // existing independently validated replay reconcile a pending
                // cancel, guest join, or reorg. Never sign against absence.
                if cache.pending.iter().any(|p| p.purpose == PURPOSE) {
                    progress("recovery_reconcile", "reconciling pending refund", started);
                    cache.sync(a, &invite, &n, c).await?;
                    super::save(&path, &cache)?;
                    reserve_cache(&path, &cache)?;
                    messages.push(format!("Invitation {}: {}", invite.genesis.id(), if matches!(cache.tracker.stage, Stage::Cancelled) { "refund accepted" } else { "join/refund reconciliation retained" }));
                    progress("recovery_reconciled", if matches!(cache.tracker.stage, Stage::Cancelled) { "refund accepted" } else { "refund/join reconciliation pending" }, started);
                } else {
                    progress("recovery_no_lobby", if matches!(cache.tracker.stage, Stage::Cancelled) { "refund accepted" } else { "no accepted unspent Lobby; deferred" }, started);
                }
                continue;
            }
            let lobby = super::invitations::accepted_lobby(a, &invite, &coins)?;
            if !cache.pending.is_empty() && cache.pending.iter().any(|p| p.purpose != PURPOSE) {
                messages.push(format!("Invitation {}: another pending action; recovery deferred", invite.genesis.id()));
                continue;
            }
            // Cold recovery is safe only against the exact accepted creation
            // output. Capture the tip before refreshing and replay from it.
            let (cursor, fresh) = n.invitation_lobby(a, &invite).await?;
            if !super::tracker::same_utxo(&fresh.utxo, &lobby.utxo) { return Err("Lobby acceptance changed during recovery".into()); }
            bootstrap_lobby(&mut cache, &invite, cursor, &fresh, n.info().await?.virtual_daa_score)?;
            progress("recovery_validate", "validating invitation history", started);
            let coherent = cache.sync(a, &invite, &n, c).await?.0;
            super::rpc::startup::phase("recovery_validation_complete", started);
            super::save(&path, &cache)?;
            reserve_cache(&path, &cache)?;
            if !coherent || !matches!(cache.tracker.stage, Stage::Lobby(ref current) if current.outpoint == fresh.outpoint && super::tracker::same_utxo(&current.utxo, &fresh.utxo)) { continue; }
            let mut _authorization_claim = None;
            if cache.pending.is_empty() {
                progress("recovery_prepare", "preparing refund", started);
                let (funding, mut claim) = wallet::reserved_fee_funding(&n, player, payer, c.fee_sompi.checked_add(1).ok_or("fee overflow")?, c.coinbase_maturity_daa, &[]).await?;
                claim.restrict(vec![fresh.outpoint, funding.outpoint])?;
                let fee_key = wallet::funding_key(&funding, player, payer)?;
                let tx = priced_transaction(c, |fee| cancel_transaction(a, &fresh, payer, &funding, fee_key, fee))?;
                cache.authorize(tx, vec![fresh.utxo.clone(), funding.utxo], PURPOSE.into(), c)?;
                _authorization_claim = Some(claim);
            }
            cache.validate(a, &invite)?;
            if cache.authorized_fees > c.fee_budget_sompi { return Err("saved recovery authorization exceeds the current fee budget".into()); }
            let pending = cache.pending.first_mut().ok_or("missing cancellation authorization")?;
            validate_cancel(a, &fresh, &pending.transaction, &pending.inputs, c.fee_sompi)?;
            wallet::funding_key(&Funding { outpoint: pending.transaction.inputs[1].previous_outpoint, utxo: pending.inputs[1].clone() }, player, payer)?;
            let sources = n.utxos(&pending.inputs.iter().map(|u| u.script_public_key.clone()).collect::<Vec<_>>()).await?;
            if !pending.transaction.inputs.iter().zip(&pending.inputs).all(|(input, u)| sources.iter().any(|f| f.outpoint == input.previous_outpoint && super::tracker::same_utxo(u, &f.utxo))) {
                return Err("pending cancellation inputs changed; reservations retained for reconciliation".into());
            }
            pending.attempted_ms = now_ms();
            let tx = pending.transaction.clone();
            // Required pre-submit journal; includes the fee reservation and
            // exact signed transaction. Unknown outcomes retry this same tx.
            progress("recovery_persist", "saving refund authorization", started);
            super::save(&path, &cache)?;
            reserve_cache(&path, &cache)?;
            progress("recovery_submit_request", "refund pending", started);
            let submitted = std::time::Instant::now();
            n.submit(&tx).await?;
            progress("recovery_submit_ack", "refund pending acceptance", submitted);
            messages.push(format!("Invitation {}: cancellation submitted; awaiting acceptance", invite.genesis.id()));
        }
        Ok(messages)
}
