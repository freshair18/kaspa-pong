//! One durable wallet journal for reusable queues, offers and games. Signed
//! authorizations survive uncertain RPC results; chain undo restores leases and
//! pending reservations together. Nothing becomes available on an RPC absence.
use super::{
    Config,
    queue_codes::{HostCode, MAGIC, Reservation},
    queue_sets::{Authorization, Inventory, Phase, Record},
    queue_tracker::QueueStage,
    reservations,
    tracker::{AcceptedTx, ChainBlock},
};
use crate::{engine::*, reusable::Set};
use argent_runtime::{Artifact, TxBuilder};
use kaspa_consensus_core::{
    Hash,
    tx::{Transaction, TransactionOutpoint, UtxoEntry},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    path::Path,
};

const HISTORY: usize = 2048;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalOffer {
    pub code: HostCode,
    pub available: bool,
    pub expected_guest: Option<Vec<u8>>,
    pub automatic: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: Hash,
    pub reservation: Reservation,
    pub creation: Transaction,
    pub creation_inputs: Vec<UtxoEntry>,
    pub host_set: Option<Set>,
    pub stage: Option<QueueStage>,
    pub cancel_requested: bool,
    pub created_ms: u64,
    pub authorized_fees: u64,
    pub paid_fees: u64,
    pub recovered: bool,
    pub unavailable: bool,
    pub cursor: Hash,
    pub anchor: Hash,
    pub fee_budget: u64,
}
impl Session {
    pub fn side(&self, owner: &[u8]) -> usize {
        usize::from(self.reservation.host.queue.owner == owner)
    }
    pub fn finished(&self) -> bool {
        self.recovered || matches!(self.stage, Some(QueueStage::Finished { .. } | QueueStage::Cancelled(_) | QueueStage::RecoveredReset(_)))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Effect {
    Wallet { closes: Vec<Hash> },
    Game { session: Hash },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Signed {
    pub authorization: Authorization,
    pub effect: Effect,
    pub attempted_ms: u64,
    pub fee: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct State {
    records: Vec<Record>,
    stages: Vec<(Hash, Option<QueueStage>, bool, bool, Hash)>,
    session_count: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Undo {
    hash: Hash,
    parent: Hash,
    daa: u64,
    before: Option<State>,
    status: Vec<(Hash, bool, bool)>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Book {
    version: u32,
    pub wallet: Inventory,
    pub cursor: Hash,
    pub daa: u64,
    pub sessions: Vec<Session>,
    pub offers: Vec<LocalOffer>,
    pub signed: Vec<Signed>,
    // First-send permission is valid only for an authorization made in this process.
    #[serde(skip)]
    pub direct_send: HashSet<Hash>,
    confirmed: HashSet<Hash>,
    conflicted: HashSet<Hash>,
    history: VecDeque<Undo>,
    pub reorgs: u64,
    pub default_fee_budget: u64,
    #[serde(default)]
    pub recent_transactions:VecDeque<super::audit::TransactionRecord>,
    #[serde(default)]
    previous_creation_pending: bool,
}
impl Book {
    pub fn new(network: String, owner: Vec<u8>, cursor: Hash) -> Self {
        Self {
            version: 3,
            wallet: Inventory::new(network, owner),
            cursor,
            daa: 0,
            sessions: vec![],
            offers: vec![],
            signed: vec![],
            direct_send: HashSet::new(),
            confirmed: HashSet::new(),
            conflicted: HashSet::new(),
            history: VecDeque::new(),
            reorgs: 0,
            default_fee_budget: Config::default().fee_budget_sompi,
            recent_transactions:VecDeque::new(),
            previous_creation_pending: false,
        }
    }
    pub fn load(path: &Path, network: &str, owner: &[u8]) -> Result<Self> {
        let mut book: Self = super::load(path)?;
        if book.version != 3 {
            return Err("unsupported reusable wallet journal version".into());
        }
        book.refresh_pending();
        book.wallet.validate(network, owner)?;
        let a = crate::reusable::artifact()?;
        let b = TxBuilder::new(&a)?;
        for session in &book.sessions {
            if session.id != session.creation.id()
                || (session.reservation.guest.owner != owner && session.reservation.host.queue.owner != owner)
            {
                return Err("invalid saved game identity".into());
            }
            if let Some(host)=&session.host_set{host.validate(&b)?;if super::queue_codes::Descriptor::from_set(host)?.owner!=session.reservation.host.queue.owner{return Err("saved host ownership differs from invitation".into());}}
            if session.reservation.payload()? != session.creation.payload{return Err("saved game differs from its signed reservation".into());}
            session.reservation.accepted(
                &b,
                &AcceptedTx { tx: session.creation.clone(), inputs: session.creation_inputs.clone() },
                0,
            )?;
            for set in session.stage.iter().flat_map(QueueStage::sets) {
                set.validate(&b)?;
            }
        }
        book.restore(path)?;
        Ok(book)
    }
    /// Keep prior-contract capital visible and old pending inputs reserved.
    /// Old active games remain with the previous client; only idle sets without
    /// an uncertain old spend may be explicitly closed by this client.
    pub fn import_previous(&mut self, paths: &[&Path]) -> Result<()> {
        self.previous_creation_pending = false;
        let mut imported = HashSet::new();
        // Read the original journal first. A newer journal may also hold
        // a copy of that set, or a record that it closed the set.
        for path in paths {
            self.import_previous_journal(path, &mut imported)?;
        }
        Ok(())
    }
    fn import_previous_journal(&mut self, path: &Path, imported: &mut HashSet<Hash>) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let previous: Self = super::load(path)?;
        if previous.wallet.owner != self.wallet.owner || previous.wallet.network != self.wallet.network {
            return Err("previous queue journal belongs to another wallet or network".into());
        }
        let pending = previous.pending();
        for r in &previous.wallet.records {
            super::queue_sets::artifact_for(&r.set)?;
            if r.set.lanes[0].state.queue.owner != self.wallet.owner {
                return Err("previous queue owner mismatch".into());
            }
            let id = r.set.id()?;
            let offered = previous.offers.iter().any(|o| {
                o.available && o.code.queue.id == id && o.code.queue.generation == r.set.generation()
            });
            let pending_spend = pending.iter().any(|p| {
                p.authorization.transaction.inputs.iter().any(|input| {
                    r.set.lanes.iter().any(|cell| cell.outpoint == input.previous_outpoint)
                })
            });
            let phase = if r.phase == Phase::Closed {
                Phase::Closed
            } else if matches!(r.phase, Phase::Available | Phase::Legacy) && r.set.idle() && !offered && !pending_spend {
                Phase::Legacy
            } else {
                Phase::Unavailable
            };
            let first = imported.insert(id);
            if let Some(known) = self.wallet.records.iter_mut().find(|n| n.set.id().ok() == Some(id)) {
                if known.phase != Phase::Closed {
                    if first || phase == Phase::Closed {
                        *known = Record { set: r.set.clone(), phase };
                    } else if pending_spend {
                        // Both client versions may have authorized spends.
                        known.phase = Phase::Unavailable;
                    }
                }
            } else {
                self.wallet.records.push(Record { set: r.set.clone(), phase });
            }
        }
        self.previous_creation_pending |= pending.iter().any(|p| p.authorization.creates_set);
        self.wallet.created_count = self.wallet.created_count.max(previous.wallet.created_count);
        previous.restore(path)?;
        Ok(())
    }
    pub fn may_auto_create(&self) -> bool {
        self.wallet.first_creation() && !self.previous_creation_pending
    }
    pub fn can_bootstrap_idle(&self) -> bool {
        self.sessions.iter().all(Session::finished)
            && !self.offers.iter().any(|o| o.available)
            && self.wallet.records.iter().all(|r| r.phase == Phase::Closed || r.set.idle())
            && !self.pending().iter().any(|p| matches!(p.effect, Effect::Game { .. }))
    }
    /// Fee selection reuses ordinary change from attempted game transactions.
    /// Native resolution outputs have separate command-only provenance checks.
    pub fn pending_fee_coins(&self, session: Hash) -> Vec<Funding> {
        let pending = self.pending();
        let script = p2pk(&self.wallet.owner);
        pending.iter().rev().filter(|p| p.attempted_ms > 0 && matches!(p.effect, Effect::Game { session: id } if id == session))
            .flat_map(|p| {
                let tx = &p.authorization.transaction;
                tx.outputs.iter().enumerate().filter_map(|(index, output)| {
                    let outpoint = TransactionOutpoint::new(tx.id(), index as u32);
                    (output.covenant.is_none() && output.script_public_key == script
                        && !pending.iter().any(|p| p.authorization.transaction.inputs.iter().any(|i| i.previous_outpoint == outpoint)))
                        .then(|| Funding { outpoint, utxo: UtxoEntry::new(output.value, script.clone(), u64::MAX, false, None) })
                }).collect::<Vec<_>>()
            }).collect()
    }
    pub fn fee_parents(&self, tx: &Transaction, entries: &[UtxoEntry]) -> Vec<Transaction> {
        self.pending().into_iter().filter(|p| {
            p.attempted_ms > 0 && tx.inputs.iter().any(|i| i.previous_outpoint.transaction_id == p.authorization.transaction.id())
                && tx.inputs.iter().zip(entries).filter(|(i, _)| i.previous_outpoint.transaction_id == p.authorization.transaction.id())
                    .all(|(_, u)| u.covenant_id.is_none() && u.script_public_key == p2pk(&self.wallet.owner))
        }).map(|p| p.authorization.transaction.clone()).collect()
    }
    pub fn pending_resolve(&self, session: Hash) -> Option<&Signed> {
        let QueueStage::Live(current) = self.session(session).ok()?.stage.as_ref()? else { return None };
        self.pending().into_iter().find(|p| p.attempted_ms > 0 && p.authorization.purpose == "resolve"
            && matches!(p.effect, Effect::Game { session: id } if id == session)
            && p.authorization.transaction.inputs.first().is_some_and(|i| i.previous_outpoint == current.court.outpoint))
    }
    /// Interpret only an exact submitted resolution of the canonical game.
    /// These outputs are planning/provenance data, never accepted game state.
    pub fn resolve_successor(&self, session: Hash) -> Result<Option<crate::reusable::Match>> {
        let Some(p) = self.pending_resolve(session) else { return Ok(None) };
        let mut stage = self.session(session)?.stage.clone().ok_or("missing resolution source")?;
        let cells = stage.cells();
        let tx = &p.authorization.transaction;
        if tx.inputs.len() != p.authorization.inputs.len() || tx.inputs.iter().zip(&p.authorization.inputs)
            .filter(|(_, u)| u.covenant_id.is_some())
            .any(|(i, u)| !cells.iter().any(|(out, actual)| *out == i.previous_outpoint && super::tracker::same_utxo(actual, u))) {
            return Ok(None);
        }
        let a = crate::reusable::artifact()?; let b = TxBuilder::new(&a)?;
        let QueueStage::Live(before) = &stage else { return Ok(None) };
        let args = super::tracker::call(&b, &a, "Court", before.court.state.state(), &tx.inputs[0].signature_script)?;
        if !super::tracker::is_entry(&a, "Court", "resolve", &args)? { return Ok(None); }
        stage.apply(&a, &b, &AcceptedTx { tx: tx.clone(), inputs: p.authorization.inputs.clone() }, u64::MAX)?;
        let QueueStage::Live(next) = stage else { return Ok(None) };
        if next.sets.iter().flat_map(|s| &s.lanes).filter(|c| c.outpoint.transaction_id == tx.id())
            .any(|c| c.state.queue.times[..c.state.queue.count].contains(&-1)) {
            return Ok(None);
        }
        Ok(Some(next))
    }
    pub fn command_successors(&self, tx: &Transaction, entries: &[UtxoEntry]) -> Result<Vec<(TransactionOutpoint, UtxoEntry)>> {
        if tx.inputs.len() != entries.len() || entries.iter().filter(|u| u.covenant_id.is_some()).count() != 1 {
            return Ok(vec![]);
        }
        let index = entries.iter().position(|u| u.covenant_id.is_some()).unwrap();
        if tx.inputs[index].sequence != 0 || entries.iter().any(|u| u.covenant_id.is_none() && u.script_public_key != p2pk(&self.wallet.owner)) {
            return Ok(vec![]);
        }
        for session in &self.sessions {
            let Some(parent) = self.pending_resolve(session.id) else { continue };
            if parent.authorization.transaction.id() != tx.inputs[index].previous_outpoint.transaction_id { continue; }
            let Some(next) = self.resolve_successor(session.id)? else { continue };
            for cell in &next.sets[session.side(&self.wallet.owner)].lanes {
                if cell.outpoint != tx.inputs[index].previous_outpoint || !super::tracker::same_utxo(&cell.utxo, &entries[index]) { continue; }
                let a = crate::reusable::artifact()?; let b = TxBuilder::new(&a)?;
                let args = super::tracker::call(&b, &a, cell.state.actor(), cell.state.state(), &tx.inputs[index].signature_script)?;
                if super::tracker::is_entry(&a, cell.state.actor(), "command", &args)? {
                    return Ok(vec![(cell.outpoint, cell.utxo.clone())]);
                }
            }
        }
        Ok(vec![])
    }
    pub fn source_parents(&self, tx: &Transaction, entries: &[UtxoEntry]) -> Result<Vec<Transaction>> {
        let mut parents = self.fee_parents(tx, entries);
        for (out, _) in self.command_successors(tx, entries)? {
            if !parents.iter().any(|p| p.id() == out.transaction_id) {
                if let Some(p) = self.pending().into_iter().find(|p| p.authorization.transaction.id() == out.transaction_id) {
                    parents.push(p.authorization.transaction.clone());
                }
            }
        }
        Ok(parents)
    }
    fn conflict_descendants(&mut self) -> bool {
        let mut changed = false;
        loop {
            let descendants = self.signed.iter().filter(|s| {
                let tx = &s.authorization.transaction;
                !self.confirmed.contains(&tx.id()) && !self.conflicted.contains(&tx.id())
                    && tx.inputs.iter().any(|i| self.conflicted.contains(&i.previous_outpoint.transaction_id))
            }).map(|s| s.authorization.transaction.id()).collect::<Vec<_>>();
            if descendants.is_empty() { break; }
            self.conflicted.extend(descendants);
            changed = true;
        }
        changed
    }
    fn state(&self) -> State {
        State {
            records: self.wallet.records.clone(),
            stages: self
                .sessions
                .iter()
                .filter(|s| !s.finished() || s.unavailable)
                .map(|s| (s.id, s.stage.clone(), s.unavailable, s.recovered, s.cursor))
                .collect(),
            session_count: self.sessions.len(),
        }
    }
    fn restore_state(&mut self, s: State) {
        self.wallet.records = s.records;
        for (index, session) in self.sessions.iter_mut().enumerate() {
            if let Some((_, stage, unavailable, recovered, cursor)) = s.stages.iter().find(|(id, ..)| *id == session.id) {
                session.stage = stage.clone();
                session.unavailable = *unavailable;
                session.recovered = *recovered;
                session.cursor = *cursor;
            } else if index >= s.session_count {
                session.stage = None;
            }
        }
    }

    fn refresh_pending(&mut self) {
        self.wallet.pending = self
            .signed
            .iter()
            .filter(|s| self.is_pending(s.authorization.transaction.id()))
            .map(|s| s.authorization.clone())
            .collect();
        self.direct_send.retain(|id| self.wallet.pending.iter().any(|p| p.transaction.id() == *id));
    }
    pub fn is_pending(&self, id: Hash) -> bool {
        !self.confirmed.contains(&id) && !self.conflicted.contains(&id)
    }
    pub fn is_conflicted(&self, id: Hash) -> bool {
        self.conflicted.contains(&id)
    }
    pub fn was_confirmed(&self, id: Hash) -> bool {
        self.confirmed.contains(&id)
    }
    pub fn pending(&self) -> Vec<&Signed> {
        self.signed.iter().filter(|s| self.is_pending(s.authorization.transaction.id())).collect()
    }
    pub fn pending_game(&self, id: Hash) -> bool {
        self.pending().iter().any(|s| matches!(s.effect,Effect::Game{session} if session==id))
    }
    pub fn available(&self) -> Vec<&Set> {
        self.wallet
            .available()
            .filter(|s| {
                !self
                    .offers
                    .iter()
                    .any(|o| o.available && o.code.queue.id == s.id().unwrap() && o.code.queue.generation == s.generation())
            })
            .collect()
    }
    pub fn session(&self, id: Hash) -> Result<&Session> {
        self.sessions.iter().find(|s| s.id == id).ok_or_else(|| "game is not in this wallet journal".into())
    }
    pub fn rematch_offer(&self, s: &Session) -> Option<HostCode> {
        if !s.finished() || s.recovered || s.side(&self.wallet.owner) != 1 {
            return None;
        }
        self.offers
            .iter()
            .find(|o| {
                o.available
                    && o.automatic
                    && o.expected_guest.as_ref() == Some(&s.reservation.guest.owner)
                    && o.code.queue.id == s.reservation.host.queue.id
                    && o.code.queue.generation == s.reservation.host.queue.generation + 1
                    && o.code.tag != s.reservation.host.tag
                    && o.code.terms().ok() == s.reservation.host.terms().ok()
                    && self
                        .wallet
                        .available()
                        .any(|set| set.id().ok() == Some(o.code.queue.id) && set.generation() == o.code.queue.generation)
            })
            .map(|o| o.code.clone())
    }
    pub fn reserve_offer(&mut self, code: HostCode, expected_guest: Option<Vec<u8>>, automatic: bool) -> Result<()> {
        if !self.available().iter().any(|s| s.id().ok() == Some(code.queue.id) && s.generation() == code.queue.generation) {
            return Err("queue set is already reserved or settling".into());
        }
        self.offers.push(LocalOffer { code, available: true, expected_guest, automatic });
        Ok(())
    }
    pub fn abandon_offer(&mut self, tag: [u8; 8]) {
        for o in &mut self.offers {
            if o.code.tag == tag {
                o.available = false;
            }
        }
        let ids = self
            .sessions
            .iter()
            .filter(|s| {
                s.reservation.host.tag == tag && s.side(&self.wallet.owner) == 1 && matches!(s.stage, Some(QueueStage::Lobby { .. }))
            })
            .map(|s| s.id)
            .collect::<Vec<_>>();
        for id in ids {
            let _ = self.request_cancel(id);
        }
    }
    pub fn add_session(&mut self, mut session: Session) -> Result<()> {
        if self.sessions.iter().any(|s| s.id == session.id) {
            return Ok(());
        }
        if session.reservation.guest.owner != self.wallet.owner && session.reservation.host.queue.owner != self.wallet.owner {
            return Err("game belongs to another wallet".into());
        }
        session.cursor = self.cursor;
        session.anchor = self.cursor;
        let id = session.id;
        self.sessions.push(session);
        self.update_records(&[id]);
        Ok(())
    }
    pub fn request_cancel(&mut self, id: Hash) -> Result<()> {
        let s = self.sessions.iter_mut().find(|s| s.id == id).ok_or("unknown game")?;
        s.cancel_requested = true;
        if s.side(&self.wallet.owner) == 1 && matches!(s.stage, Some(QueueStage::Lobby { .. })) {
            for offer in &mut self.offers {
                if offer.code == s.reservation.host {
                    offer.available = false;
                }
            }
            if let Some(r) = self.wallet.records.iter_mut().find(|r| r.set.id().ok() == Some(s.reservation.host.queue.id)) {
                if r.set.idle() {
                    r.phase = Phase::Available;
                }
            }
        }
        Ok(())
    }
    pub fn authorize(&mut self, signed: Signed, config: &Config) -> Result<()> {
        let id = signed.authorization.transaction.id();
        if self.signed.iter().any(|s| s.authorization.transaction.id() == id) {
            return Ok(());
        }
        if self.pending().iter().any(|old| {
            old.authorization
                .transaction
                .inputs
                .iter()
                .any(|i| signed.authorization.transaction.inputs.iter().any(|j| i.previous_outpoint == j.previous_outpoint))
        }) {
            return Err("transaction conflicts with an uncertain authorization".into());
        }
        let fee = super::client::transaction_fee(&signed.authorization.transaction, &signed.authorization.inputs)?;
        if fee != signed.fee || fee == 0 || fee > config.fee_sompi {
            return Err("authorization exceeds the reviewed fee allowance".into());
        }
        let artifact = crate::reusable::artifact()?;
        let builder = TxBuilder::new(&artifact)?;
        signed.authorization.validate(&builder, &self.wallet.owner)?;
        if let Effect::Game { session } = signed.effect {
            let s = self.sessions.iter_mut().find(|s| s.id == session).ok_or("missing game for authorization")?;
            if signed.authorization.purpose == "start" && s.cancel_requested {
                return Err("host offer was cancelled before authorization".into());
            }
            let total = s.authorized_fees.checked_add(fee).ok_or("game fee overflow")?;
            if total > s.fee_budget {
                return Err("game fee budget exhausted; open Wallet > Game status / fee budget to extend it".into());
            }
            s.authorized_fees = total;
        }
        if signed.authorization.creates_set {
            self.wallet.created_count = self.wallet.created_count.checked_add(1).ok_or("queue count overflow")?;
        }
        self.signed.push(signed);
        self.refresh_pending();
        Ok(())
    }
    pub fn restore(&self, path: &Path) -> Result<()> {
        let inputs = self
            .wallet
            .records
            .iter()
            .filter(|r| r.phase != Phase::Closed)
            .flat_map(|r| r.set.lanes.iter().map(|c| c.outpoint))
            .chain(self.pending().iter().flat_map(|s| s.authorization.transaction.inputs.iter().map(|i| i.previous_outpoint)))
            .collect();
        reservations::restore(path, inputs)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        super::save(path, self)?;
        self.restore(path)
    }
    /// Apply a full RPC batch atomically. Empty blocks retain only their parent;
    /// snapshots are stored when a relevant transition changes wallet state.
    pub fn apply(&mut self, a: &Artifact, removed: &[Hash], added: &[ChainBlock]) -> Result<bool> {
        let b = TxBuilder::new(a)?;
        let mut candidate = self.clone();
        let mut changed = !removed.is_empty();
        if !removed.is_empty() {
            candidate.reorgs += 1;
            candidate.direct_send.clear();
        }
        for hash in removed {
            let reverted=candidate.recent_transactions.iter().filter(|r|r.accepting_block==Some(*hash) && r.status==super::audit::TransactionStatus::Accepted).cloned().collect::<Vec<_>>();
            for mut r in reverted {r.status=super::audit::TransactionStatus::Reverted;candidate.recent_transactions.push_back(r);}
            let undo = candidate.history.pop_back().ok_or("queue reorg exceeds retained history; resynchronization required")?;
            if undo.hash != *hash || candidate.cursor != *hash {
                return Err("incoherent queue chain rollback".into());
            }
            if let Some(before) = undo.before {
                candidate.restore_state(before);
            }
            for (id, confirmed, conflicted) in undo.status {
                if !confirmed && candidate.confirmed.contains(&id) {
                    if let Some(s) = candidate.signed.iter().find(|s| s.authorization.transaction.id() == id) {
                        if let Effect::Game { session } = s.effect {
                            if let Some(game) = candidate.sessions.iter_mut().find(|g| g.id == session) {
                                game.paid_fees = game.paid_fees.saturating_sub(s.fee);
                            }
                        }
                    }
                }
                if confirmed {
                    candidate.confirmed.insert(id);
                } else {
                    candidate.confirmed.remove(&id);
                }
                if conflicted {
                    candidate.conflicted.insert(id);
                } else {
                    candidate.conflicted.remove(&id);
                }
            }
            candidate.cursor = undo.parent;
            candidate.daa = undo.daa;
        }
        for block in added {
            let before = candidate.state();
            let statuses = candidate
                .signed
                .iter()
                .map(|s| {
                    let id = s.authorization.transaction.id();
                    (id, candidate.confirmed.contains(&id), candidate.conflicted.contains(&id))
                })
                .collect::<Vec<_>>();
            let mut relevant = false;
            for accepted in &block.transactions {
                relevant |= candidate.accepted(a, &b, accepted, block.daa,block.hash)?;
            }
            candidate.history.push_back(Undo {
                hash: block.hash,
                parent: candidate.cursor,
                daa: candidate.daa,
                before: relevant.then_some(before),
                status: statuses
                    .into_iter()
                    .filter(|(id, c, f)| *c != candidate.confirmed.contains(id) || *f != candidate.conflicted.contains(id))
                    .collect(),
            });
            candidate.cursor = block.hash;
            candidate.daa = block.daa;
            changed |= relevant;
            for s in &mut candidate.sessions {
                if !s.unavailable && !s.recovered {
                    s.cursor = block.hash;
                }
            }
            while candidate.history.len() > HISTORY {
                candidate.history.pop_front();
            }
        }
        let retained = candidate.history.iter().flat_map(|h| h.status.iter().map(|(id, _, _)| *id)).collect::<HashSet<_>>();
        candidate.signed.retain(|s| {
            let id = s.authorization.transaction.id();
            retained.contains(&id) || (!candidate.confirmed.contains(&id) && !candidate.conflicted.contains(&id))
        });
        candidate.refresh_pending();
        while candidate.recent_transactions.len()>64{candidate.recent_transactions.pop_front();}
        *self = candidate;
        Ok(changed)
    }
    fn accepted(&mut self, a: &Artifact, b: &TxBuilder<'_>, accepted: &AcceptedTx, daa: u64,block:Hash) -> Result<bool> {
        let tx = &accepted.tx;
        let id = tx.id();
        let mut changed = false;
        let mut changed_sessions = vec![];
        let mut bank_restorations = HashSet::new();
        let owner = self.wallet.owner.clone();
        let before=self.sessions.iter().filter(|s|s.id==id || s.stage.as_ref().is_some_and(|stage|stage.cells().iter().any(|(out,_)|tx.inputs.iter().any(|i|i.previous_outpoint==*out))))
            .map(|s|(s.id,s.stage.as_ref().map(QueueStage::display).unwrap_or(super::tracker::Stage::AwaitingLobby))).collect::<Vec<_>>();
        // Resume a saved guest reservation even after an ambiguous submit.
        for session in &mut self.sessions {
            if session.recovered {
                continue;
            }
            if (session.stage.is_none() || session.unavailable) && session.id == id {
                let (lobby, guest) = session.reservation.accepted(b, accepted, daa)?;
                session.stage = Some(if let Some(host)=&session.host_set{QueueStage::Lobby { lobby, sets: [guest,host.clone()] }}else{QueueStage::Invitation{lobby,guest}});
                session.unavailable = false;
                changed = true;
                changed_sessions.push(session.id);
            } else if !session.unavailable
                && let Some(stage) = &mut session.stage
            {
                let abandoned_host = session.cancel_requested && session.reservation.host.queue.owner == owner;
                let skip = if let QueueStage::Lobby { lobby, sets } = stage {
                    abandoned_host
                        && !tx.inputs.iter().any(|i| {
                            i.previous_outpoint == lobby.outpoint || sets[0].lanes.iter().any(|c| c.outpoint == i.previous_outpoint)
                        })
                } else {
                    false
                };
                let restores_bank = matches!(stage, QueueStage::Finished { .. } | QueueStage::Cancelled(_) | QueueStage::RecoveredReset(_));
                if !skip && stage.apply(a, b, accepted, daa)? {
                    if restores_bank { bank_restorations.insert(session.id); }
                    if matches!(stage, QueueStage::RecoveredReset(set) if set.idle()) { session.recovered = true; }
                    changed = true;
                    changed_sessions.push(session.id);
                }
            }
        }
        // A host discovers only invitations addressed to its exact persisted
        // offer. Invalid payloads cannot stall following unrelated chain data.
        if tx.payload.starts_with(MAGIC) && !self.sessions.iter().any(|s| s.id == id) {
            if let Ok(reservation) = Reservation::parse(tx, &self.wallet.network) {
                if let Some(local_offer) = self.offers.iter().find(|o| {
                    o.available
                        && o.code == reservation.host
                        && o.expected_guest.as_ref().is_none_or(|p| *p == reservation.guest.owner)
                }) {
                    if let Ok((lobby, guest)) = reservation.accepted(b, accepted, daa) {
                        if let Some(host) = self
                            .wallet
                            .records
                            .iter()
                            .find(|r| {
                                r.set.id().ok() == Some(local_offer.code.queue.id)
                                    && r.set.generation() == local_offer.code.queue.generation
                                    && r.set.idle()
                                    && r.phase == Phase::Available
                            })
                            .map(|r| r.set.clone())
                        {
                            let mut reservation=reservation;reservation.host=local_offer.code.clone();
                            self.sessions.push(Session {
                                id,
                                reservation,
                                creation: tx.clone(),
                                creation_inputs: accepted.inputs.clone(),
                                host_set: Some(host.clone()),
                                stage: Some(QueueStage::Lobby { lobby, sets: [guest, host] }),
                                cancel_requested: false,
                                created_ms: super::client::now_ms(),
                                authorized_fees: 0,
                                paid_fees: 0,
                                recovered: false,
                                unavailable: false,
                                cursor: self.cursor,
                                anchor: self.cursor,
                                fee_budget: self.default_fee_budget,
                            });
                            changed = true;
                            changed_sessions.push(id);
                        }
                    }
                }
            }
        }
        if let Some(signed) = self.signed.iter().find(|s| s.authorization.transaction.id() == id).cloned() {
            if !self.confirmed.contains(&id) {
                if signed.authorization.inputs.len() != accepted.inputs.len()
                    || signed.authorization.inputs.iter().zip(&accepted.inputs).any(|(x, y)| !same_source(x, y))
                {
                    return Err("accepted authorization source differs from the reviewed source".into());
                }
                for i in 0..tx.inputs.len() {
                    argent_runtime::execute_input_with_covenants(tx, accepted.inputs.clone(), i)?;
                }
                for mut set in signed.authorization.successors {
                    for c in &mut set.lanes {
                        c.utxo.block_daa_score = daa;
                    }
                    set.validate(b)?;
                    self.upsert(set);
                }
                match signed.effect {
                    Effect::Wallet { closes } => {
                        for r in &mut self.wallet.records {
                            if closes.contains(&r.set.id()?) {
                                r.phase = Phase::Closed;
                            }
                        }
                    }
                    Effect::Game { session } => {
                        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session) {
                            s.paid_fees = s.paid_fees.checked_add(signed.fee).ok_or("paid fee overflow")?;
                        }
                    }
                }
                self.confirmed.insert(id);
                self.conflicted.remove(&id);
                changed = true;
            }
        }
        for signed in &self.signed {
            let own = signed.authorization.transaction.id();
            if own != id
                && !self.confirmed.contains(&own)
                && !self.conflicted.contains(&own)
                && signed
                    .authorization
                    .transaction
                    .inputs
                    .iter()
                    .any(|i| tx.inputs.iter().any(|j| i.previous_outpoint == j.previous_outpoint))
            {
                self.conflicted.insert(own);
                changed = true;
            }
        }
        changed |= self.conflict_descendants();
        if changed {
            self.update_records(&changed_sessions);
            for session_id in changed_sessions {
                let session=self.session(session_id)?;
                if let Some(stage)=session.stage.as_ref(){
                    let previous=before.iter().find(|(s,_)|*s==session_id).map(|(_,stage)|stage.clone()).unwrap_or(super::tracker::Stage::AwaitingLobby);
                    let mut event=super::audit::GameEvent::between(&previous,&stage.display(),id,daa);
                    if bank_restorations.contains(&session_id) { event.action = "restore_bank".into(); }
                    self.recent_transactions.push_back(super::audit::TransactionRecord::for_game(session_id,&self.wallet.network,event,Some(block),self.reorgs,super::audit::TransactionStatus::Accepted));
                }
            }
        }
        Ok(changed)
    }
    fn upsert(&mut self, set: Set) {
        let phase = if set.idle() { Phase::Available } else { Phase::Reserved(set.lanes[0].state.game) };
        if let Some(r) = self.wallet.records.iter_mut().find(|r| r.set.id().ok() == set.id().ok()) {
            *r = Record { set, phase };
        } else {
            self.wallet.records.push(Record { set, phase });
        }
    }
    fn update_records(&mut self, ids: &[Hash]) {
        for s in self
            .sessions
            .iter()
            .filter(|s| ids.contains(&s.id) && s.side(&self.wallet.owner) == 1 && matches!(s.stage, Some(QueueStage::Cancelled(_))))
        {
            if let Some(r) = self.wallet.records.iter_mut().find(|r| r.set.id().ok() == Some(s.reservation.host.queue.id)) {
                if r.set.idle() {
                    r.phase = Phase::Available;
                }
            }
        }
        let owner = &self.wallet.owner;
        let changes = self
            .sessions
            .iter()
            .filter(|s| ids.contains(&s.id))
            .filter_map(|s| s.stage.as_ref())
            .flat_map(|stage| {
                stage.sets().into_iter().filter(|set| set.lanes[0].state.queue.owner == *owner).map(move |set| {
                    let phase = match stage {
                        QueueStage::Lobby { lobby, .. } | QueueStage::Invitation{lobby,..} => Phase::Reserved(lobby.utxo.covenant_id.unwrap()),
                        QueueStage::Live(_) => Phase::Active(set.lanes[0].state.game),
                        QueueStage::Collecting { .. } => Phase::Settling(set.lanes[0].state.game),
                        _ if set.lanes[0].state.restoring => Phase::Settling(set.lanes[0].state.game),
                        _ => Phase::Available,
                    };
                    (set.clone(), phase)
                })
            })
            .collect::<Vec<_>>();
        for (set, phase) in changes {
            // A completed older game must never overwrite a newer generation.
            if let Some(r) = self.wallet.records.iter_mut().find(|r| r.set.id().ok() == set.id().ok()) {
                if r.set.generation() > set.generation() {
                    continue;
                }
                if r.set.generation() == set.generation()
                    && r.set.lanes[0].outpoint != set.lanes[0].outpoint
                    && set.idle()
                    && !r.set.idle()
                    && !r.set.lanes[0].state.restoring
                {
                    continue;
                }
                r.set = set;
                r.phase = phase;
            } else {
                self.wallet.records.push(Record { set, phase });
            }
        }
    }
    pub fn all_cells(&self) -> Vec<(TransactionOutpoint, UtxoEntry)> {
        self.wallet
            .records
            .iter()
            .filter(|r| r.phase != Phase::Closed && r.phase != Phase::Unavailable)
            .flat_map(|r| r.set.lanes.iter().map(|c| (c.outpoint, c.utxo.clone())))
            .chain(
                self.sessions
                    .iter()
                    .filter(|s| {
                        !s.recovered
                            && !s.unavailable
                            && !(s.cancel_requested
                                && s.side(&self.wallet.owner) == 1
                                && matches!(s.stage, Some(QueueStage::Lobby { .. })))
                    })
                    .filter_map(|s| s.stage.as_ref())
                    .flat_map(QueueStage::cells),
            )
            .collect()
    }
    pub fn owned_cells(&self) -> Vec<(TransactionOutpoint, UtxoEntry)> {
        self.wallet
            .records
            .iter()
            .filter(|r| !matches!(r.phase, Phase::Closed | Phase::Unavailable | Phase::Legacy))
            .flat_map(|r| r.set.lanes.iter().map(|c| (c.outpoint, c.utxo.clone())))
            .collect()
    }
    /// Scripts that provide positive recovery proofs, including a completed
    /// reset even when its historical game transactions have been pruned.
    pub fn recovery_scripts(&self, b: &TxBuilder<'_>) -> Result<Vec<kaspa_consensus_core::tx::ScriptPublicKey>> {
        let mut scripts = self.all_cells().into_iter().map(|(_, u)| u.script_public_key).collect::<Vec<_>>();
        scripts.extend(self.wallet.records.iter().flat_map(|r| r.set.lanes.iter().map(|c| c.utxo.script_public_key.clone())));
        for p in self.pending() {
            scripts.extend(p.authorization.transaction.outputs.iter().map(|o| o.script_public_key.clone()));
            scripts.extend(p.authorization.inputs.iter().map(|u| u.script_public_key.clone()));
        }
        for s in self.sessions.iter().filter(|s| !s.finished() || s.stage.as_ref().and_then(|stage| stage.restoring_set(&self.wallet.owner)).is_some()) {
            let side = s.side(&self.wallet.owner);
            let original = if side == 1 {
                s.host_set.as_ref().ok_or("saved host set is missing")?
            } else {
                let Some(r) = self.wallet.records.iter().find(|r| r.set.id().ok() == Some(s.reservation.guest.id)) else {
                    continue;
                };
                &r.set
            };
            let mut offer = s.reservation.offer()?;
            if side==1 {offer.generations[1]=original.generation();}
            let game = s.creation.outputs[0].covenant.ok_or("saved reservation lacks game identity")?.covenant_id;
            for lane in &original.lanes {
                let mut state = lane.state.reset(game, offer.generations[side] + 1, side, 0, false);
                scripts.push(b.covenant_utxo(state.actor(), state.state(), 1, 0, false, Some(original.id()?))?.script_public_key);
                if state.queue.lane == 0 {
                    state.restoring = true;
                    scripts.push(b.covenant_utxo(state.actor(), state.state(), 1, 0, false, Some(original.id()?))?.script_public_key);
                }
            }
        }
        Ok(scripts)
    }
    /// Absence never releases a lease. Positive exact UTXOs can prove an
    /// unchanged actor, an authorized wallet operation, or an idle reset.
    /// Unresolved games retain their sources and can be replayed independently.
    pub fn bootstrap(&mut self, cursor: Hash, daa: u64, found: &[Funding]) -> Result<()> {
        let a = crate::reusable::artifact()?;
        let b = TxBuilder::new(&a)?;
        let pending = self.pending().into_iter().cloned().collect::<Vec<_>>();
        for p in pending {
            if !matches!(p.effect, Effect::Wallet { .. }) {
                continue;
            }
            let next = p.authorization.successors.iter().map(|s| accepted_set(s, found)).collect::<Option<Vec<_>>>();
            let proof = if p.authorization.successors.is_empty() {
                p.authorization.transaction.outputs.iter().enumerate().all(|(i, o)| {
                    found.iter().any(|f| {
                        f.outpoint == TransactionOutpoint::new(p.authorization.transaction.id(), i as u32)
                            && f.utxo.amount == o.value
                            && f.utxo.script_public_key == o.script_public_key
                            && f.utxo.covenant_id == o.covenant.map(|c| c.covenant_id)
                            && f.utxo.block_daa_score != u64::MAX
                    })
                })
            } else {
                next.is_some()
            };
            if proof {
                for set in next.unwrap_or_default() {
                    set.validate(&b)?;
                    self.upsert(set);
                }
                if let Effect::Wallet { closes } = p.effect {
                    for r in &mut self.wallet.records {
                        if closes.contains(&r.set.id()?) {
                            r.phase = Phase::Closed;
                        }
                    }
                }
                self.confirmed.insert(p.authorization.transaction.id());
                self.conflicted.remove(&p.authorization.transaction.id());
            }
        }
        for r in &mut self.wallet.records {
            if matches!(r.phase,Phase::Closed|Phase::Legacy) {
                continue;
            }
            if let Some(set) = accepted_set(&r.set, found) {
                r.set = set;
                if r.phase == Phase::Unavailable && r.set.validate(&b).is_ok() {
                    if r.set.idle() { r.phase = Phase::Available; }
                    else if r.set.lanes[0].state.restoring { r.phase = Phase::Settling(r.set.lanes[0].state.game); }
                }
            } else {
                r.phase = Phase::Unavailable;
            }
        }
        for s in &mut self.sessions {
            if let Some(stage) = &mut s.stage {
                if let Some(waiting) = stage.restoring_set(&self.wallet.owner) {
                    let mut states = waiting.lanes.each_ref().map(|c| c.state.clone());
                    states[0].restoring = false;
                    if let Some(restored) = find_reset_set(&b, found, waiting.id()?, &states)? {
                        let restored_id = restored.lanes[0].outpoint.transaction_id;
                        for p in &self.signed {
                            let tx = &p.authorization.transaction;
                            if matches!(p.effect, Effect::Game { session } if session == s.id)
                                && p.authorization.purpose == "restore queue Bank"
                                && tx.inputs.iter().any(|i| i.previous_outpoint == waiting.lanes[0].outpoint)
                            {
                                if tx.id() == restored_id {
                                    if self.confirmed.insert(restored_id) {
                                        s.paid_fees = s.paid_fees.checked_add(p.fee).ok_or("paid fee overflow")?;
                                    }
                                    self.conflicted.remove(&restored_id);
                                } else if !self.confirmed.contains(&tx.id()) {
                                    self.conflicted.insert(tx.id());
                                }
                            }
                        }
                        if let Some(record) = self.wallet.records.iter_mut().find(|r| r.set.id().ok() == restored.id().ok()
                            && r.set.generation() == restored.generation() && r.set.lanes[0].outpoint == waiting.lanes[0].outpoint)
                        {
                            record.set = restored.clone();
                            record.phase = Phase::Available;
                        }
                        stage.replace_restored(restored);
                        s.recovered |= matches!(stage, QueueStage::RecoveredReset(_));
                        s.unavailable = false;
                        continue;
                    }
                    if accepted_set(waiting, found).is_some() { s.unavailable = false; }
                }
            }
            if s.finished() && !s.recovered {
                // A deep reorg can invalidate a previously accepted terminal
                // snapshot. Only that game's latest generation is affected.
                let mut offer = s.reservation.offer()?;
                let side = s.side(&self.wallet.owner);
                if side==1{let host=s.host_set.as_ref().ok_or("saved host set missing")?;offer.pools[1]=host.id()?;offer.generations[1]=host.generation();}
                if self.wallet.records.iter().any(|r| {
                    r.set.id().ok() == Some(offer.pools[side])
                        && r.set.generation() <= offer.generations[side] + 1
                        && r.phase == Phase::Unavailable
                }) {
                    s.unavailable = true;
                }
                continue;
            }
            let side = s.side(&self.wallet.owner);
            let mut offer = s.reservation.offer()?;
            if side==1 {let host=s.host_set.as_ref().ok_or("saved host set missing")?;offer.pools[1]=host.id()?;offer.generations[1]=host.generation();}
            let pool = offer.pools[side];
            let game = s.creation.outputs[0].covenant.ok_or("missing game identity")?.covenant_id;
            let Some(record) = self.wallet.records.iter_mut().find(|r| r.set.id().ok() == Some(pool)) else {
                continue;
            };
            if record.set.generation() > offer.generations[side] + 1
                || (record.set.generation() == offer.generations[side] + 1 && !record.set.idle())
            {
                continue;
            }
            let mut states =
                std::array::from_fn::<_, 3, _>(|i| record.set.lanes[i].state.reset(game, offer.generations[side] + 1, side, 0, false));
            let mut reset = find_reset_set(&b, found, pool, &states)?;
            if reset.is_none() {
                states[0].restoring = true;
                reset = find_reset_set(&b, found, pool, &states)?;
            }
            if let Some(set) = reset {
                let idle = set.idle();
                record.phase = if idle { Phase::Available } else { Phase::Settling(game) };
                record.set = set.clone();
                s.recovered = idle;
                if !idle { s.stage = Some(QueueStage::RecoveredReset(set)); }
                s.unavailable = false;
                // This native, generation-bound reset proves old game spends
                // obsolete; no score or transaction acceptance is invented.
                for p in &self.signed {
                    if matches!(p.effect,Effect::Game{session} if session==s.id)
                        && !self.confirmed.contains(&p.authorization.transaction.id())
                    {
                        self.conflicted.insert(p.authorization.transaction.id());
                    }
                }
            } else {
                s.recovered = false;
                let coherent =
                    if let Some(stage) = &s.stage {
                        stage
                            .cells()
                            .iter()
                            .all(|(out, u)| found.iter().any(|f| f.outpoint == *out && super::tracker::same_utxo(&f.utxo, u)))
                    } else {
                        s.creation.inputs.iter().zip(&s.creation_inputs).all(|(i, u)| {
                            found.iter().any(|f| f.outpoint == i.previous_outpoint && super::tracker::same_utxo(&f.utxo, u))
                        })
                    };
                s.unavailable = !coherent;
                if !coherent && !(side == 1 && record.set.idle() && record.phase == Phase::Available) {
                    record.phase = Phase::Unavailable;
                }
                if !coherent && side == 1 && record.set.idle() {
                    s.cancel_requested = true;
                    for offer in &mut self.offers {
                        if offer.code == s.reservation.host {
                            offer.available = false;
                        }
                    }
                }
            }
        }
        self.refresh_pending();
        self.cursor = cursor;
        self.daa = daa;
        self.history.clear();
        Ok(())
    }
    pub fn restore_session(
        &mut self,
        id: Hash,
        stage: QueueStage,
        cursor: Hash,
        confirmed: Vec<Hash>,
        conflicted: Vec<Hash>,
    ) -> Result<()> {
        if cursor != self.cursor {
            return Err("chain advanced during game recovery; retrying".into());
        }
        let before = self.state();
        let previous = self
            .signed
            .iter()
            .filter(|s| matches!(s.effect,Effect::Game{session} if session==id))
            .map(|s| {
                let tx = s.authorization.transaction.id();
                (tx, self.confirmed.contains(&tx), self.conflicted.contains(&tx), s.fee)
            })
            .collect::<Vec<_>>();
        let s = self.sessions.iter_mut().find(|s| s.id == id).ok_or("unknown recovered game")?;
        if !s.unavailable {
            return Ok(());
        }
        s.stage = Some(stage);
        s.unavailable = false;
        s.cursor = cursor;
        for (tx, was_confirmed, _, fee) in &previous {
            if confirmed.contains(tx) {
                self.confirmed.insert(*tx);
                self.conflicted.remove(tx);
                if !*was_confirmed {
                    s.paid_fees = s.paid_fees.checked_add(*fee).ok_or("paid fee overflow")?;
                }
            } else {
                self.confirmed.remove(tx);
                if *was_confirmed {
                    s.paid_fees = s.paid_fees.saturating_sub(*fee);
                }
                if conflicted.contains(tx) {
                    self.conflicted.insert(*tx);
                } else {
                    self.conflicted.remove(tx);
                }
            }
        }
        self.conflict_descendants();
        self.update_records(&[id]);
        if let Some(last) = self.history.back_mut() {
            if last.before.is_none() {
                last.before = Some(before);
            }
            for (tx, c, f, _) in previous {
                if !last.status.iter().any(|(old, ..)| *old == tx) {
                    last.status.push((tx, c, f));
                }
            }
        }
        self.refresh_pending();
        Ok(())
    }
}
fn find_reset_set(b: &TxBuilder<'_>, found: &[Funding], pool: Hash, states: &[crate::reusable::Lane; 3]) -> Result<Option<Set>> {
    let mut lanes = Vec::with_capacity(3);
    for state in states {
        let script = b.covenant_utxo(state.actor(), state.state(), 1, 0, false, Some(pool))?.script_public_key;
        let mut matches = found.iter().filter(|f| f.utxo.script_public_key == script
            && f.utxo.covenant_id == Some(pool) && f.utxo.block_daa_score != u64::MAX);
        let Some(coin) = matches.next() else { return Ok(None); };
        if matches.next().is_some() { return Ok(None); }
        lanes.push(Cell { outpoint: coin.outpoint, utxo: coin.utxo.clone(), state: state.clone() });
    }
    let set = Set { lanes: lanes.try_into().map_err(|_| "wrong reset queue count")? };
    set.validate(b)?;
    Ok(Some(set))
}

fn accepted_set(set: &Set, found: &[Funding]) -> Option<Set> {
    let mut set = set.clone();
    for c in &mut set.lanes {
        c.utxo = found
            .iter()
            .find(|f| f.outpoint == c.outpoint && same_source(&f.utxo, &c.utxo) && f.utxo.block_daa_score != u64::MAX)?
            .utxo
            .clone();
    }
    Some(set)
}
pub(super) fn same_source(a: &UtxoEntry, b: &UtxoEntry) -> bool {
    a.amount == b.amount
        && a.script_public_key == b.script_public_key
        && a.covenant_id == b.covenant_id
        && a.is_coinbase == b.is_coinbase
}

#[cfg(test)]
mod tests;
