//! Local host-only launch construction from authenticated accepted invitations.
use super::super::engine::*;
use super::tracker::{Invitation, redeem};
use argent_runtime::{Artifact, ArtifactValue, TxBuilder};
use kaspa_consensus_core::{
    constants::TX_VERSION_TOCCATA,
    hashing::{sighash::{SigHashReusedValuesUnsync, calc_schnorr_signature_hash},
        sighash_type::{SigHashType, SIG_HASH_ALL}},
    tx::{CovenantBinding, MutableTransaction, Transaction, UtxoEntry},
};
use kaspa_txscript::{pay_to_script_hash_signature_script_with_flags, script_builder::ScriptBuilder};
use secp256k1::{Keypair, Message, Secp256k1, XOnlyPublicKey, schnorr::Signature};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartPackage {
    pub version: u32,
    pub game: kaspa_consensus_core::Hash,
    pub lobby: Cell<Lobby>,
    /// An unfunded local draft is never broadcastable or exchanged.
    pub funding: Option<Funding>,
    pub fee: u64,
    pub transaction: Transaction,
    pub right_signature: Vec<u8>,
    pub fee_signature: Vec<u8>,
}
impl StartPackage {
    pub fn inputs(&self) -> Vec<UtxoEntry> {
        std::iter::once(self.lobby.utxo.clone()).chain(self.funding.iter().map(|f| f.utxo.clone())).collect()
    }
    pub fn complete(&self) -> bool {
        self.funding.is_some() && !self.right_signature.is_empty() && !self.fee_signature.is_empty()
    }
    pub fn enqueue(&self, cache: &mut super::client::Cache, player: &[u8], config: &super::Config) -> Result<bool> {
        let super::tracker::Stage::Lobby(lobby) = &cache.tracker.stage else { return Ok(false) };
        if !cache.pending.is_empty() { return Ok(false); }
        if player != self.lobby.state.right || !self.complete() || self.lobby.outpoint != lobby.outpoint
            || !super::tracker::same_utxo(&self.lobby.utxo, &lobby.utxo) {
            return Err("signed launch does not match the accepted lobby/player; create a fresh match".into());
        }
        cache.authorize(self.transaction.clone(), self.inputs(), "host start".into(), config)?;
        Ok(true)
    }
}

fn unsigned(b: &TxBuilder<'_>, lobby: &Cell<Lobby>, funding: Option<&Funding>, fee: u64) -> Result<Transaction> {
    let id = lobby.utxo.covenant_id.ok_or("lobby has no covenant")?;
    let plan = lobby.state.opening();
    let control = u64::try_from(lobby.state.control_value)?;
    let stake = stake_amount(lobby.state.stake_per_player)?;
    if lobby.utxo.amount != stake { return Err("Lobby must contain exactly the joining player's stake".into()); }
    let court = stake.checked_mul(2).and_then(|v| v.checked_sub(2 * control)).filter(|v| *v > 0).ok_or("invalid pot allocation")?;
    let mut outputs = Vec::new();
    for (actor, state, value) in std::iter::once(("Court", plan.world.state(), court))
        .chain(plan.queues.iter().map(|q| (q.actor(), q.state(), control / QUEUES_PER_PLAYER as u64))) {
        let mut output = b.genesis_output(actor, state, value)?;
        output.covenant = Some(CovenantBinding::new(0, id));
        outputs.push(output);
    }
    let mut inputs = vec![TxBuilder::transaction_input(lobby.outpoint, vec![])];
    if let Some(funding) = funding {
        if funding.utxo.covenant_id.is_some() || funding.outpoint == lobby.outpoint {
            return Err("host funding must be an independent ordinary coin".into());
        }
        let spend = stake.checked_add(fee).ok_or("stake and fee overflow")?;
        let change = funding.utxo.amount.checked_sub(spend).ok_or("host coin cannot cover its stake and external fee")?;
        if change > 0 {
            outputs.push(kaspa_consensus_core::tx::TransactionOutput::new(change, funding.utxo.script_public_key.clone()));
        }
        inputs.push(TxBuilder::transaction_input(funding.outpoint, vec![]));
    } else if fee != 0 {
        return Err("an unfunded proposal cannot declare a paid fee".into());
    }
    Ok(Transaction::new(TX_VERSION_TOCCATA, inputs, outputs, 0, Default::default(), 0, vec![]))
}
fn mutable(p: &StartPackage) -> MutableTransaction<Transaction> {
    let mut m = MutableTransaction::new(p.transaction.clone());
    m.entries = p.inputs().into_iter().map(Some).collect();
    m
}
fn witness(a: &Artifact, b: &TxBuilder<'_>, p: &mut StartPackage) -> Result<()> {
    let mut args = vec![
        ArtifactValue::Bytes(if p.right_signature.is_empty() { vec![0; 65] } else { p.right_signature.clone() }),
    ];
    for actor in ["Player", "Court"] {
        let contract = a.sil_abi.contract(actor).ok_or("launch actor missing")?;
        let (prefix, _, suffix) = contract.compiled.script_parts(&contract.compiled.bytecode).ok_or("invalid template spans")?;
        args.push(ArtifactValue::Bytes(prefix.to_vec()));
        args.push(ArtifactValue::Bytes(suffix.to_vec()));
    }
    let encoded = argent::artifact::encode_contract_entry_sig_script(&a.sil_abi, "Lobby", "start", &args)?;
    p.transaction.inputs[0].signature_script = pay_to_script_hash_signature_script_with_flags(
        redeem(b, a, "Lobby", p.lobby.state.state())?, encoded, argent_runtime::covenant_engine_flags(),
    )?;
    if p.funding.is_some() {
        p.transaction.inputs[1].signature_script = ScriptBuilder::new()
            .add_data(if p.fee_signature.is_empty() { &[0; 65] } else { &p.fee_signature })?.drain();
    }
    Ok(())
}

/// Reconstruct the agreed transition locally. No host signature is requested.
pub fn prepare(a: &Artifact, invite: &Invitation, lobby: &Cell<Lobby>) -> Result<StartPackage> {
    invite.validate(a)?;
    let b = TxBuilder::new(a)?;
    let mut p = StartPackage {
        version: GAME_VERSION, game: invite.genesis.id(), lobby: lobby.clone(), funding: None, fee: 0,
        transaction: unsigned(&b, lobby, None, 0)?, right_signature: vec![], fee_signature: vec![],
    };
    witness(a, &b, &mut p)?;
    validate(a, invite, &p, u64::MAX)?;
    Ok(p)
}

fn verify_signature(p: &StartPackage, input: usize, signature: &[u8], public: &[u8], hash_type: SigHashType) -> Result<()> {
    if signature.len() != 65 || signature[64] != hash_type.to_u8() {
        return Err("launch signature has an unauthorized sighash flag".into());
    }
    let hash = calc_schnorr_signature_hash(&mutable(p).as_verifiable(), input, hash_type, &SigHashReusedValuesUnsync::new());
    Secp256k1::verification_only().verify_schnorr(&Signature::from_slice(&signature[..64])?,
        &Message::from_digest_slice(hash.as_bytes().as_slice())?, &XOnlyPublicKey::from_slice(public)?)?;
    Ok(())
}
pub fn validate(a: &Artifact, invite: &Invitation, p: &StartPackage, max_fee: u64) -> Result<()> {
    invite.validate(a)?;
    if p.version != GAME_VERSION || p.game != invite.genesis.id() || p.lobby.outpoint.transaction_id != p.game
        || p.lobby.outpoint.index != 0 || p.lobby.state.state() != invite.lobby.state() {
        return Err("invalid launch package identity or lobby".into());
    }
    match &p.funding {
        None if p.fee != 0 || !p.right_signature.is_empty() || !p.fee_signature.is_empty() => {
            return Err("unfunded proposal contains completion fields".into());
        }
        Some(_) if p.fee == 0 || p.fee > max_fee || !p.complete() => {
            return Err("funded launch requires both host signatures and a bounded external fee".into());
        }
        _ => {}
    }
    let b = TxBuilder::new(a)?;
    let expected = b.covenant_utxo("Lobby", p.lobby.state.state(), p.lobby.utxo.amount,
        p.lobby.utxo.block_daa_score, false, p.lobby.utxo.covenant_id)?;
    if !super::tracker::same_utxo(&expected, &p.lobby.utxo) || p.lobby.utxo.amount != invite.genesis.outputs[0].value
        || p.lobby.utxo.covenant_id != invite.genesis.outputs[0].covenant.map(|c| c.covenant_id) {
        return Err("invalid launch source".into());
    }
    let mut canonical = unsigned(&b, &p.lobby, p.funding.as_ref(), p.fee)?;
    let mut actual = p.transaction.clone();
    for input in &mut actual.inputs {
        input.signature_script.clear();
        input.compute_commit = kaspa_consensus_core::mass::ComputeBudget(0).into();
    }
    actual.set_storage_mass(0); actual.finalize(); canonical.finalize();
    if actual != canonical { return Err("launch changes agreed inputs, outputs, stakes, change or fees".into()); }
    let mut encoded = p.clone();
    witness(a, &b, &mut encoded)?;
    if encoded.transaction.inputs.iter().map(|i| &i.signature_script).ne(p.transaction.inputs.iter().map(|i| &i.signature_script)) {
        return Err("launch witness differs from its signatures".into());
    }
    if p.funding.is_some() {
        validate_storage(p)?;
        verify_signature(p, 0, &p.right_signature, &invite.lobby.right, SIG_HASH_ALL)?;
        argent_runtime::execute_input_with_covenants(&p.transaction, p.inputs(), 1)?;
        argent_runtime::execute_input_with_covenants(&p.transaction, p.inputs(), 0)?;
        if super::client::transaction_fee(&p.transaction, &p.inputs())? != p.fee {
            return Err("launch fee differs from external funding remainder".into());
        }
    } else if p.transaction.storage_mass() != 0 {
        return Err("an unfunded proposal cannot claim a storage-mass commitment".into());
    }
    Ok(())
}
/// A proposal has no actual guest input and therefore no mineability claim.
/// The completed transaction must pass exact contextual storage validation.
pub fn validate_storage(p: &StartPackage) -> Result<()> {
    if p.funding.is_none() {
        if p.transaction.storage_mass() != 0 { return Err("unfunded proposal has committed mass".into()); }
        let control = u64::try_from(p.lobby.state.control_value)?;
        let stake = stake_amount(p.lobby.state.stake_per_player)?;
        let court = p.lobby.utxo.amount.checked_add(stake).and_then(|v| v.checked_sub(2 * control)).ok_or("invalid proposal principal")?;
        if p.transaction.inputs.len() != 1 || p.transaction.inputs[0].previous_outpoint != p.lobby.outpoint
            || p.transaction.outputs.len() != 1 + QUEUE_COUNT || p.transaction.outputs[0].value != court
            || p.transaction.outputs[1..].iter().any(|o| o.value != control / QUEUES_PER_PLAYER as u64) {
            return Err("unfunded proposal changes the promised game deposits".into());
        }
        return Ok(());
    }
    let mass = storage_mass(&p.transaction, p.inputs())?;
    let limit = kaspa_consensus_core::config::params::MAINNET_PARAMS.block_mass_limits().after().storage;
    if mass > limit || p.transaction.storage_mass() != mass {
        return Err(format!("launch storage mass {mass} is invalid or exceeds limit {limit}; choose different host funding").into());
    }
    Ok(())
}

/// Host chooses a real owned coin before signing the complete transaction.
/// Relay fee converges against its full witnesses; principal is never a fee.
pub fn approve(a: &Artifact, invite: &Invitation, proposal: StartPackage, candidates: Vec<Funding>,
    right: &Keypair, payer: &Keypair, config: &super::Config) -> Result<StartPackage> {
    validate(a, invite, &proposal, config.fee_sompi)?;
    if proposal.funding.is_some() || pubkey(right) != invite.lobby.right {
        return Err("only the invited guest may fund an open proposal".into());
    }
    let b = TxBuilder::new(a)?;
    let stake = stake_amount(invite.lobby.stake_per_player)?;
    let script = p2pk(&pubkey(payer));
    let mut best: Option<(u64, StartPackage)> = None;
    for funding in candidates {
        if funding.utxo.covenant_id.is_some() || funding.utxo.script_public_key != script || funding.utxo.amount <= stake { continue; }
        let mut p = proposal.clone();
        p.funding = Some(funding);
        let mut fee = 1;
        let mut stabilized = false;
        for _ in 0..3 {
            p.fee = fee;
            let Ok(tx) = unsigned(&b, &p.lobby, p.funding.as_ref(), fee) else { break };
            p.transaction = tx;
            p.right_signature = sign(&mutable(&p), 0, right);
            p.fee_signature = sign(&mutable(&p), 1, payer);
            witness(a, &b, &mut p)?;
            let entries = p.inputs();
            // Compute budgets are assigned by the VM and affect relay mass.
            // Only real guest-owned drafts are measured; none is submitted.
            argent_runtime::execute_transaction_with_covenants(&mut p.transaction, entries)?;
            let required = super::client::relay_fee(&p.transaction, config)?;
            if required == fee { stabilized = true; break; }
            fee = required;
        }
        if !stabilized { continue; }
        let mass = storage_mass(&p.transaction, p.inputs())?;
        let limit = kaspa_consensus_core::config::params::MAINNET_PARAMS.block_mass_limits().after().storage;
        if mass > limit { continue; }
        if best.as_ref().is_none_or(|(old, _)| mass < *old) { best = Some((mass, p)); }
    }
    let (_, p) = best.ok_or("no host coin can fund its 5 KAS stake, external fee and valid storage mass")?;
    let entries = p.inputs();
    commit_storage_mass(&p.transaction, entries)?;
    validate(a, invite, &p, config.fee_sompi)?;
    Ok(p)
}
