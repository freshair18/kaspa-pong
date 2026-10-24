//! Accepted-chain observer. Mempool submissions never enter this state.
use super::super::engine::*;
use super::audit::{TransactionRecord, TransactionStatus};
use argent_runtime::{Artifact, TxBuilder, materialize_redeem_script};
use kaspa_consensus_core::{
    Hash,
    tx::{Transaction, TransactionOutpoint, UtxoEntry},
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invitation {
    pub version: u32,
    pub network: String,
    pub artifact_id: String,
    pub anchor: Hash,
    pub genesis: Transaction,
    pub lobby: Lobby,
}
impl Invitation {
    pub fn validate(&self, artifact: &Artifact) -> Result<()> {
        if self.version != GAME_VERSION {
            return Err("unsupported game version; start a new match with the current client".into());
        }
        if self.artifact_id != artifact.id.to_string() {
            return Err("invitation version/artifact mismatch".into());
        }
        if self.artifact_id != GAME_ARTIFACT_ID {
            return Err("this artifact needs a client implementing its game rules; it is not the current Pong artifact".into());
        }
        if self.lobby.left.len() != 32
            || self.lobby.right.len() != 32
            || self.lobby.refund.len() != 32
            || self.lobby.left == self.lobby.right
            || self.lobby.ball_tick_daa != DEFAULT_BALL_TICK_DAA
        {
            return Err("invalid player/refund keys".into());
        }
        for key in [&self.lobby.left, &self.lobby.right, &self.lobby.refund] {
            secp256k1::XOnlyPublicKey::from_slice(key)?;
        }
        if self.lobby.stake_per_player != DEFAULT_STAKE {
            return Err("this client requires a 5 KAS stake per player".into());
        }
        let builder = TxBuilder::new(artifact)?;
        let control = u64::try_from(self.lobby.control_value)?;
        let stake = stake_amount(self.lobby.stake_per_player)?;
        let value = stake;
        if control != SIDE_DEPOSIT {
            return Err("this client allocates 3 KAS per side's queues from the pot".into());
        }
        let output = self.genesis.outputs.first().ok_or("missing lobby output")?;
        let expected = builder.genesis_output("Lobby", self.lobby.state(), value)?;
        if output.value != value || output.script_public_key != expected.script_public_key || output.covenant.is_none() {
            return Err("invitation does not commit the advertised lobby".into());
        }
        super::invitations::authenticate(artifact, self)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Stage {
    AwaitingLobby,
    Lobby(Cell<Lobby>),
    Live(Snapshot),
    Collecting(Collecting),
    Reclaimed(Finish),
    Cancelled,
}
impl Stage {
    pub fn cells(&self) -> Vec<(TransactionOutpoint, UtxoEntry)> {
        match self {
            Self::Lobby(c) => vec![(c.outpoint, c.utxo.clone())],
            Self::Live(s) => std::iter::once((s.court.outpoint, s.court.utxo.clone()))
                .chain(s.players.iter().map(|p| (p.outpoint, p.utxo.clone())))
                .collect(),
            Self::Collecting(s) => std::iter::once((s.collector.outpoint, s.collector.utxo.clone()))
                .chain(s.players.iter().map(|p| (p.outpoint, p.utxo.clone())))
                .collect(),
            _ => vec![],
        }
    }
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Undo {
    pub hash: Hash,
    pub parent: Hash,
    pub before: Option<Stage>,
    pub transactions: Vec<Hash>,
    #[serde(default)]
    pub daa: Option<u64>,
    #[serde(default)]
    pub events: Vec<super::audit::GameEvent>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tracker {
    pub cursor: Hash,
    pub daa: u64,
    pub stage: Stage,
    pub history: VecDeque<Undo>,
    pub reorgs: u64,
}
impl Tracker {
    pub fn new(invite: &Invitation) -> Self {
        Self { cursor: invite.anchor, daa: 0, stage: Stage::AwaitingLobby, history: VecDeque::new(), reorgs: 0 }
    }
    pub fn accepted(&self, txid: Hash) -> bool {
        self.history.iter().any(|h| h.transactions.contains(&txid))
    }
    /// A cold guest's root was authenticated against its own node's UTXO set,
    /// not created at its observation cursor. A tip-only rollback must retain
    /// that root while replay checks its spends and current-node consistency.
    pub fn rewind_empty_history(&mut self, invite: &Invitation, common: Hash) {
        self.cursor = common;
        if !matches!(&self.stage, Stage::Lobby(c) if c.outpoint == TransactionOutpoint::new(invite.genesis.id(),0)) {
            self.stage = Stage::AwaitingLobby;
        }
        self.reorgs += 1;
    }
    /// Caller publishes this candidate only after the entire RPC batch succeeds.
    pub fn apply(&mut self, artifact: &Artifact, invite: &Invitation, removed: &[Hash], added: &[ChainBlock]) -> Result<()> {
        self.apply_with_events(artifact, invite, removed, added).map(|_| ())
    }
    pub fn apply_with_events(
        &mut self,
        artifact: &Artifact,
        invite: &Invitation,
        removed: &[Hash],
        added: &[ChainBlock],
    ) -> Result<Vec<TransactionRecord>> {
        let mut candidate = self.clone();
        let mut observed = Vec::new();
        if !removed.is_empty() {
            candidate.reorgs += 1;
        }
        for hash in removed {
            let undo = candidate.history.pop_back().ok_or("reorganization exceeds retained history; replay required")?;
            if undo.hash != *hash || candidate.cursor != *hash {
                return Err("incoherent removed-chain order; replay required".into());
            }
            for txid in &undo.transactions {
                let event = undo.events.iter().find(|e| e.txid == *txid).cloned().unwrap_or_else(|| super::audit::GameEvent {
                    payment: None,
                    txid: *txid,
                    accepted_daa: undo.daa,
                    action: "unknown".into(),
                    side: None,
                    direction: None,
                });
                observed.push(TransactionRecord::new(invite, event, Some(undo.hash), candidate.reorgs, TransactionStatus::Reverted));
            }
            if let Some(before) = undo.before {
                candidate.stage = before;
            }
            candidate.cursor = undo.parent;
        }
        let builder = TxBuilder::new(artifact)?;
        for block in added {
            if block.hash == candidate.cursor || candidate.history.iter().any(|h| h.hash == block.hash) {
                return Err("duplicate added-chain block".into());
            }
            let before = candidate.stage.clone();
            let mut transactions = Vec::new();
            let mut events = Vec::new();
            for accepted in &block.transactions {
                let source_stage = candidate.stage.clone();
                if candidate.observe(artifact, &builder, invite, &accepted.tx, &accepted.inputs, block.daa)? {
                    transactions.push(accepted.tx.id());
                    let mut event = super::audit::GameEvent::between(&source_stage, &candidate.stage, accepted.tx.id(), block.daa);
                    event.payment = super::fees::Payment::from_transaction(&accepted.tx, &accepted.inputs)?;
                    observed.push(TransactionRecord::new(
                        invite,
                        event.clone(),
                        Some(block.hash),
                        candidate.reorgs,
                        TransactionStatus::Accepted,
                    ));
                    events.push(event);
                }
            }
            candidate.history.push_back(Undo {
                hash: block.hash,
                parent: candidate.cursor,
                before: if transactions.is_empty() { None } else { Some(before) },
                transactions,
                daa: Some(block.daa),
                events,
            });
            candidate.cursor = block.hash;
            candidate.daa = block.daa;
            if candidate.history.len() > 2048 {
                candidate.history.pop_front();
            }
        }
        // On a removal-only batch the caller supplies the new node DAA before
        // projection. Never retain the old branch's speculative clock.
        if added.is_empty() && !removed.is_empty() {
            candidate.daa = 0;
        }
        *self = candidate;
        Ok(observed)
    }
    fn observe(
        &mut self,
        artifact: &Artifact,
        b: &TxBuilder<'_>,
        invite: &Invitation,
        tx: &Transaction,
        entries: &[UtxoEntry],
        daa: u64,
    ) -> Result<bool> {
        if matches!(self.stage, Stage::AwaitingLobby | Stage::Lobby(_)) && tx.id() == invite.genesis.id() {
            let id = tx.outputs.first().and_then(|o| o.covenant).ok_or("accepted lobby lacks covenant")?.covenant_id;
            self.stage = Stage::Lobby(find_cell(
                b,
                tx,
                daa,
                "Lobby",
                invite.lobby.clone(),
                invite.lobby.state(),
                tx.outputs[0].value,
                id,
                None,
            )?);
            return Ok(true);
        }
        if matches!(self.stage, Stage::AwaitingLobby) { return Ok(false); }
        let sources = self.stage.cells();
        if !tx.inputs.iter().any(|i| sources.iter().any(|(o, _)| *o == i.previous_outpoint)) {
            return Ok(false);
        }
        if entries.len() != tx.inputs.len() {
            return Err("accepted transaction lacks complete source UTXOs".into());
        }
        for (outpoint, entry) in &sources {
            if let Some(i) = tx.inputs.iter().position(|i| i.previous_outpoint == *outpoint) {
                if !same_utxo(&entries[i], entry) {
                    return Err("accepted source metadata differs from replayed UTXO".into());
                }
                argent_runtime::execute_input_with_covenants(tx, entries.to_vec(), i)?;
            }
        }
        match self.stage.clone() {
            Stage::Lobby(lobby) => {
                let i = source_index(tx, lobby.outpoint)?;
                let call = call(b, artifact, "Lobby", lobby.state.state(), &tx.inputs[i].signature_script)?;
                if is_entry(artifact, "Lobby", "start", &call)? {
                    let plan = lobby.state.opening();
                    let id = lobby.utxo.covenant_id.ok_or("missing lobby covenant")?;
                    let control = u64::try_from(lobby.state.control_value)?;
                    let court_value = lobby.utxo.amount.checked_add(lobby.state.stake_per_player as u64).and_then(|v| v.checked_sub(2 * control)).ok_or("invalid split")?;
                    self.stage = Stage::Live(Snapshot {
                        court: find_cell(b, tx, daa, "Court", plan.world.clone(), plan.world.state(), court_value, id, Some(i))?,
                        players: plan
                            .queues
                            .iter()
                            .map(|q| {
                                find_cell(b, tx, daa, q.actor(), q.clone(), q.state(), control / QUEUES_PER_PLAYER as u64, id, Some(i))
                            })
                            .collect::<Result<Vec<_>>>()?
                            .try_into()
                            .map_err(|_| "wrong queue count")?,
                    });
                } else if is_entry(artifact, "Lobby", "cancel", &call)? {
                    self.stage = Stage::Cancelled;
                } else {
                    return Err("unknown lobby transition".into());
                }
            }
            Stage::Live(mut s) => {
                let id = s.court.utxo.covenant_id.ok_or("missing game covenant")?;
                if let Some(i) = tx.inputs.iter().position(|i| i.previous_outpoint == s.court.outpoint) {
                    let args = call(b, artifact, "Court", s.court.state.state(), &tx.inputs[i].signature_script)?;
                    if is_entry(artifact, "Court", "resolve", &args)? {
                        let side = usize::try_from(script_int(args.first().ok_or("missing paddle side")?)?)?;
                        let until = script_int(args.get(1).ok_or("missing resolve deadline")?)?;
                        let plan = checkpoint_plan(&s, side, until)?;
                        let authorizer = i;
                        let court = find_cell(
                            b,
                            tx,
                            daa,
                            "Court",
                            plan.world.clone(),
                            plan.world.state(),
                            s.court.utxo.amount,
                            id,
                            Some(authorizer),
                        )?;
                        for index in 0..QUEUE_COUNT {
                            if tx.inputs.iter().any(|input| input.previous_outpoint == s.players[index].outpoint) {
                                let q = &plan.queues[index];
                                s.players[index] = find_cell(
                                    b,
                                    tx,
                                    daa,
                                    q.actor(),
                                    q.clone(),
                                    q.state(),
                                    s.players[index].utxo.amount,
                                    id,
                                    Some(authorizer),
                                )?;
                            }
                        }
                        s.court = court;
                        self.stage = Stage::Live(s);
                    } else if is_entry(artifact, "Court", "settle", &args)?
                        || is_entry(artifact, "Court", "timeout", &args)? {
                        let w = &s.court.state;
                        let finish = if is_entry(artifact, "Court", "timeout", &args)? { timeout_result(&s)? } else { w.result() };
                        let collection = Collection { left: w.left.clone(), right: w.right.clone(), finish };
                        let collector = find_cell(b, tx, daa, "Collector", collection.clone(), collection.state(), s.court.utxo.amount, id, Some(i))?;
                        self.stage = Stage::Collecting(Collecting { collector, players: s.players });
                    } else {
                        return Err("unknown court transition".into());
                    }
                } else {
                    for index in 0..QUEUE_COUNT {
                        let player = &s.players[index];
                        if let Some(i) = tx.inputs.iter().position(|i| i.previous_outpoint == player.outpoint) {
                            let args = call(b, artifact, player.state.actor(), player.state.state(), &tx.inputs[i].signature_script)?;
                            if !is_entry(artifact, player.state.actor(), "command", &args)? {
                                return Err("lane spent without command or Court".into());
                            }
                            let direction = script_int(args.first().ok_or("missing movement steps")?)?;
                            let queue = player.state.push(direction, player.utxo.block_daa_score)?;
                            s.players[index] =
                                find_cell(b, tx, daa, queue.actor(), queue.clone(), queue.state(), player.utxo.amount, id, Some(i))?;
                        }
                    }
                    self.stage = Stage::Live(s);
                }
            }
            Stage::Collecting(mut s) => {
                let c = &s.collector;
                if let Some(i) = tx.inputs.iter().position(|input| input.previous_outpoint == c.outpoint) {
                    let args = call(b, artifact, "Collector", c.state.state(), &tx.inputs[i].signature_script)?;
                    if !is_entry(artifact, "Collector", "collect", &args)? {
                        return Err("unknown collector transition".into());
                    }
                    self.stage = Stage::Reclaimed(c.state.finish.clone());
                } else {
                    // A command already in flight can be accepted after Court closes.
                    // Preserve its actual successor so collection does not use a spent lane.
                    let id = c.utxo.covenant_id.ok_or("missing collection covenant")?;
                    for q in &mut s.players {
                        if let Some(i) = tx.inputs.iter().position(|input| input.previous_outpoint == q.outpoint) {
                            let args = call(b, artifact, "Player", q.state.state(), &tx.inputs[i].signature_script)?;
                            if !is_entry(artifact, "Player", "command", &args)? {
                                return Err("lane spent without command or Collector".into());
                            }
                            let direction = script_int(args.first().ok_or("missing movement steps")?)?;
                            let next = q.state.push(direction, q.utxo.block_daa_score)?;
                            *q = find_cell(b, tx, daa, "Player", next.clone(), next.state(), q.utxo.amount, id, Some(i))?;
                        }
                    }
                    self.stage = Stage::Collecting(s);
                }
            }
            _ => return Err("unexpected spent terminal state".into()),
        }
        Ok(true)
    }
}

pub fn same_utxo(a: &UtxoEntry, b: &UtxoEntry) -> bool {
    a.amount == b.amount
        && a.script_public_key == b.script_public_key
        && a.block_daa_score == b.block_daa_score
        && a.is_coinbase == b.is_coinbase
        && a.covenant_id == b.covenant_id
}
fn source_index(tx: &Transaction, o: TransactionOutpoint) -> Result<usize> {
    tx.inputs.iter().position(|i| i.previous_outpoint == o).ok_or_else(|| "missing actor input".into())
}
fn find_cell<T>(
    b: &TxBuilder<'_>,
    tx: &Transaction,
    daa: u64,
    actor: &str,
    state: T,
    encoded: State,
    value: u64,
    id: Hash,
    authorizer: Option<usize>,
) -> Result<Cell<T>> {
    let script = b.covenant_utxo(actor, encoded, value, daa, false, Some(id))?.script_public_key;
    let mut matches = tx.outputs.iter().enumerate().filter(|(_, o)| {
        o.script_public_key == script
            && o.value == value
            && o.covenant.is_some_and(|c| c.covenant_id == id && authorizer.is_none_or(|i| usize::from(c.authorizing_input) == i))
    });
    let (i, _) = matches.next().ok_or_else(|| format!("accepted transaction has no expected {actor} successor"))?;
    if matches.next().is_some() {
        return Err("ambiguous actor successor".into());
    }
    Ok(accepted_cell(tx, i, daa, state))
}
fn is_entry(a: &Artifact, actor: &str, entry: &str, pushes: &[Vec<u8>]) -> Result<bool> {
    let entry = a.sil_abi.contract(actor).and_then(|c| c.entry(entry)).ok_or("entry absent from artifact")?;
    Ok(pushes.last().is_some_and(|p| p.as_slice() == entry.dispatch_tag.as_bytes()))
}
pub fn redeem(b: &TxBuilder<'_>, a: &Artifact, actor: &str, state: State) -> Result<Vec<u8>> {
    let c = a.sil_abi.contract(actor).ok_or("unknown actor")?;
    Ok(materialize_redeem_script(&a.sil_abi, c, &b.materialize_actor_state(actor, state)?)?)
}
fn call(b: &TxBuilder<'_>, a: &Artifact, actor: &str, state: State, script: &[u8]) -> Result<Vec<Vec<u8>>> {
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
