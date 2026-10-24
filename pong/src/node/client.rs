//! Reconcile accepted state while up to three independent input lanes are in flight.
use super::super::engine::*;
use super::{
    Config,
    audit::{ReorgEvent, ReorgLog, TransactionRecord, TransactionStatus, net_transaction_changes},
    rpc::Node,
    tracker::{ChainBlock, Invitation, Stage, Tracker, same_utxo},
};
use argent_runtime::{Artifact, TxBuilder};
use kaspa_consensus_core::{
    Hash,
    config::params::MAINNET_PARAMS,
    mass::MassCalculator,
    tx::{Transaction, UtxoEntry},
};
use secp256k1::Keypair;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pending {
    pub transaction: Transaction,
    pub inputs: Vec<UtxoEntry>,
    pub purpose: String,
    pub attempted_ms: u64,
    pub branch_changed: bool,
}
/// Runtime-only proof: this authorization has never crossed the RPC boundary.
/// It cannot be reconstructed from an attempted timestamp or a saved cache.
#[derive(Debug)]
pub struct Unsubmitted {
    id: Hash,
}
impl Unsubmitted {
    pub fn id(&self) -> Hash { self.id }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cache {
    pub version: u32,
    pub game: Hash,
    pub network: String,
    pub artifact: String,
    pub tracker: Tracker,
    pub pending: Vec<Pending>,
    /// Durable revocations. Keep reservations until the write is acknowledged.
    pub withdrawals: Vec<Hash>,
    #[serde(skip)]
    unsubmitted: Vec<Hash>,
    // Conservative upper bound: charge each newly authorized transaction once,
    // even if its submission outcome is uncertain. Identical retries are free.
    pub authorized_fees: u64,
    pub fee_ledger: super::fees::Ledger,
    pub last_reorg: Option<ReorgEvent>,
    pub game_reorgs: u64,
    /// Durable catch-up evidence, not yet a confirmed game reorganization.
    pub reorg_candidates: Vec<ReorgEvent>,
    pub sync_transactions: Vec<TransactionRecord>,
    /// Durable outbox: retained until the append-only log has synced to disk.
    pub reorg_log_pending: Vec<ReorgEvent>,
    pub transaction_log_pending: Vec<TransactionRecord>,
    pub recent_transactions: VecDeque<TransactionRecord>,
}
impl Cache {
    pub fn new(invite: &Invitation) -> Self {
        Self {
            version: 2,
            game: invite.genesis.id(),
            network: invite.network.clone(),
            artifact: invite.artifact_id.clone(),
            tracker: Tracker::new(invite),
            pending: Vec::new(),
            withdrawals: Vec::new(),
            unsubmitted: Vec::new(),
            authorized_fees: 0,
            fee_ledger: super::fees::Ledger::new(),
            last_reorg: None,
            game_reorgs: 0,
            reorg_candidates: Vec::new(),
            sync_transactions: Vec::new(),
            reorg_log_pending: Vec::new(),
            transaction_log_pending: Vec::new(),
            recent_transactions: VecDeque::new(),
        }
    }
    pub fn validate(&self, a: &Artifact, invite: &Invitation) -> Result<()> {
        if self.fee_ledger.version != 1 {
            return Err("unsupported fee ledger version".into());
        }
        if self.version != 2
            || self.game != invite.genesis.id()
            || self.network != invite.network
            || self.artifact != invite.artifact_id
        {
            return Err("state file belongs to another game, network or artifact".into());
        }
        let b = TxBuilder::new(a)?;
        match &self.tracker.stage {
            Stage::Lobby(c) => check_cell(&b, "Lobby", c.state.state(), &c.utxo)?,
            Stage::Live(s) => {
                check_cell(&b, "Court", s.court.state.state(), &s.court.utxo)?;
                for p in &s.players {
                    check_cell(&b, p.state.actor(), p.state.state(), &p.utxo)?;
                }
                if s.court.state.left != invite.lobby.left
                    || s.court.state.right != invite.lobby.right
                    || s.court.utxo.amount != COURT_VALUE
                    || s.players.iter().any(|p| p.utxo.amount != QUEUE_DEPOSIT)
                    || s.court.state.stake_per_player != invite.lobby.stake_per_player
                {
                    return Err("cached game keys differ from invitation".into());
                }
            }
            Stage::Collecting(s) => {
                check_cell(&b, "Collector", s.collector.state.state(), &s.collector.utxo)?;
                if s.collector.state.left != invite.lobby.left
                    || s.collector.state.right != invite.lobby.right
                    || s.collector.utxo.amount != COURT_VALUE
                    || s.players.iter().any(|p| p.utxo.amount != QUEUE_DEPOSIT)
                    || s.collector.state.finish.stake_per_player != invite.lobby.stake_per_player
                {
                    return Err("cached collection keys differ from invitation".into());
                }
                for q in &s.players {
                    check_cell(&b, "Player", q.state.state(), &q.utxo)?;
                }
            }
            _ => {}
        }
        if self.pending.len() > QUEUES_PER_PLAYER {
            return Err("too many pending input lanes".into());
        }
        let mut reserved = std::collections::HashSet::new();
        for p in &self.pending {
            if p.transaction.inputs.iter().any(|i| !reserved.insert(i.previous_outpoint)) {
                return Err("pending transactions double-spend a reserved input".into());
            }
            if p.transaction.inputs.len() != p.inputs.len() {
                return Err("pending transaction lacks source entries".into());
            }
            for i in 0..p.inputs.len() {
                argent_runtime::execute_input_with_covenants(&p.transaction, p.inputs.clone(), i)?;
            }
        }
        if self.withdrawals.iter().any(|id| !self.pending.iter().any(|p| p.transaction.id() == *id)
            || self.pending.iter().any(|p| p.transaction.inputs.iter().any(|i| i.previous_outpoint.transaction_id == *id))) {
            return Err("invalid withdrawal reservation or fee descendant".into());
        }
        Ok(())
    }
    pub fn authorize_unsubmitted(
        &mut self, tx: Transaction, inputs: Vec<UtxoEntry>, purpose: String, config: &Config,
    ) -> Result<Unsubmitted> {
        let id = tx.id();
        self.authorize(tx, inputs, purpose, config)?;
        self.unsubmitted.push(id);
        Ok(Unsubmitted { id })
    }
    pub fn mark_exposed(&mut self, fresh: Unsubmitted) -> Result<()> {
        if self.withdrawals.contains(&fresh.id) || !self.unsubmitted.contains(&fresh.id) {
            return Err("authorization is withdrawing or no longer fresh".into());
        }
        self.unsubmitted.retain(|id| *id != fresh.id);
        Ok(())
    }
    pub fn begin_withdrawal(&mut self, fresh: &Unsubmitted) -> Result<()> {
        let id = fresh.id;
        if !self.unsubmitted.contains(&id)
            || !self.pending.iter().any(|p| p.transaction.id() == id)
            || self.pending.iter().any(|p| p.transaction.inputs.iter().any(|i| i.previous_outpoint.transaction_id == id))
        {
            return Err("unsubmitted authorization missing or has reserved descendants".into());
        }
        if !self.withdrawals.contains(&id) { self.withdrawals.push(id); }
        Ok(())
    }
    /// Call only after the revocation snapshot is durable (including recovery
    /// from that snapshot). A failed write must retain both marker and inputs.
    pub fn complete_withdrawal(&mut self, id: Hash) -> Result<()> {
        if !self.withdrawals.contains(&id) { return Err("withdrawal is not recorded".into()); }
        let index = self.pending.iter().position(|p| p.transaction.id() == id).ok_or("withdrawal reservation missing")?;
        let p = &self.pending[index];
        let fee = transaction_fee(&p.transaction, &p.inputs)?;
        self.authorized_fees = self.authorized_fees.checked_sub(fee).ok_or("withdrawal fee accounting underflow")?;
        self.pending.remove(index);
        self.withdrawals.retain(|txid| *txid != id);
        self.unsubmitted.retain(|txid| *txid != id);
        Ok(())
    }
    pub fn recover_withdrawals(&mut self) -> Result<()> {
        for id in self.withdrawals.clone() { self.complete_withdrawal(id)?; }
        self.unsubmitted.clear(); // Replayed pending commands have no runtime proof.
        Ok(())
    }
    /// Rebuild chain history without forgetting a possibly propagated spend or
    /// resetting the user's cumulative authorization limit.
    pub fn rescan(&mut self, invite: &Invitation) {
        let reorgs = self.tracker.reorgs;
        self.tracker = Tracker::new(invite);
        self.tracker.reorgs = reorgs;
        self.fee_ledger = super::fees::Ledger::new();
        for p in &mut self.pending {
            p.branch_changed = true;
        }
    }
    pub async fn sync(&mut self, a: &Artifact, invite: &Invitation, node: &Node, config: &Config) -> Result<(bool, bool)> {
        self.sync_inner(a, invite, node, config, None).await
    }
    /// Gameplay uses a bounded fetch-ahead reader. Setup/recovery retain the
    /// same Full replay and independent covenant verification as before.
    pub async fn sync_follow(
        &mut self,
        a: &Artifact,
        invite: &Invitation,
        node: &Node,
        config: &Config,
        feed: &mut super::pipeline::ChainFeed,
    ) -> Result<(bool, bool)> {
        self.sync_inner(a, invite, node, config, Some(feed)).await
    }
    async fn sync_inner(
        &mut self,
        a: &Artifact,
        invite: &Invitation,
        node: &Node,
        config: &Config,
        mut feed: Option<&mut super::pipeline::ChainFeed>,
    ) -> Result<(bool, bool)> {
        let mut working = self.tracker.clone();
        let mut replayed = false;
        let mut fee_restart = None;
        let mut reports = Vec::new();
        let mut transactions = Vec::new();
        for _ in 0..8 {
            let delta = match feed.as_deref_mut() {
                Some(feed) => feed.next(working.cursor).await?,
                None => node.delta(working.cursor).await?,
            };
            if !delta.removed.is_empty() {
                let mut event = ReorgEvent::capture(invite, &working, &delta.removed, &delta.added, delta.common);
                event.pending_local_transaction = self.pending.first().map(|p| p.transaction.id());
                reports.push(event);
            }
            if working.history.is_empty() && delta.removed.contains(&working.cursor) {
                working.rewind_empty_history(invite, delta.common.ok_or("missing reorganization common ancestor")?);
                transactions.extend(replay_events(&mut working, a, invite, &[], &delta.added).await?);
            } else if let Err(error) =
                replay_events(&mut working, a, invite, &delta.removed, &delta.added).await.map(|events| transactions.extend(events))
            {
                if error.to_string().contains("replay required") && !replayed {
                    let count = working.reorgs + 1;
                    // A deep rollback can exceed retained undo data. Preserve
                    // every known removed transaction; unknown details remain
                    // explicit in the accompanying reorg report.
                    if let Some(report) = reports.last() {
                        for event in &report.removed_game_transactions {
                            transactions.push(TransactionRecord::new(invite, event.clone(), None, count, TransactionStatus::Reverted));
                        }
                    }
                    working = Tracker::new(invite);
                    working.reorgs = count;
                    replayed = true;
                    fee_restart = Some(transactions.len());
                    continue;
                }
                return Err(error);
            }
            // Joined results can outlive the other RPC future. Keep their
            // errors Send without changing the public engine error type.
            let (info, coherent) = tokio::try_join!(async { node.info().await.map_err(|e| e.to_string()) }, async {
                node.consistent(&working.stage).await.map_err(|e| e.to_string())
            })?;
            // The 10-BPS global tip need not stand still across RPC round trips.
            // These exact outpoints AND acceptance metadata prove our game
            // actors still exist in the node's current UTXO view. A missed game
            // spend makes this check fail and the next delta catches it up.
            // With no actors (e.g. before genesis), require chain catch-up.
            // Do not enable keys while Full history is still many batches
            // behind: acceptance of a new command would then be slow to see.
            let chain_daa = working.history.back().and_then(|h| h.daa).unwrap_or(working.daa);
            let caught_up = info.virtual_daa_score.saturating_sub(chain_daa) <= 5 * DAA_PER_SECOND as u64;
            if coherent && caught_up && (!working.stage.cells().is_empty() || working.cursor == info.sink) {
                if !config.allow_unsynced && !node.synced().await? {
                    return Err("node lost synchronization; reconnecting to another candidate".into());
                }
                working.daa = info.virtual_daa_score;
                let reorg = self.commit_sync(working, reports, transactions, true, fee_restart);
                self.pending.retain(|p| !self.tracker.accepted(p.transaction.id()));
                return Ok((true, reorg));
            }
            if delta.added.is_empty() && delta.removed.is_empty() {
                break;
            }
        }
        let reorg = self.commit_sync(working, reports, transactions, false, fee_restart);
        Ok((false, reorg))
    }
    /// Merge only the validation worker's chain-owned fields. Its local spend
    /// snapshot can predate authorizations made while validation was running.
    pub fn publish_validated(&mut self, mut validated: Cache, reorg: bool) {
        let pending = self.pending.first().map(|p| p.transaction.id());
        if let (Some(previous), Some(next)) = (&self.last_reorg, &mut validated.last_reorg) {
            if previous.id == next.id && next.pending_local_transaction.is_none() {
                next.pending_local_transaction = previous.pending_local_transaction;
            }
        }
        for event in validated.reorg_log_pending.iter_mut().chain(&mut validated.reorg_candidates) {
            if event.pending_local_transaction.is_none() {
                event.pending_local_transaction = pending;
            }
        }
        if reorg {
            if let Some(event) = &mut validated.last_reorg {
                if event.pending_local_transaction.is_none() {
                    event.pending_local_transaction = pending;
                }
            }
        }
        self.tracker = validated.tracker;
        self.fee_ledger = validated.fee_ledger;
        self.last_reorg = validated.last_reorg;
        self.game_reorgs = validated.game_reorgs;
        self.reorg_candidates = validated.reorg_candidates;
        self.sync_transactions = validated.sync_transactions;
        self.recent_transactions = validated.recent_transactions;
        self.reorg_log_pending.extend(validated.reorg_log_pending);
        self.transaction_log_pending.extend(validated.transaction_log_pending);
        self.pending.retain(|p| !self.tracker.accepted(p.transaction.id()));
        if reorg {
            for p in &mut self.pending {
                p.branch_changed = true;
            }
        }
    }
    /// Global-tip changes do not stale an independent lane. Only exact game
    /// inputs, reservations or a lost fee-change parent invalidate its draft.
    pub fn draft_current(&self, tx: &Transaction, inputs: &[UtxoEntry]) -> bool {
        if inputs.len() != tx.inputs.len() {
            return false;
        }
        let cells = self.tracker.stage.cells();
        tx.inputs.iter().zip(inputs).all(|(input, entry)| {
            if entry.covenant_id.is_some() {
                cells.iter().any(|(out, u)| *out == input.previous_outpoint && same_utxo(u, entry))
            } else if entry.block_daa_score == u64::MAX {
                self.pending.iter().any(|p| {
                    !p.branch_changed
                        && p.attempted_ms > 0
                        && !self.withdrawals.contains(&p.transaction.id())
                        && !self.unsubmitted.contains(&p.transaction.id())
                        && p.transaction.id() == input.previous_outpoint.transaction_id
                        && p.transaction.outputs.get(input.previous_outpoint.index as usize).is_some_and(|o| {
                            o.covenant.is_none() && o.value == entry.amount && o.script_public_key == entry.script_public_key
                        })
                }) || self.tracker.accepted(input.previous_outpoint.transaction_id)
            } else {
                true
            }
        })
    }
    /// Commit a replay batch; incomplete batches retain evidence until coherent.
    pub fn commit_sync(
        &mut self,
        working: Tracker,
        mut reports: Vec<ReorgEvent>,
        transactions: Vec<TransactionRecord>,
        settled: bool,
        fee_restart: Option<usize>,
    ) -> bool {
        if fee_restart.is_some() {
            // Undo history is bounded, but fees retain accepted game tx IDs.
            // Preserve that evidence before a deep replay rebuilds the ledger.
            if let Some(report) = reports.last_mut().or_else(|| self.reorg_candidates.last_mut()) {
                for (txid, payment) in &self.fee_ledger.payments {
                    if !report.removed_game_transactions.iter().any(|event| event.txid == *txid) {
                        report.removed_game_transactions.push(super::audit::GameEvent {
                            txid: *txid,
                            accepted_daa: None,
                            action: "unknown".into(),
                            side: None,
                            direction: None,
                            payment: Some(payment.clone()),
                        });
                    }
                }
            }
            self.fee_ledger = super::fees::Ledger::new();
        }
        self.tracker = working;
        self.reorg_candidates.extend(reports);
        for (index, event) in transactions.into_iter().enumerate() {
            if fee_restart.is_none_or(|start| index >= start) {
                self.fee_ledger.apply(&event);
            }
            self.sync_transactions.push(event);
        }
        if !settled {
            return false;
        }
        let accepted = self.fee_ledger.payments.keys().copied().collect();
        let event = ReorgEvent::settle(std::mem::take(&mut self.reorg_candidates), &self.sync_transactions, &accepted);
        let game_reorg = event.is_some();
        if let Some(event) = event {
            self.game_reorgs += 1;
            self.last_reorg = Some(event.clone());
            self.reorg_log_pending.push(event);
        }
        for event in net_transaction_changes(std::mem::take(&mut self.sync_transactions)) {
            self.recent_transactions.push_back(event.clone());
            while self.recent_transactions.len() > 6 {
                self.recent_transactions.pop_front();
            }
            self.transaction_log_pending.push(event);
        }
        if game_reorg {
            for p in &mut self.pending {
                p.branch_changed = true;
            }
        }
        game_reorg
    }
    pub fn flush_reorg_log(&mut self, path: &std::path::Path, log: &mut ReorgLog) -> Result<()> {
        if self.reorg_log_pending.is_empty() {
            return Ok(());
        }
        // Persist first; a crash before/after the log write is retried by ID.
        super::save(path, self)?;
        for event in &self.reorg_log_pending {
            log.append(event)?;
        }
        self.reorg_log_pending.clear();
        super::save(path, self)?;
        Ok(())
    }
    pub fn flush_transaction_log(&mut self, path: &std::path::Path, log: &mut ReorgLog) -> Result<Vec<TransactionRecord>> {
        if self.transaction_log_pending.is_empty() {
            return Ok(Vec::new());
        }
        super::save(path, self)?;
        let mut emitted = Vec::new();
        for event in &self.transaction_log_pending {
            if log.append_record(&event.id, event)? {
                emitted.push(event.clone());
            }
        }
        self.transaction_log_pending.clear();
        super::save(path, self)?;
        Ok(emitted)
    }
    pub fn authorize(&mut self, tx: Transaction, inputs: Vec<UtxoEntry>, purpose: String, config: &Config) -> Result<()> {
        if !self.withdrawals.is_empty() || tx.inputs.iter().any(|i| self.unsubmitted.contains(&i.previous_outpoint.transaction_id)) {
            return Err("withdrawal pending or fee parent has never been submitted".into());
        }
        let lane = self.transaction_lane(&tx);
        if let Some(lane) = lane {
            if receiving_contact_due(&self.tracker.stage, (lane % 2) as i64, self.tracker.daa)? {
                return Err("receiving contact is due; confirm pending commands before proving contact".into());
            }
        }
        if self.pending.len() >= QUEUES_PER_PLAYER
            || (!self.pending.is_empty()
                && (lane.is_none() || self.pending.iter().any(|p| self.transaction_lane(&p.transaction).is_none())))
            || self
                .pending
                .iter()
                .any(|p| p.transaction.inputs.iter().any(|a| tx.inputs.iter().any(|b| a.previous_outpoint == b.previous_outpoint)))
        {
            return Err("transaction conflicts with a pending lane or exclusive game action".into());
        }
        let fee = transaction_fee(&tx, &inputs)?;
        if fee > config.fee_sompi {
            return Err("transaction exceeds configured fee cap".into());
        }
        let next = self.authorized_fees.checked_add(fee).ok_or("fee budget overflow")?;
        if next > config.fee_budget_sompi {
            return Err("configured fee budget exhausted; increase it explicitly to continue".into());
        }
        self.authorized_fees = next;
        self.pending.push(Pending { transaction: tx, inputs, purpose, attempted_ms: 0, branch_changed: false });
        Ok(())
    }
    /// A coherent accepted transition spending one of our game inputs proves
    /// this pending transaction lost its race, even if an orphan pool retains it.
    pub fn discard_conflicting_pending(&mut self) -> bool {
        let cells = self.tracker.stage.cells();
        let conflicts = self
            .pending
            .iter()
            .filter(|pending| {
                if self.withdrawals.contains(&pending.transaction.id()) { return false; }
                pending.transaction.inputs.iter().zip(&pending.inputs).any(|(input, entry)| {
                    entry.covenant_id.is_some() && !cells.iter().any(|(out, u)| *out == input.previous_outpoint && same_utxo(u, entry))
                })
            })
            .map(|p| p.transaction.id())
            .collect::<Vec<_>>();
        for id in &conflicts {
            self.discard_pending(*id);
        }
        !conflicts.is_empty()
    }
    /// Also discard descendants spending a dropped transaction's fee change.
    pub fn discard_pending(&mut self, id: Hash) {
        let mut dropped = vec![id];
        for p in &self.pending {
            if p.transaction.inputs.iter().any(|i| dropped.contains(&i.previous_outpoint.transaction_id)) {
                dropped.push(p.transaction.id());
            }
        }
        if dropped.iter().any(|id| self.withdrawals.contains(id)) { return; }
        if dropped.iter().any(|id| self.unsubmitted.contains(id)) {
            // Losing an exposed fee parent also invalidates its fresh child,
            // but cannot release that child's durable reservation early.
            for pending in &mut self.pending {
                if dropped.contains(&pending.transaction.id()) { pending.branch_changed = true; }
            }
            return;
        }
        self.pending.retain(|p| !dropped.contains(&p.transaction.id()));
        self.unsubmitted.retain(|id| !dropped.contains(id));
    }
    fn transaction_lane(&self, tx: &Transaction) -> Option<usize> {
        if let Stage::Live(s) = &self.tracker.stage {
            if tx.inputs.iter().any(|i| i.previous_outpoint == s.court.outpoint) {
                return None;
            }
            return s.players.iter().position(|p| tx.inputs.iter().any(|i| i.previous_outpoint == p.outpoint));
        }
        None
    }
    pub fn available_lane(&self, side: i64) -> Option<usize> {
        let Stage::Live(s) = &self.tracker.stage else {
            return None;
        };
        s.players
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.state.side == side
                    && p.state.count < 4
                    && !self.pending.iter().any(|tx| tx.transaction.inputs.iter().any(|i| i.previous_outpoint == p.outpoint))
            })
            .min_by_key(|(_, p)| (p.state.count, p.state.lane))
            .map(|(i, _)| i)
    }
    pub fn next_action(&self, side: i64, daa: u64, movement: Option<i64>) -> Result<Option<Action>> {
        if !self.withdrawals.is_empty() { return Ok(None); }
        if self.pending.is_empty() {
            return action(&self.tracker.stage, side, daa, movement);
        }
        // Do not extend the pending-command tail once a receiver proof is due.
        // Already authorized commands retain their input/fee reservations.
        if receiving_contact_due(&self.tracker.stage, side, daa)? {
            return Ok(None);
        }
        // Resolves spend our three lanes, so wait for outstanding commands.
        if self.pending.iter().all(|p| self.transaction_lane(&p.transaction).is_some())
            && movement.is_some()
            && self.available_lane(side).is_some()
            && matches!(&self.tracker.stage, Stage::Live(s) if !project(s, daa)?.world.ended())
        {
            return Ok(Some(Action::Move(movement.unwrap())));
        }
        Ok(None)
    }
    /// A single funded wallet can send three batches without waiting for fee
    /// change to be accepted. Ordinary P2PK change carries no DAA-dependent rule.
    fn pending_funding(&self, payer: &[u8], minimum: u64) -> Option<Funding> {
        let script = p2pk(payer);
        self.pending.iter().rev().filter(|p| p.attempted_ms > 0 && !p.branch_changed
            && !self.withdrawals.contains(&p.transaction.id()) && !self.unsubmitted.contains(&p.transaction.id())).find_map(|p| {
            p.transaction.outputs.iter().enumerate().find_map(|(i, output)| {
                let outpoint = kaspa_consensus_core::tx::TransactionOutpoint::new(p.transaction.id(), i as u32);
                (output.covenant.is_none()
                    && output.script_public_key == script
                    && output.value >= minimum
                    && !self.pending.iter().any(|p| p.transaction.inputs.iter().any(|i| i.previous_outpoint == outpoint)))
                .then(|| Funding { outpoint, utxo: UtxoEntry::new(output.value, script.clone(), u64::MAX, false, None) })
            })
        })
    }
}
async fn replay_events(
    tracker: &mut Tracker,
    a: &Artifact,
    invite: &Invitation,
    removed: &[Hash],
    added: &[ChainBlock],
) -> Result<Vec<TransactionRecord>> {
    let mut candidate = tracker.clone();
    let a = a.clone();
    let invite = invite.clone();
    let removed = removed.to_vec();
    let added = added.to_vec();
    let (next, result) = tokio::task::spawn_blocking(move || {
        let result = candidate.apply_with_events(&a, &invite, &removed, &added).map_err(|e| e.to_string());
        (candidate, result)
    })
    .await?;
    *tracker = next;
    result.map_err(Into::into)
}
pub fn transaction_fee(tx: &Transaction, inputs: &[UtxoEntry]) -> Result<u64> {
    if inputs.len() != tx.inputs.len() {
        return Err("fee calculation lacks source entries".into());
    }
    let sum = |values: Vec<u64>| values.into_iter().try_fold(0u64, |total, v| total.checked_add(v).ok_or("amount overflow"));
    let input = sum(inputs.iter().map(|u| u.amount).collect())?;
    let output = sum(tx.outputs.iter().map(|o| o.value).collect())?;
    Ok(input.checked_sub(output).ok_or("outputs exceed inputs")?)
}
/// Match the pinned node's post-Toccata relay pricing, including transient mass.
/// This is a configured relay rate, not a prediction of congestion priority.
pub fn relay_fee(tx: &Transaction, config: &Config) -> Result<u64> {
    let masses = MassCalculator::new_with_consensus_params(&MAINNET_PARAMS).calc_non_contextual_masses(tx);
    let mass = masses.compute_mass.max(masses.normalized_transient(&MAINNET_PARAMS.mempool_block_mass_cofactors().after()));
    let fee = mass.checked_mul(config.relay_fee_per_kg).ok_or("fee calculation overflow")? / 1000;
    let fee = if fee == 0 { config.relay_fee_per_kg } else { fee };
    if fee > config.fee_sompi {
        return Err(format!("estimated fee {fee} exceeds fee_sompi cap {}; raise the cap explicitly", config.fee_sompi).into());
    }
    Ok(fee)
}
pub fn priced_transaction(config: &Config, mut build: impl FnMut(u64) -> Result<Transaction>) -> Result<Transaction> {
    // A trial at the cap can create tiny change and fail storage validation,
    // although the actual relay-priced transaction is valid. Start low and
    // converge before authorizing/submitting; relay_fee still enforces the cap.
    let mut fee = 1;
    for _ in 0..3 {
        let tx = build(fee)?;
        let required = relay_fee(&tx, config)?;
        if fee == required {
            return Ok(tx);
        }
        fee = required;
    }
    Err("transaction fee did not stabilize".into())
}
fn check_cell(b: &TxBuilder<'_>, actor: &str, state: State, u: &UtxoEntry) -> Result<()> {
    let expected = b.covenant_utxo(actor, state, u.amount, u.block_daa_score, u.is_coinbase, u.covenant_id)?;
    if !same_utxo(u, &expected) {
        return Err("cached typed state does not match its UTXO commitment; rescan required".into());
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Move(i64),
    Checkpoint(usize, i64),
    Resolve(bool),
    Timeout,
    Settle,
    Reclaim,
}
/// Uses accepted game state and chain DAA, never pending/mempool progress.
pub fn receiving_contact_due(stage: &Stage, side: i64, daa: u64) -> Result<bool> {
    if let Stage::Live(s) = stage {
        if !s.court.state.ended() {
            let (receiver, deadline) = next_contact(s)?;
            return Ok(side == receiver as i64 && i64::try_from(daa)? >= deadline);
        }
    }
    Ok(false)
}
pub fn action(stage: &Stage, side: i64, daa: u64, movement: Option<i64>) -> Result<Option<Action>> {
    match stage {
        Stage::Live(s) => {
            if side != 0 && side != 1 {
                return Err("invalid player side".into());
            }
            if s.court.state.ended() {
                return Ok(Some(Action::Settle));
            }
            let (receiver, deadline) = next_contact(s)?;
            let now = i64::try_from(daa)?;
            // Only the receiver can prove contact. An unanswered contact lets
            // its opponent end the entire match after fifty DAA.
            if side as usize != receiver && now >= timeout_result(s)?.end_daa {
                return Ok(Some(Action::Timeout));
            }
            if now >= deadline && side as usize == receiver {
                return Ok(Some(Action::Resolve(false)));
            }
            let own = side as usize;
            let count: usize = (own..QUEUE_COUNT).step_by(2).map(|i| s.players[i].state.count).sum();
            if let Some(direction) =
                movement.filter(|_| count < QUEUES_PER_PLAYER * 3 && !project(s, daa).map(|p| p.world.ended()).unwrap_or(false))
            {
                if (own..QUEUE_COUNT).step_by(2).any(|i| s.players[i].state.count < 4) {
                    return Ok(Some(Action::Move(direction)));
                }
            }
            let until = now.min(deadline);
            // Low-volume commands can stay queued until contact. Compact
            // early only for queue pressure; age alone need not incur a fee.
            if count >= 8 && until > s.players[own].state.through {
                return Ok(Some(Action::Checkpoint(own, until)));
            }
            Ok(None)
        }
        Stage::Collecting(_) => Ok(Some(Action::Reclaim)),
        _ => Ok(None),
    }
}
pub async fn build_action(
    a: &Artifact,
    node: &Node,
    cache: &Cache,
    player: &Keypair,
    payer: &Keypair,
    which: Action,
    config: &Config,
) -> Result<(Transaction, Vec<UtxoEntry>, String, super::reservations::Claim)> {
    let minimum = config.fee_sompi.checked_add(1).ok_or("fee overflow")?;
    let public = pubkey(payer);
    let (funding, mut claim) = match cache.pending_funding(&public, minimum).or_else(|| cache.pending_funding(&pubkey(player), minimum)) {
        Some(f) => { let claim = super::reservations::Claim::inputs(vec![f.outpoint])?; (f, claim) },
        None => {
            super::wallet::reserved_fee_funding(
                node, player, payer,
                minimum,
                config.coinbase_maturity_daa,
                &cache.pending.iter().flat_map(|p| p.transaction.inputs.iter().map(|i| i.previous_outpoint)).collect::<Vec<_>>(),
            )
            .await?
        }
    };
    let lane = if matches!(which, Action::Move(_)) {
        Some(
            cache
                .available_lane(player_side(
                    match &cache.tracker.stage {
                        Stage::Live(s) => &s.court.state,
                        _ => return Err("no live game".into()),
                    },
                    player,
                )?)
                .ok_or("all input lanes are pending or full")?,
        )
    } else {
        None
    };
    let a = a.clone();
    let stage = cache.tracker.stage.clone();
    let player = *player;
    let payer = *super::wallet::funding_key(&funding, &player, payer)?;
    let config = config.clone();
    // VM execution/signing must not block concurrent chain-clock RPC polling.
    tokio::task::spawn_blocking(move || {
        let (tx, inputs, purpose) = build_funded_action(&a, &stage, &player, &payer, which, &config, funding, lane).map_err(|e| e.to_string())?;
        claim.restrict(tx.inputs.iter().map(|i| i.previous_outpoint).collect()).map_err(|e| e.to_string())?;
        Ok::<_, String>((tx, inputs, purpose, claim))
    })
    .await?
    .map_err(Into::into)
}
fn build_funded_action(
    a: &Artifact,
    stage: &Stage,
    player: &Keypair,
    payer: &Keypair,
    which: Action,
    config: &Config,
    funding: Funding,
    lane: Option<usize>,
) -> Result<(Transaction, Vec<UtxoEntry>, String)> {
    let b = TxBuilder::new(a)?;
    match (stage, which) {
        (Stage::Live(s), Action::Move(direction)) => {
            let side = lane.ok_or("missing input lane")?;
            let tx =
                priced_transaction(config, |fee| Ok(command_tx(&b, &s.players[side], direction, player, &funding, payer, fee)?.0))?;
            Ok((tx, vec![s.players[side].utxo.clone(), funding.utxo], format!("move {direction}")))
        }
        (Stage::Live(s), Action::Checkpoint(side, until)) => {
            let tx = priced_transaction(config, |fee| Ok(checkpoint_tx(&b, s, side, until, player, &funding, payer, fee)?.0))?;
            Ok((
                tx,
                vec![
                    s.court.utxo.clone(),
                    s.players[side].utxo.clone(),
                    s.players[side + 2].utxo.clone(),
                    s.players[side + 4].utxo.clone(),
                    funding.utxo,
                ],
                "paddle checkpoint".into(),
            ))
        }
        (Stage::Live(s), Action::Resolve(takeover)) => {
            let (side, _) = next_contact(s)?;
            let tx = priced_transaction(config, |fee| Ok(resolve_tx(&b, s, player, &funding, payer, fee, takeover)?.0))?;
            Ok((
                tx,
                vec![
                    s.court.utxo.clone(),
                    s.players[side].utxo.clone(),
                    s.players[side + 2].utxo.clone(),
                    s.players[side + 4].utxo.clone(),
                    funding.utxo,
                ],
                "contact".into(),
            ))
        }
        (Stage::Live(s), Action::Settle) => {
            let tx = priced_transaction(config, |fee| settle_tx(&b, s, &funding, payer, fee))?;
            Ok((tx, vec![s.court.utxo.clone(), funding.utxo], "settlement".into()))
        }
        (Stage::Live(s), Action::Timeout) => {
            let tx = priced_transaction(config, |fee| Ok(timeout_tx(&b, s, player, &funding, payer, fee)?.0))?;
            Ok((tx, vec![s.court.utxo.clone(), funding.utxo], "match timeout".into()))
        }
        (Stage::Collecting(s), Action::Reclaim) => Ok((
            priced_transaction(config, |fee| reclaim_tx(&b, s, &funding, payer, fee))?,
            std::iter::once(s.collector.utxo.clone())
                .chain(s.players.iter().map(|p| p.utxo.clone()))
                .chain(std::iter::once(funding.utxo))
                .collect(),
            "refund".into(),
        )),
        _ => Err("state changed before transaction construction".into()),
    }
}
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().min(u128::from(u64::MAX)) as u64
}
