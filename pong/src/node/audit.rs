//! Public reorganization diagnostics. No private keys or signing material.
use super::tracker::Stage;
use kaspa_consensus_core::Hash;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GameEvent {
    pub txid: Hash,
    pub accepted_daa: Option<u64>,
    pub action: String,
    /// 0 = left, 1 = right, when known.
    pub side: Option<i64>,
    /// Signed step count: negative = up, positive = down, only for movement transactions.
    pub direction: Option<i64>,
}
impl GameEvent {
    pub fn between(before: &Stage, after: &Stage, txid: Hash, daa: u64) -> Self {
        let mut event = Self { txid, accepted_daa: Some(daa), action: "unknown".into(), side: None, direction: None };
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
            (Stage::Live(_), Stage::Collecting(result))
                if result.collector.state.finish.left_score < result.collector.state.finish.win_score
                    && result.collector.state.finish.right_score < result.collector.state.finish.win_score =>
            {
                event.side = Some(result.collector.state.finish.winner);
                "timeout"
            }
            (Stage::Live(_), Stage::Collecting(_)) => "finish",
            (Stage::Live(a), Stage::Live(b)) if a.court.state.resolves == b.court.state.resolves => {
                event.side = (0..2).find(|&i| a.players[i].outpoint != b.players[i].outpoint).map(|i| i as i64);
                "checkpoint"
            }
            (Stage::Live(_), Stage::Live(b)) => {
                event.side = Some(1 - b.court.state.resolver);
                "resolve"
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
    pub fn for_game(
        game: Hash,
        network: &str,
        event: GameEvent,
        block: Option<Hash>,
        generation: u64,
        status: TransactionStatus,
    ) -> Self {
        let identity =
            serde_json::to_vec(&(game, network, block, event.txid, generation, status)).expect("record identity serializes");
        Self {
            version: 1,
            id: blake2b_simd::Params::new().hash_length(32).hash(&identity).to_hex().to_string(),
            observed_unix_ms: super::client::now_ms(),
            game,
            network: network.into(),
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
    /// Emphasize three lost/retimed game actions, or a game acceptance time
    /// shifted by at least 20 DAA. Global chain depth alone is irrelevant.
    pub fn is_big_game_rollback(&self) -> bool {
        let changed: HashSet<_> = self
            .missing_game_transactions
            .iter()
            .copied()
            .chain(
                self.reaccepted.iter().filter(|event| event.old_daa.is_some_and(|old| old != event.new_daa)).map(|event| event.txid),
            )
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
        format!("{label}: {} tx removed; {} retimed{example}{incomplete}", self.missing_game_transactions.len(), changed.len())
    }
}
