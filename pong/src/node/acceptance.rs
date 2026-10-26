//! Independently validated, ephemeral peer views. The coordinator's Cache is
//! never replaced here: reservations, signing and settlement remain primary-owned.
use super::{
    Config,
    client::{Cache, now_ms},
    pipeline::Follower,
    rpc::{Node, broadcast::Pool},
    tracker::{Stage, Tracker},
};
use argent_runtime::Artifact;
use kaspa_consensus_core::Hash;
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::oneshot, task::JoinHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Relation {
    Same,
    Ahead,
    Behind,
    Conflict,
    Unrelated,
}
const PEER_LIFETIME: Duration = Duration::from_secs(2);

fn secondary_seed(primary: &Cache, invite: &super::tracker::Invitation) -> Cache {
    // Carry only replayable chain state. Never clone reservations, fee ownership
    // or durable outboxes into a secondary. A two-second retained checkpoint
    // also lets a slightly lagging peer recognize the reader's initial cursor.
    let mut seed = Cache::new(invite);
    seed.tracker = primary.tracker.clone();
    for _ in 0..20 {
        let Some(undo) = seed.tracker.history.pop_back() else {
            break;
        };
        if let Some(before) = undo.before {
            seed.tracker.stage = before;
        }
        seed.tracker.cursor = undo.parent;
    }
    seed.tracker.daa = seed.tracker.history.back().and_then(|h| h.daa).unwrap_or(primary.tracker.daa.saturating_sub(20));
    seed
}

fn same_stage(a: &Stage, b: &Stage) -> bool {
    if std::mem::discriminant(a) != std::mem::discriminant(b) {
        return false;
    }
    let (a, b) = (a.cells(), b.cells());
    !a.is_empty() && a.len() == b.len() && a.iter().zip(b.iter()).all(|((x, u), (y, v))| x == y && super::tracker::same_utxo(u, v))
}

/// Compare ordered game acceptances *after a shared chain checkpoint*. Ordinary
/// tip lag is not a negative vote. Acceptance block/DAA differences are conflicts,
/// even when transaction IDs agree; covenant metadata cannot be mixed across views.
fn relation(a: &Tracker, b: &Tracker) -> Relation {
    if same_stage(&a.stage, &b.stage) {
        return Relation::Same;
    }
    let ancestors: HashSet<_> = std::iter::once(a.cursor).chain(a.history.iter().flat_map(|h| [h.parent, h.hash])).collect();
    let common = std::iter::once(b.cursor)
        .chain(b.history.iter().rev().map(|h| h.hash))
        .chain(b.history.front().map(|h| h.parent))
        .find(|h| ancestors.contains(h));
    let Some(common) = common else {
        return Relation::Unrelated;
    };
    fn suffix(t: &Tracker, common: Hash) -> Option<Vec<(Hash, u64, Hash)>> {
        let start = if t.cursor == common {
            t.history.len()
        } else if t.history.front().is_some_and(|h| h.parent == common) {
            0
        } else {
            t.history.iter().position(|h| h.hash == common)? + 1
        };
        Some(
            t.history
                .iter()
                .skip(start)
                .flat_map(|h| h.transactions.iter().map(move |id| (*id, h.daa.unwrap_or(0), h.hash)))
                .collect(),
        )
    }
    let (Some(left), Some(right)) = (suffix(a, common), suffix(b, common)) else {
        return Relation::Unrelated;
    };
    if left.iter().zip(&right).any(|(a, b)| a != b) {
        return Relation::Conflict;
    }
    match left.len().cmp(&right.len()) {
        std::cmp::Ordering::Less => Relation::Ahead,
        std::cmp::Ordering::Greater => Relation::Behind,
        std::cmp::Ordering::Equal => Relation::Conflict,
    }
}

struct View {
    tracker: Arc<Tracker>,
    observed: Instant,
}
#[derive(Default)]
struct Decision {
    source: Option<String>,
    conflicts: Vec<String>,
}

fn choose(primary: &Tracker, views: &HashMap<String, View>, excluded: &HashSet<String>, now: Instant) -> Decision {
    let mut eligible = Vec::new();
    let mut conflicts = Vec::new();
    for (id, view) in views {
        if excluded.contains(id) || now.saturating_duration_since(view.observed) > PEER_LIFETIME {
            continue;
        }
        // Results and payouts remain primary-owned. Preview only a live court,
        // and bound a remote clock's lead to the existing catch-up tolerance.
        if !matches!(view.tracker.stage, Stage::Live(_))
            || view.tracker.daa > primary.daa.saturating_add(5 * crate::engine::DAA_PER_SECOND as u64)
        {
            continue;
        }
        match relation(primary, &view.tracker) {
            Relation::Ahead => eligible.push(id.clone()),
            Relation::Conflict => conflicts.push(id.clone()),
            _ => {}
        }
    }
    if !conflicts.is_empty() {
        return Decision { source: None, conflicts };
    }
    eligible.sort_by(|a, b| views[b].tracker.daa.cmp(&views[a].tracker.daa).then_with(|| a.cmp(b)));
    let mut selected: Option<String> = None;
    for id in eligible {
        if let Some(previous) = &selected {
            match relation(&views[previous].tracker, &views[&id].tracker) {
                Relation::Ahead => selected = Some(id),
                Relation::Same | Relation::Behind => {}
                _ => return Decision { source: None, conflicts: vec![previous.clone(), id] },
            }
        } else {
            selected = Some(id);
        }
    }
    Decision { source: selected, conflicts: vec![] }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Cell, Snapshot, World, empty_queues};
    use crate::node::tracker::Undo;
    use kaspa_consensus_core::tx::{ScriptPublicKey, TransactionOutpoint, UtxoEntry};
    fn hash(n: u8) -> Hash {
        Hash::from_bytes([n; 32])
    }
    fn baseline() -> Tracker {
        let world = World::opening(vec![1; 32], vec![2; 32]);
        let queues = empty_queues(&world.left, &world.right);
        let entry = UtxoEntry::new(1_000_000_000, ScriptPublicKey::new(0, Default::default()), 100, false, None);
        Tracker {
            cursor: hash(1),
            daa: 100,
            reorgs: 0,
            history: Default::default(),
            stage: Stage::Live(Snapshot {
                court: Cell { outpoint: TransactionOutpoint::new(hash(2), 0), utxo: entry.clone(), state: world },
                players: std::array::from_fn(|i| Cell {
                    outpoint: TransactionOutpoint::new(hash(3 + i as u8), 0),
                    utxo: entry.clone(),
                    state: queues[i].clone(),
                }),
            }),
        }
    }
    fn extend(base: &Tracker, tx: u8, block: u8, daa: u64) -> Tracker {
        let mut next = base.clone();
        let before = next.stage.clone();
        if let Stage::Live(s) = &mut next.stage {
            s.players[0].outpoint = TransactionOutpoint::new(hash(tx), 0);
            s.players[0].utxo.block_daa_score = daa;
            s.players[0].state.y += 1;
        }
        next.history.push_back(Undo {
            hash: hash(block),
            parent: base.cursor,
            before: Some(before),
            transactions: vec![hash(tx)],
            daa: Some(daa),
            events: vec![],
        });
        next.cursor = hash(block);
        next.daa = daa;
        next
    }
    fn views(entries: Vec<(&str, Tracker)>, now: Instant) -> HashMap<String, View> {
        entries.into_iter().map(|(id, tracker)| (id.into(), View { tracker: Arc::new(tracker), observed: now })).collect()
    }
    #[test]
    fn an_unseen_transaction_is_lag_not_a_conflicting_vote() {
        let root = baseline();
        let ahead = extend(&root, 20, 30, 101);
        let now = Instant::now();
        assert_eq!(relation(&root, &ahead), Relation::Ahead);
        assert_eq!(relation(&ahead, &root), Relation::Behind);
        let decision = choose(&root, &views(vec![("fast", ahead.clone()), ("lagging", root.clone())], now), &HashSet::new(), now);
        assert_eq!(decision.source.as_deref(), Some("fast"));
        assert!(decision.conflicts.is_empty());
        assert!(choose(&ahead, &views(vec![("lagging", root)], now), &HashSet::new(), now).source.is_none());
    }
    #[test]
    fn conflicting_spends_and_acceptance_metadata_fall_back_to_primary() {
        let root = baseline();
        let a = extend(&root, 20, 30, 101);
        let b = extend(&root, 21, 31, 101);
        let now = Instant::now();
        assert_eq!(relation(&a, &b), Relation::Conflict);
        let decision = choose(&root, &views(vec![("a", a.clone()), ("b", b)], now), &HashSet::new(), now);
        assert!(decision.source.is_none());
        assert_eq!(decision.conflicts.len(), 2);
        let changed = extend(&root, 20, 31, 102);
        assert_eq!(relation(&a, &changed), Relation::Conflict);
        let decision = choose(&a, &views(vec![("other", changed)], now), &HashSet::new(), now);
        assert!(decision.source.is_none());
        assert_eq!(decision.conflicts, vec!["other"]);
    }
    #[test]
    fn select_a_coherent_extension_and_never_merge_two_peer_branches() {
        let root = baseline();
        let a = extend(&root, 20, 30, 101);
        let b = extend(&a, 21, 31, 102);
        let now = Instant::now();
        let decision = choose(&root, &views(vec![("a", a), ("b", b)], now), &HashSet::new(), now);
        assert_eq!(decision.source.as_deref(), Some("b"));
        let mut unrelated = extend(&baseline(), 22, 32, 101);
        unrelated.history.front_mut().unwrap().parent = hash(99);
        assert_eq!(relation(&root, &unrelated), Relation::Unrelated);
        assert!(choose(&root, &views(vec![("unrelated", unrelated)], now), &HashSet::new(), now).source.is_none());
    }
    #[test]
    fn stale_quarantined_and_excessive_clock_leads_never_drive_presentation() {
        let root = baseline();
        let now = Instant::now();
        let a = extend(&root, 20, 30, 101);
        let mut candidates = views(vec![("a", a)], now - PEER_LIFETIME - Duration::from_millis(1));
        assert!(choose(&root, &candidates, &HashSet::new(), now).source.is_none());
        candidates.get_mut("a").unwrap().observed = now;
        assert!(choose(&root, &candidates, &HashSet::from(["a".into()]), now).source.is_none());
        candidates.get_mut("a").unwrap().tracker = Arc::new(extend(&root, 20, 30, 151));
        assert!(choose(&root, &candidates, &HashSet::new(), now).source.is_none());
    }

    #[tokio::test]
    async fn provisional_selection_and_conflict_fallback_never_mutate_reservations() {
        use crate::engine::{DEFAULT_BALL_TICK_DAA, DEFAULT_STAKE, GAME_VERSION, Lobby};
        use crate::node::{client::Pending, rpc::RpcTransport, tracker::Invitation};
        use kaspa_consensus_core::tx::Transaction;
        use kaspa_wrpc_client::{KaspaRpcClient, WrpcEncoding};
        let dir = tempfile::tempdir().unwrap();
        let a: Artifact =
            crate::node::load(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/artifacts/artifact.json"))).unwrap();
        let genesis = Transaction::new(0, vec![], vec![], 0, Default::default(), 0, vec![]);
        let invite = Invitation {
            version: GAME_VERSION,
            network: "devnet".into(),
            artifact_id: a.id.to_string(),
            anchor: hash(1),
            genesis: genesis.clone(),
            lobby: Lobby {
                left: vec![1; 32],
                right: vec![2; 32],
                refund: vec![1; 32],
                control_value: 0,
                ball_tick_daa: DEFAULT_BALL_TICK_DAA,
                stake_per_player: DEFAULT_STAKE,
            },
        };
        let mut primary = Cache::new(&invite);
        primary.tracker = baseline();
        primary.authorized_fees = 42;
        primary.pending.push(Pending {
            transaction: genesis,
            inputs: vec![],
            purpose: "retained reservation".into(),
            attempted_ms: 123,
            branch_changed: false,
        });
        let before = serde_json::to_value(&primary).unwrap();
        let mut config = Config::default();
        config.broadcast.endpoints = vec!["ws://127.0.0.1:1".into()];
        config.broadcast.log = dir.path().join("acceptance.jsonl");
        let client =
            KaspaRpcClient::new(WrpcEncoding::Borsh, Some("ws://127.0.0.1:1"), None, Some("devnet".parse().unwrap()), None).unwrap();
        let mut node = Node {
            rpc: Arc::new(RpcTransport::Wrpc(client)),
            endpoint: "ws://127.0.0.1:1".into(),
            network: "devnet".parse().unwrap(),
            prefix: kaspa_addresses::Prefix::Devnet,
            candidate: 0,
            broadcast: None,
        };
        let pool = Pool::start(&config, &node).unwrap().unwrap();
        node.broadcast = Some(pool.clone());
        let mut observers = Observers::new(a, invite, &node, config).unwrap();
        let now = Instant::now();
        observers.next_discovery = now + Duration::from_secs(60);
        observers.views = views(vec![("fast", extend(&primary.tracker, 20, 30, 101))], now);
        let selected = observers.poll(&primary, true, now);
        assert_eq!(selected.source.as_deref(), Some("fast"));
        assert!(!selected.conflict);
        assert_eq!(serde_json::to_value(&primary).unwrap(), before);
        observers.views.insert("conflict".into(), View { tracker: Arc::new(extend(&primary.tracker, 21, 31, 101)), observed: now });
        let conflict = observers.poll(&primary, true, now + Duration::from_millis(101));
        assert!(conflict.tracker.is_none() && conflict.conflict);
        assert!(conflict.epoch > selected.epoch, "fallback must clear buffered provisional sprites");
        assert_eq!(serde_json::to_value(&primary).unwrap(), before);
        let mut coherent = primary.clone();
        coherent.tracker.history.push_back(Undo {
            hash: hash(40),
            parent: hash(1),
            before: None,
            transactions: vec![],
            daa: Some(102),
            events: vec![],
        });
        coherent.tracker.cursor = hash(40);
        coherent.tracker.daa = 102;
        assert!(observers.poll(&coherent, false, now + Duration::from_millis(202)).conflict);
        let resolved = observers.poll(&coherent, true, now + Duration::from_millis(303));
        assert!(!resolved.conflict);
        assert!(resolved.tracker.is_none());
        assert_eq!(coherent.pending.len(), 1);
        assert_eq!(coherent.authorized_fees, 42);
        let mut advanced = coherent.clone();
        advanced.tracker = extend(&coherent.tracker, 22, 41, 103);
        let seed = secondary_seed(&advanced, &observers.invite);
        assert!(seed.pending.is_empty());
        assert_eq!(seed.authorized_fees, 0);
        assert!(same_stage(&seed.tracker.stage, &primary.tracker.stage));
        assert_eq!(seed.tracker.cursor, primary.tracker.cursor);
        assert_eq!(advanced.pending.len(), 1);
        drop(observers);
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "read-only public mainnet capability probe; no wallet or transactions"]
    async fn public_submission_peers_qualify_for_independent_acceptance() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config { network: "mainnet".into(), endpoint: "resolver".into(), ..Default::default() };
        config.broadcast.log = dir.path().join("readiness.jsonl");
        let node = Node::connect(&config).await.unwrap();
        let pool = node.broadcast.as_ref().unwrap();
        let until = Instant::now() + Duration::from_secs(30);
        while Instant::now() < until && pool.observers().len() < 7 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let peers = pool.observers();
        let total = peers.len();
        let mut tasks = tokio::task::JoinSet::new();
        for (id, peer) in peers {
            tasks.spawn(async move { (id, peer.endpoint.clone(), peer.qualify_acceptance().await.map_err(|e| e.to_string())) });
        }
        let mut qualified = 0;
        while let Some(result) = tasks.join_next().await {
            let (id, endpoint, result) = result.unwrap();
            if result.is_ok() {
                qualified += 1;
            }
            eprintln!("{}", json!({"peer_id":id,"endpoint":endpoint,"qualified":result.is_ok(),"error":result.err()}));
        }
        node.disconnect().await.unwrap();
        eprintln!("{}", json!({"secondary_peers":total,"qualified_secondary_observers":qualified,"transactions_submitted":0}));
        assert!(qualified > 0, "no secondary node supports the required Full history and UTXO APIs");
    }
}

struct Qualification {
    task: JoinHandle<()>,
    result: oneshot::Receiver<std::result::Result<Node, String>>,
}
impl Drop for Qualification {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct Peer {
    endpoint: String,
    follower: Follower,
}

#[derive(Clone)]
pub struct Selection {
    pub tracker: Option<Arc<Tracker>>,
    pub observed: Option<Instant>,
    pub source: Option<String>,
    pub conflict: bool,
    pub epoch: u64,
}

pub struct Observers {
    pool: Arc<Pool>,
    artifact: Artifact,
    invite: super::tracker::Invitation,
    config: Config,
    peers: HashMap<String, Peer>,
    qualifying: HashMap<String, Qualification>,
    retry: HashMap<String, Instant>,
    views: HashMap<String, View>,
    quarantined: HashSet<String>,
    conflict_anchor: Option<Hash>,
    next_discovery: Instant,
    displayed: Option<Arc<Tracker>>,
    epoch: u64,
    last_logged: Option<(Option<String>, Hash, bool)>,
    evaluated: Option<(Hash, u64, bool, u64)>,
    cached: Option<Selection>,
    next_evaluation: Instant,
}

impl Observers {
    pub fn new(a: Artifact, invite: super::tracker::Invitation, node: &Node, mut config: Config) -> Option<Self> {
        if !config.multi_peer_acceptance || config.broadcast.nodes <= 1 {
            return None;
        }
        config.allow_unsynced = false;
        Some(Self {
            pool: node.broadcast.clone()?,
            artifact: a,
            invite,
            config,
            peers: HashMap::new(),
            qualifying: HashMap::new(),
            retry: HashMap::new(),
            views: HashMap::new(),
            quarantined: HashSet::new(),
            conflict_anchor: None,
            next_discovery: Instant::now(),
            displayed: None,
            epoch: 0,
            last_logged: None,
            evaluated: None,
            cached: None,
            next_evaluation: Instant::now(),
        })
    }

    pub fn poll(&mut self, primary: &Cache, ready: bool, now: Instant) -> Selection {
        let mut dirty = false;
        if now >= self.next_discovery {
            self.next_discovery = now + Duration::from_secs(1);
            let available = self.pool.observers();
            let available_ids: HashSet<_> = available.iter().map(|(id, _)| id.clone()).collect();
            self.peers.retain(|id, _| available_ids.contains(id));
            self.qualifying.retain(|id, _| available_ids.contains(id));
            let old_len = self.views.len();
            self.views.retain(|id, _| available_ids.contains(id));
            dirty |= old_len != self.views.len();
            self.quarantined.retain(|id| available_ids.contains(id));
            for (id, node) in available {
                if self.peers.contains_key(&id)
                    || self.qualifying.contains_key(&id)
                    || self.retry.get(&id).is_some_and(|until| *until > now)
                {
                    continue;
                }
                let (sender, result) = oneshot::channel();
                let task = tokio::spawn(async move {
                    let qualified = node.qualify_acceptance().await.map_err(|e| e.to_string());
                    let _ = sender.send(qualified.map(|_| node));
                });
                self.qualifying.insert(id, Qualification { task, result });
            }
        }
        let completed: Vec<_> = self
            .qualifying
            .iter_mut()
            .filter_map(|(id, q)| match q.result.try_recv() {
                Ok(result) => Some((id.clone(), result)),
                Err(oneshot::error::TryRecvError::Closed) => Some((id.clone(), Err("acceptance qualification stopped".into()))),
                Err(oneshot::error::TryRecvError::Empty) => None,
            })
            .collect();
        for (id, result) in completed {
            self.qualifying.remove(&id);
            match result {
                Ok(node) => {
                    let endpoint = node.endpoint.clone();
                    let follower = Follower::secondary(
                        secondary_seed(primary, &self.invite),
                        self.artifact.clone(),
                        self.invite.clone(),
                        node,
                        self.config.clone(),
                    );
                    self.pool.observation_event(json!({"type":"acceptance_peer_ready","peer_id":id,"endpoint":endpoint}));
                    self.peers.insert(id, Peer { endpoint, follower });
                }
                Err(error) => self.failed(id, error, now),
            }
        }
        let mut failures = vec![];
        for (id, peer) in &self.peers {
            // Each follower has its own bounded feed, cursor, replay and UTXO
            // consistency check. No block from another peer enters that history.
            for _ in 0..3 {
                match peer.follower.try_update() {
                    Ok(None) => break,
                    Err(error) => {
                        failures.push((id.clone(), error));
                        break;
                    }
                    Ok(Some(update)) => {
                        dirty = true;
                        if !update.ready {
                            self.views.remove(id);
                            continue;
                        }
                        for record in &update.cache.transaction_log_pending {
                            self.pool.observation_event(
                                json!({"type":"acceptance_peer_transaction","peer_id":id,"endpoint":peer.endpoint,
                                "txid":record.event.txid,"status":record.status,"accepted_daa":record.event.accepted_daa,
                                "accepting_block":record.accepting_block,"validated_ms":update.validated_ms,
                                "arrival_ms":record.accepting_block.and_then(|b|update.arrivals.get(&b)).copied()}),
                            );
                        }
                        let age = Duration::from_millis(now_ms().saturating_sub(update.validated_ms));
                        self.views.insert(
                            id.clone(),
                            View { tracker: Arc::new(update.cache.tracker), observed: now.checked_sub(age).unwrap_or(now) },
                        );
                    }
                }
            }
        }
        for (id, error) in failures {
            dirty = true;
            self.failed(id, error, now);
        }

        let evaluation = (primary.tracker.cursor, primary.tracker.daa, ready, primary.game_reorgs);
        if !dirty && self.evaluated == Some(evaluation) && now < self.next_evaluation {
            if let Some(cached) = &self.cached {
                return cached.clone();
            }
        }
        // No history scans on every 5-ms gameplay tick. Fresh validation is
        // evaluated immediately; otherwise only recheck freshness at 10 Hz.
        self.evaluated = Some(evaluation);
        self.next_evaluation = now + Duration::from_millis(100);

        self.quarantined.retain(|id| {
            self.views.get(id).is_some_and(|v| !matches!(relation(&primary.tracker, &v.tracker), Relation::Same | Relation::Behind))
        });
        let decision = if ready { choose(&primary.tracker, &self.views, &self.quarantined, now) } else { Decision::default() };
        if !decision.conflicts.is_empty() {
            self.pool.observation_event(
                json!({"type":"acceptance_conflict","peer_ids":decision.conflicts,"primary_cursor":primary.tracker.cursor}),
            );
            self.quarantined.extend(decision.conflicts);
            self.conflict_anchor = Some(primary.tracker.cursor);
        }
        // Resume spending only after another coherent primary validation, not
        // merely because a peer disappeared or its provisional cache expired.
        if ready && self.conflict_anchor.is_some_and(|anchor| anchor != primary.tracker.cursor) {
            self.conflict_anchor = None;
        }
        let conflict = self.conflict_anchor.is_some();
        let source = if conflict { None } else { decision.source };
        let preview = source.as_ref().and_then(|id| self.views.get(id));
        let selected = preview.map(|v| v.tracker.clone());
        let shown = selected.as_deref().unwrap_or(&primary.tracker);
        if let Some(previous) = &self.displayed {
            if shown.daa < previous.daa
                || matches!(relation(previous, shown), Relation::Behind | Relation::Conflict | Relation::Unrelated)
            {
                self.epoch = self.epoch.wrapping_add(1);
            }
        }
        if self.displayed.as_ref().is_none_or(|old| old.cursor != shown.cursor || old.daa != shown.daa) {
            self.displayed = Some(selected.clone().unwrap_or_else(|| Arc::new(primary.tracker.clone())));
        }
        let stamp = (source.clone(), shown.cursor, conflict);
        if self.last_logged.as_ref() != Some(&stamp) {
            self.pool.observation_event(json!({"type":"acceptance_selection","peer_id":source,"cursor":shown.cursor,"provisional":selected.is_some(),"conflict":conflict}));
            self.last_logged = Some(stamp);
        }
        let selection = Selection { tracker: selected, observed: preview.map(|v| v.observed), source, conflict, epoch: self.epoch };
        self.cached = Some(selection.clone());
        selection
    }
    fn failed(&mut self, id: String, error: String, now: Instant) {
        self.peers.remove(&id);
        self.views.remove(&id);
        self.retry.insert(id.clone(), now + Duration::from_secs(30));
        self.pool.observation_event(json!({"type":"acceptance_peer_error","peer_id":id,"error":error}));
    }
}
