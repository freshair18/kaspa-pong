//! Public reorganization diagnostics. No private keys or signing material.
use super::super::engine::Result;
use super::tracker::{ChainBlock, Invitation, Stage, Tracker};
use kaspa_consensus_core::Hash;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GameEvent {
    pub txid: Hash,
    pub accepted_daa: Option<u64>,
    pub action: String,
    /// 0 = left, 1 = right, when known.
    pub side: Option<i64>,
    /// Signed step count: negative = up, positive = down, only for movement transactions.
    pub direction: Option<i64>,
    #[serde(default)]
    pub payment: Option<super::fees::Payment>,
}
impl GameEvent {
    pub fn between(before: &Stage, after: &Stage, txid: Hash, daa: u64) -> Self {
        let mut event = Self { txid, accepted_daa: Some(daa), action: "unknown".into(), side: None, direction: None, payment: None };
        event.action = match (before, after) {
            (Stage::AwaitingLobby, _) => "genesis",
            (Stage::Lobby(_), Stage::Live(_)) => "start",
            (Stage::Lobby(_), Stage::Cancelled) => "cancel_lobby",
            (Stage::Live(a), Stage::Live(b)) if a.court.outpoint == b.court.outpoint => {
                if let Some(side) = (0..a.players.len()).find(|&i| a.players[i].outpoint != b.players[i].outpoint) {
                    event.side = Some(b.players[side].state.side);
                    let queue = &b.players[side].state;
                    event.direction = queue.count.checked_sub(1).and_then(|i| queue.commands.get(i).copied());
                }
                "move"
            }
            (Stage::Live(_), Stage::Collecting(s)) if s.collector.state.finish.winner < 0 => "cancel_game",
            (Stage::Live(s), Stage::Collecting(result)) if !s.court.state.ended() => {
                event.side = Some(result.collector.state.finish.winner);
                "timeout"
            }
            (Stage::Live(_), Stage::Collecting(_)) => "finish",
            (Stage::Live(a), Stage::Live(b)) if a.court.state.resolves == b.court.state.resolves => {
                event.side = (0..2).find(|&i| a.players[i].outpoint != b.players[i].outpoint).map(|i| i as i64);
                "checkpoint"
            }
            (Stage::Live(a), Stage::Live(b)) => {
                event.side = Some(1 - b.court.state.resolver);
                if b.court.state.takeovers > a.court.state.takeovers { "takeover" } else { "resolve" }
            }
            (Stage::Collecting(_), Stage::Reclaimed(_)) => "refund",
            (Stage::Collecting(a), Stage::Collecting(b)) => {
                if let Some(i) = (0..a.players.len()).find(|&i| a.players[i].outpoint != b.players[i].outpoint) {
                    event.side = Some(b.players[i].state.side);
                    let q = &b.players[i].state;
                    event.direction = q.count.checked_sub(1).and_then(|j| q.commands.get(j).copied());
                }
                "move"
            }
            _ => "unknown",
        }
        .into();
        event
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionStatus {
    Accepted,
    Reverted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransactionRecord {
    pub version: u32,
    pub id: String,
    pub observed_unix_ms: u64,
    pub game: Hash,
    pub network: String,
    pub status: TransactionStatus,
    pub accepting_block: Option<Hash>,
    #[serde(flatten)]
    pub event: GameEvent,
}
impl TransactionRecord {
    pub fn new(invite: &Invitation, event: GameEvent, block: Option<Hash>, generation: u64, status: TransactionStatus) -> Self {
        let identity = serde_json::to_vec(&(invite.genesis.id(), &invite.network, block, event.txid, generation, status))
            .expect("record identity serializes");
        Self {
            version: 1,
            id: blake2b_simd::Params::new().hash_length(32).hash(&identity).to_hex().to_string(),
            observed_unix_ms: super::client::now_ms(),
            game: invite.genesis.id(),
            network: invite.network.clone(),
            status,
            accepting_block: block,
            event,
        }
    }
    pub fn terminal_lines(&self) -> String {
        let status = if self.status == TransactionStatus::Accepted { "ACCEPTED" } else { "REVERTED" };
        let side = match self.event.side {
            Some(0) => " LEFT",
            Some(1) => " RIGHT",
            _ => "",
        };
        let direction = match self.event.direction {
            Some(-1) => " UP".to_string(),
            Some(1) => " DOWN".to_string(),
            Some(steps) if steps < 0 => format!(" UP x{}", steps.unsigned_abs()),
            Some(steps) if steps > 0 => format!(" DOWN x{steps}"),
            _ => String::new(),
        };
        let daa = self.event.accepted_daa.map(|d| d.to_string()).unwrap_or_else(|| "unknown".into());
        format!("{status} {}{side}{direction} | DAA {daa}\n{}", self.event.action.to_uppercase(), self.event.txid)
    }
}
/// Publish net accepted-game changes, not intermediate rollback/replay steps.
pub fn net_transaction_changes(records: Vec<TransactionRecord>) -> Vec<TransactionRecord> {
    let mut bounds = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        bounds.entry(record.event.txid).and_modify(|pair: &mut (usize, usize)| pair.1 = index).or_insert((index, index));
    }
    let mut keep = HashSet::new();
    for (first, last) in bounds.into_values() {
        let before = &records[first];
        let after = &records[last];
        if before.status == TransactionStatus::Reverted {
            if after.status == TransactionStatus::Accepted && before.event.accepted_daa == after.event.accepted_daa {
                continue;
            }
            keep.insert(first);
        }
        if after.status == TransactionStatus::Accepted {
            keep.insert(last);
        }
    }
    records.into_iter().enumerate().filter_map(|(index, record)| keep.contains(&index).then_some(record)).collect()
}

pub fn transaction_log_path(state: &Path) -> PathBuf {
    state.with_extension("transactions.jsonl")
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reaccepted {
    pub txid: Hash,
    pub old_daa: Option<u64>,
    pub new_daa: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReorgEvent {
    pub version: u32,
    pub id: String,
    pub observed_unix_ms: u64,
    pub game: Hash,
    pub network: String,
    pub old_cursor: Hash,
    pub new_cursor: Hash,
    pub common_ancestor: Option<Hash>,
    pub old_selected_daa: Option<u64>,
    pub common_ancestor_daa: Option<u64>,
    /// Old selected tip minus the common ancestor, not a count of blocks.
    pub removed_daa_span: Option<u64>,
    pub removed_blocks: Vec<Hash>,
    pub added_blocks: Vec<(Hash, u64)>,
    pub removed_game_transactions: Vec<GameEvent>,
    pub reaccepted: Vec<Reaccepted>,
    pub missing_game_transactions: Vec<Hash>,
    /// False when retained undo metadata cannot fully describe a deep reorganization.
    pub history_complete: bool,
    pub discarded_local_inputs: usize,
    pub pending_local_transaction: Option<Hash>,
}
impl ReorgEvent {
    /// Changed blocks alone are irrelevant: game acceptance must be lost or
    /// its DAA changed. Incomplete history alone proves neither.
    pub fn affects_game(&self) -> bool {
        !self.missing_game_transactions.is_empty()
            || self.reaccepted.iter().any(|event| event.old_daa.is_some_and(|old| old != event.new_daa))
    }
    /// Evaluate the entire catch-up: a transaction missing in one RPC batch
    /// can be reaccepted unchanged in a later batch.
    pub fn settle(reports: Vec<Self>, records: &[TransactionRecord], accepted: &HashSet<Hash>) -> Option<Self> {
        let mut reports = reports.into_iter();
        let mut report = reports.next()?;
        let mut original = HashMap::new();
        for event in &report.removed_game_transactions {
            original.entry(event.txid).or_insert_with(|| event.clone());
        }
        for other in reports {
            report.new_cursor = other.new_cursor;
            report.history_complete &= other.history_complete;
            for hash in other.removed_blocks {
                if !report.removed_blocks.contains(&hash) {
                    report.removed_blocks.push(hash);
                }
            }
            for block in other.added_blocks {
                if !report.added_blocks.iter().any(|(hash, _)| *hash == block.0) {
                    report.added_blocks.push(block);
                }
            }
            if other.common_ancestor_daa < report.common_ancestor_daa {
                report.common_ancestor = other.common_ancestor;
                report.common_ancestor_daa = other.common_ancestor_daa;
            }
            for event in other.removed_game_transactions {
                original.entry(event.txid).or_insert(event);
            }
        }
        report.removed_daa_span =
            report.old_selected_daa.zip(report.common_ancestor_daa).map(|(old, common)| old.saturating_sub(common));
        report.removed_game_transactions.clear();
        report.reaccepted.clear();
        report.missing_game_transactions.clear();
        let mut ordered: Vec<_> = original.into_values().collect();
        ordered.sort_by_key(|event| (event.accepted_daa, event.txid));
        for mut event in ordered {
            let mut changes = records.iter().filter(|record| record.event.txid == event.txid);
            let Some(first) = changes.next() else {
                // Deep replay can lose undo metadata. The durable accepted-fee
                // ledger still proves this game transaction previously existed.
                if !accepted.contains(&event.txid) {
                    report.missing_game_transactions.push(event.txid);
                    report.removed_game_transactions.push(event);
                }
                continue;
            };
            // Events first discovered during this catch-up were never
            // previously published, so replaying them is not a game rollback.
            if first.status != TransactionStatus::Reverted {
                continue;
            }
            event.accepted_daa = first.event.accepted_daa;
            let last = changes.next_back().unwrap_or(first);
            if last.status == TransactionStatus::Accepted {
                let Some(new_daa) = last.event.accepted_daa else { continue };
                if event.accepted_daa.is_none_or(|old| old == new_daa) {
                    continue;
                }
                report.reaccepted.push(Reaccepted { txid: event.txid, old_daa: event.accepted_daa, new_daa });
            } else {
                report.missing_game_transactions.push(event.txid);
            }
            report.removed_game_transactions.push(event);
        }
        report.affects_game().then_some(report)
    }
    pub fn capture(invite: &Invitation, tracker: &Tracker, removed: &[Hash], added: &[ChainBlock], common: Option<Hash>) -> Self {
        let mut events = Vec::new();
        let mut complete = true;
        for hash in removed {
            if let Some(undo) = tracker.history.iter().find(|h| h.hash == *hash) {
                for txid in &undo.transactions {
                    if let Some(event) = undo.events.iter().find(|e| e.txid == *txid) {
                        events.push(event.clone());
                    } else {
                        complete = false;
                        events.push(GameEvent {
                            payment: None,
                            txid: *txid,
                            accepted_daa: undo.daa,
                            action: "unknown".into(),
                            side: None,
                            direction: None,
                        });
                    }
                }
            } else {
                complete = false;
            }
        }
        let common =
            common.or_else(|| removed.last().and_then(|hash| tracker.history.iter().find(|h| h.hash == *hash).map(|h| h.parent)));
        let old_daa = tracker.history.back().and_then(|h| h.daa);
        let common_daa = common.and_then(|hash| tracker.history.iter().find(|h| h.hash == hash).and_then(|h| h.daa));
        let mut reaccepted = Vec::new();
        let mut missing = Vec::new();
        for event in &events {
            if let Some(block) = added.iter().find(|b| b.transactions.iter().any(|t| t.tx.id() == event.txid)) {
                reaccepted.push(Reaccepted { txid: event.txid, old_daa: event.accepted_daa, new_daa: block.daa });
            } else {
                missing.push(event.txid);
            }
        }
        let new_cursor = added.last().map(|b| b.hash).or(common).unwrap_or(tracker.cursor);
        let observed_unix_ms = super::client::now_ms();
        let identity =
            serde_json::to_vec(&(invite.genesis.id(), tracker.cursor, removed, new_cursor, tracker.reorgs, observed_unix_ms))
                .expect("hashes serialize");
        let id = blake2b_simd::Params::new().hash_length(32).hash(&identity).to_hex().to_string();
        Self {
            version: 1,
            id,
            observed_unix_ms,
            game: invite.genesis.id(),
            network: invite.network.clone(),
            old_cursor: tracker.cursor,
            new_cursor,
            common_ancestor: common,
            old_selected_daa: old_daa,
            common_ancestor_daa: common_daa,
            removed_daa_span: old_daa.zip(common_daa).and_then(|(old, ancestor)| old.checked_sub(ancestor)),
            removed_blocks: removed.to_vec(),
            added_blocks: added.iter().map(|b| (b.hash, b.daa)).collect(),
            removed_game_transactions: events,
            reaccepted,
            missing_game_transactions: missing,
            history_complete: complete,
            discarded_local_inputs: 0,
            pending_local_transaction: None,
        }
    }
    /// Emphasize three lost/retimed game actions, or a game acceptance time
    /// shifted by at least 20 DAA. Global chain depth alone is irrelevant.
    pub fn is_big_game_rollback(&self) -> bool {
        let changed: HashSet<_> = self
            .missing_game_transactions
            .iter()
            .copied()
            .chain(self.reaccepted.iter().filter(|event| event.old_daa.is_some_and(|old| old != event.new_daa)).map(|event| event.txid))
            .collect();
        changed.len() >= 3 || self.reaccepted.iter().any(|event| event.old_daa.is_some_and(|old| old.abs_diff(event.new_daa) >= 20))
    }
    pub fn notice(&self) -> String {
        let changed: Vec<_> = self.reaccepted.iter().filter(|event| event.old_daa.is_some_and(|old| old != event.new_daa)).collect();
        let example = changed
            .first()
            .map(|event| {
                format!("; DAA {} -> {}", event.old_daa.map(|daa| daa.to_string()).unwrap_or_else(|| "unknown".into()), event.new_daa)
            })
            .unwrap_or_default();
        let incomplete = if self.history_complete { "" } else { "; details incomplete/unknown" };
        let label = if self.is_big_game_rollback() { "Game rollback" } else { "Reorg" };
        format!(
            "{label}: {} tx removed; {} retimed{example}{incomplete}",
            self.missing_game_transactions.len(),
            changed.len()
        )
    }
}

pub fn log_path(state: &Path) -> PathBuf {
    state.with_extension("reorgs.jsonl")
}

/// Replaying a durable outbox after a crash must not duplicate log entries.
pub struct ReorgLog {
    file: File,
    known: HashSet<String>,
    failed: bool,
}
impl ReorgLog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).read(true).append(true).open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let mut known = HashSet::new();
        for line in bytes[..end].split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
            #[derive(Deserialize)]
            struct Identity {
                id: String,
            }
            let event: Identity = serde_json::from_slice(line).map_err(|e| format!("invalid event log {}: {e}", path.display()))?;
            if event.id.is_empty() {
                return Err("event log record has an empty identity".into());
            }
            known.insert(event.id);
        }
        // A crash during the final write can leave an incomplete JSON line.
        // Complete preceding records remain intact; the outbox supplies a retry.
        if end != bytes.len() {
            // Windows append handles cannot truncate. Use a separate ordinary
            // write handle only for repairing the incomplete trailing record.
            let repair = OpenOptions::new().write(true).open(path)?;
            repair.set_len(end as u64)?;
            repair.sync_data()?;
        }
        Ok(Self { file, known, failed: false })
    }
    pub fn append(&mut self, event: &ReorgEvent) -> Result<()> {
        self.append_record(&event.id, event).map(|_| ())
    }
    /// Returns true only when a new record was written, for terminal emission.
    pub fn append_record(&mut self, id: &str, event: &impl Serialize) -> Result<bool> {
        if self.failed {
            return Err("event log write failed; restart the client after checking storage".into());
        }
        if self.known.contains(id) {
            return Ok(false);
        }
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        if let Err(error) = self.file.write_all(&line).and_then(|_| self.file.sync_data()) {
            // Do not append behind a potentially incomplete record. The durable
            // outbox and open() recover it on restart.
            self.failed = true;
            return Err(error.into());
        }
        self.known.insert(id.to_string());
        Ok(true)
    }
}
