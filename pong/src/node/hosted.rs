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
static RESERVED: Mutex<Vec<(PathBuf, Vec<TransactionOutpoint>)>> = Mutex::new(Vec::new());
#[derive(Default, Serialize, Deserialize)]
struct Registry { records: Vec<Record> }
#[derive(Serialize, Deserialize)]
struct Record { invitation: PathBuf, created_ms: u64 }
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

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use kaspa_consensus_core::{Hash, tx::UtxoEntry};
    use kaspa_txscript::script_builder::ScriptBuilder;
    use secp256k1::{Secp256k1, SecretKey};
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
    fn fixture() -> (Artifact, Invitation, Cell<Lobby>, Funding) {
        let a: Artifact = serde_json::from_str(include_str!("../../artifacts/artifact.json")).unwrap();
        let lobby = Lobby { left: pubkey(&key(1)), right: pubkey(&key(2)), refund: pubkey(&key(3)),
            control_value: SIDE_DEPOSIT as i64, ball_tick_daa: DEFAULT_BALL_TICK_DAA, stake_per_player: DEFAULT_STAKE };
        let funding = Funding { outpoint: TransactionOutpoint::new(Hash::from_bytes([90; 32]), 0),
            utxo: UtxoEntry::new(2_000_000_000, p2pk(&pubkey(&key(3))), 1, false, None) };
        let invite = super::super::invitations::create(&a, "devnet", Hash::from_bytes([0; 32]), &lobby, &funding, &key(1), &key(3), 100_000).unwrap();
        let cell = accepted_cell(&invite.genesis, 0, 100, lobby);
        let fee = Funding { outpoint: TransactionOutpoint::new(Hash::from_bytes([91; 32]), 0),
            utxo: UtxoEntry::new(100_000_000, p2pk(&pubkey(&key(3))), 1, false, None) };
        (a, invite, cell, fee)
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
pub fn remember(invitation: &Path, invite: &Invitation, created_ms: u64) -> Result<()> {
    let path = registry_path(&invite.network, &invite.lobby.left);
    let _lock = super::lock(&path)?;
    let mut registry = load_registry(&path)?;
    let invitation = invitation.canonicalize()?;
    if !registry.records.iter().any(|r| r.invitation == invitation) {
        registry.records.push(Record { invitation: invitation.clone(), created_ms });
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
    Ok(RESERVED.lock().map_err(|_| "recovery reservation lock poisoned")?
        .iter().flat_map(|(_, inputs)| inputs.iter().copied()).collect())
}
// Each journal owns its snapshot. Replace only after reading a durable journal
// or successfully saving reconciliation, never on an RPC absence/error.
fn reserve(path: &Path, inputs: impl Iterator<Item = TransactionOutpoint>) -> Result<()> {
    let path = path.canonicalize()?;
    let mut reserved = RESERVED.lock().map_err(|_| "recovery reservation lock poisoned")?;
    let inputs = inputs.collect();
    if let Some((_, old)) = reserved.iter_mut().find(|(p, _)| *p == path) { *old = inputs; }
    else { reserved.push((path, inputs)); }
    Ok(())
}
fn reserve_cache(path: &Path, cache: &Cache) -> Result<()> {
    reserve(path, pending_reservations(cache).into_iter())
}
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
/// Startup/reconnection only; no timer or fee/signing work on the gameplay path.
pub async fn recover(a: &Artifact, c: &Config, player: &Keypair, payer: &Keypair) -> Result<Vec<String>> {
    let due = recovery_records(a, c, player, payer)?;
    if due.is_empty() { return Ok(vec![]); }
    let n = Node::connect(c).await?;
    let result = recover_records(a, c, player, payer, &n, due).await;
    let _ = n.rpc.disconnect().await;
    result
}
/// Reuse the menu's qualified connection; its owner controls disconnect/retry.
pub async fn recover_with_node(a: &Artifact, c: &Config, player: &Keypair, payer: &Keypair, n: &Node) -> Result<Vec<String>> {
    let due = recovery_records(a, c, player, payer)?;
    recover_records(a, c, player, payer, n, due).await
}
/// Local preparation also restores reservations when no invitation is expired.
/// Call under both signer locks before enabling any wallet funding path.
pub fn needs_recovery(a: &Artifact, c: &Config, player: &Keypair, payer: &Keypair) -> Result<bool> {
    Ok(!recovery_records(a, c, player, payer)?.is_empty())
}
fn recovery_records(a: &Artifact, c: &Config, player: &Keypair, payer: &Keypair) -> Result<Vec<Record>> {
    // Discover existing public host records as well as newly registered custom
    // directories. No wallet files or arbitrary JSON files are scanned.
    if Path::new("games").is_dir() {
        for entry in std::fs::read_dir("games")? {
            let path = entry?.path().join("invitation.json");
            if !path.is_file() { continue; }
            let Some(invite) = current_invitation(&path, a, &c.network)? else { continue; };
            if invite.network != c.network || invite.lobby.left != pubkey(player) || invite.version != GAME_VERSION || invite.artifact_id != a.id.to_string() { continue; }
            invite.validate(a)?;
            let created = std::fs::metadata(&path)?.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_millis().min(u64::MAX as u128) as u64;
            remember(&path, &invite, created)?;
        }
    }
    let registry = load_registry(&registry_path(&c.network, &pubkey(player)))?;
    // Capture old authorizations once, not on every movement preparation.
    // Never spend a coin merely because its earlier submit outcome is unknown.
    let mut directories: Vec<PathBuf> = registry.records.iter().filter_map(|r| r.invitation.parent().map(Path::to_path_buf)).collect();
    if Path::new("games").is_dir() {
        for entry in std::fs::read_dir("games")? {
            let path = entry?.path();
            if path.is_dir() && !directories.contains(&path) { directories.push(path); }
        }
    }
    let mut current_records = Vec::new();
    for record in registry.records {
        let Some(invite) = current_invitation(&record.invitation, a, &c.network)? else { continue; };
        invite.validate(a)?;
        if invite.network != c.network || invite.lobby.left != pubkey(player) { return Err("host registry identity mismatch".into()); }
        reserve(&record.invitation, invite.genesis.inputs.iter().map(|i| i.previous_outpoint))?;
        current_records.push(record);
    }
    let mut state_paths: Vec<_> = directories.iter().flat_map(|d| [d.join("left-state.json"), d.join("right-state.json")]).collect();
    for key in [pubkey(player), pubkey(payer)] {
        let index = state_registry_path(&c.network, &key);
        if index.exists() {
            let paths: Vec<PathBuf> = super::load(&index)?;
            state_paths.extend(paths);
        }
    }
    state_paths.sort(); state_paths.dedup();
    for path in state_paths {
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
                }
            }
            if !path.is_file() { continue; }
            let value: serde_json::Value = super::load(&path)?;
            if !current_cache(&value, a, &c.network) { continue; }
            let cache: Cache = serde_json::from_value(value).map_err(|e| format!("{}: {e}", path.display()))?;
            if cache.pending.is_empty() || cache.pending.iter().any(|pending| pending.inputs.iter().any(|u| u.covenant_id.is_none() &&
                (u.script_public_key == p2pk(&pubkey(player)) || u.script_public_key == p2pk(&pubkey(payer))))) {
                reserve_cache(&path, &cache)?;
            }
    }
    Ok(current_records.into_iter().filter(|r| expired(r.created_ms, now_ms())).collect())
}
async fn recover_records(a: &Artifact, c: &Config, player: &Keypair, payer: &Keypair, n: &Node, due: Vec<Record>) -> Result<Vec<String>> {
        let mut messages = Vec::new();
        for record in due {
            let invite: Invitation = super::load(&record.invitation)?;
            invite.validate(a)?;
            if invite.network != c.network || invite.lobby.left != pubkey(player) || invite.lobby.refund != pubkey(payer) {
                return Err("host recovery registry does not belong to the selected player/refund key".into());
            }
            let path = record.invitation.with_file_name("left-state.json");
            let _game_lock = super::lock(&path)?;
            let mut cache: Cache = if path.exists() { super::load(&path)? } else { Cache::new(&invite) };
            cache.validate(a, &invite)?;
            cache.recover_withdrawals()?;
            let output = &invite.genesis.outputs[0];
            let coins = n.utxos(&[output.script_public_key.clone()]).await?;
            let outpoint = TransactionOutpoint::new(invite.genesis.id(), 0);
            if !coins.iter().any(|f| f.outpoint == outpoint) {
                // Absence alone does not prove refund acceptance. Let the
                // existing independently validated replay reconcile a pending
                // cancel, guest join, or reorg. Never sign against absence.
                if cache.pending.iter().any(|p| p.purpose == PURPOSE) {
                    cache.sync(a, &invite, &n, c).await?;
                    super::save(&path, &cache)?;
                    reserve_cache(&path, &cache)?;
                    messages.push(format!("Invitation {}: {}", invite.genesis.id(), if matches!(cache.tracker.stage, Stage::Cancelled) { "refund accepted" } else { "join/refund reconciliation retained" }));
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
            let coherent = cache.sync(a, &invite, &n, c).await?.0;
            super::save(&path, &cache)?;
            reserve_cache(&path, &cache)?;
            if !coherent || !matches!(cache.tracker.stage, Stage::Lobby(ref current) if current.outpoint == fresh.outpoint && super::tracker::same_utxo(&current.utxo, &fresh.utxo)) { continue; }
            if cache.pending.is_empty() {
                let funding = wallet::fee_funding(&n, player, payer, c.fee_sompi.checked_add(1).ok_or("fee overflow")?, c.coinbase_maturity_daa, &[]).await?;
                let fee_key = wallet::funding_key(&funding, player, payer)?;
                let tx = priced_transaction(c, |fee| cancel_transaction(a, &fresh, payer, &funding, fee_key, fee))?;
                cache.authorize(tx, vec![fresh.utxo.clone(), funding.utxo], PURPOSE.into(), c)?;
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
            super::save(&path, &cache)?;
            reserve_cache(&path, &cache)?;
            n.submit(&tx).await?;
            messages.push(format!("Invitation {}: cancellation submitted; awaiting acceptance", invite.genesis.id()));
        }
        Ok(messages)
}
