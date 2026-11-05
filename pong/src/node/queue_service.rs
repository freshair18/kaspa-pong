//! Durable queue coordinator. Following, authorization and submission use
//! separate workers, so RPC delays never own the terminal or its input thread.
use super::{
    Config,
    queue_book::{Book, Effect, Session, Signed},
    queue_codes::HostCode,
    queue_sets::{self, Authorization, Record},
    queue_tracker::QueueStage,
    reservations::{self, Claim},
    rpc::{Node, startup::Connection},
};
use crate::{
    engine::*,
    reusable::{self, Set},
};
use argent_runtime::TxBuilder;
use kaspa_consensus_core::{
    Hash,
    tx::{Transaction, TransactionOutpoint, UtxoEntry},
};
use secp256k1::Keypair;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct GameSummary {
    pub id: Hash,
    pub side: usize,
    pub finished: bool,
    pub label: String,
    pub code: HostCode,
    pub guest: Vec<u8>,
    pub rematch: Option<HostCode>,
}
#[derive(Clone)]
pub struct Pending {
    pub id: Hash,
    pub game: Option<Hash>,
    pub purpose: String,
    pub inputs: Vec<TransactionOutpoint>,
    pub fee: u64,
}
#[derive(Clone)]
pub struct View {
    pub ready: bool,
    pub observed: Instant,
    pub daa: u64,
    pub epoch: u64,
    pub records: Vec<Record>,
    pub offers: Vec<HostCode>,
    pub sessions: Vec<GameSummary>,
    pub clock: crate::network_ui::ClockSample,
    pub current: Option<Session>,
    pub pending: Vec<Pending>,
    pub confirmed: HashSet<Hash>,
    pub conflicted: HashSet<Hash>,
    pub status: String,
    pub created_count: u64,
}
pub struct Quote<T> {
    pub transaction: Transaction,
    pub inputs: Vec<UtxoEntry>,
    pub data: T,
    pub fee: u64,
    pub claim: Claim,
}
struct Inner {
    book: Mutex<Book>,
    view: Mutex<View>,
    selection: Mutex<Option<Hash>>,
    path: PathBuf,
    config: Config,
    player: Keypair,
    connection: Connection,
    cursor_gate: tokio::sync::Mutex<()>,
    _lock: std::fs::File,
}
#[derive(Clone)]
pub struct Service(Arc<Inner>);
static SERVICES: OnceLock<Mutex<HashMap<String, Service>>> = OnceLock::new();
impl Service {
    pub fn open(config: &Config, player: Keypair, connection: Connection) -> Result<Self> {
        let mut queue_config = config.clone();
        queue_config.game_fee_cap = true;
        queue_config.fee_sompi = 50_000_000;
        let config = &queue_config;
        let key = format!("{}:{}", config.network, player.x_only_public_key().0);
        let mut services = SERVICES.get_or_init(Default::default).lock().map_err(|_| "queue service lock poisoned")?;
        if let Some(service) = services.get(&key) {
            return Ok(service.clone());
        }
        let path = queue_sets::path(config, &player);
        let lock = super::lock(&path)?;
        let needs_anchor = !path.exists();
        let mut book = if needs_anchor {
            Book::new(config.network.clone(), pubkey(&player), Hash::from_bytes([0; 32]))
        } else {
            Book::load(&path, &config.network, &pubkey(&player))?
        };
        book.default_fee_budget = config.fee_budget_sompi;
        let view = View {
            ready: false,
            observed: Instant::now(),
            daa: book.daa,
            epoch: book.reorgs,
            clock: crate::network_ui::ClockSample { epoch: book.reorgs, daa: book.daa, observed: Instant::now() },
            records: vec![],
            offers: vec![],
            sessions: vec![],
            current: None,
            pending: vec![],
            confirmed: HashSet::new(),
            conflicted: HashSet::new(),
            status: "Synchronizing reusable queues…".into(),
            created_count: book.wallet.created_count,
        };
        let service = Self(Arc::new(Inner {
            book: Mutex::new(book),
            view: Mutex::new(view),
            selection: Mutex::new(None),
            path,
            config: config.clone(),
            player,
            connection,
            cursor_gate: tokio::sync::Mutex::new(()),
            _lock: lock,
        }));
        service.publish(false, None)?;
        let worker = service.clone();
        tokio::spawn(async move {
            worker.follow(needs_anchor).await;
        });
        let worker = service.clone();
        tokio::spawn(async move {
            worker.submit_pending().await;
        });
        let worker = service.clone();
        tokio::spawn(async move {
            worker.recover().await;
        });
        let worker = service.clone();
        tokio::spawn(async move {
            worker.clock().await;
        });
        let worker = service.clone();
        tokio::spawn(async move {
            worker.repair().await;
        });
        services.insert(key, service.clone());
        Ok(service)
    }
    pub fn snapshot(&self) -> View {
        self.0.view.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn config(&self) -> &Config {
        &self.0.config
    }
    pub fn player(&self) -> Keypair {
        self.0.player
    }
    pub fn was_confirmed(&self, id: Hash) -> bool {
        self.0.book.lock().map(|b| b.was_confirmed(id)).unwrap_or(false)
    }
    pub async fn node(&self) -> Result<Node> {
        let mut connection = self.0.connection.clone();
        connection.get(&self.0.config).await
    }
    fn publish(&self, ready: bool, status: Option<String>) -> Result<()> {
        let book = self.0.book.lock().map_err(|_| "queue journal lock poisoned")?;
        self.publish_book(&book, ready, status)?;
        if ready {
            self.0.view.lock().map_err(|_| "queue view lock poisoned")?.observed = Instant::now();
        }
        Ok(())
    }
    fn publish_book(&self, book: &Book, ready: bool, status: Option<String>) -> Result<()> {
        let selected = *self.0.selection.lock().map_err(|_| "queue selection lock poisoned")?;
        let mut view = self.0.view.lock().map_err(|_| "queue view lock poisoned")?;
        view.ready = ready;
        if let Some(status) = status {
            view.status = status;
        }
        view.daa = book.daa;
        view.epoch = book.reorgs;
        if view.clock.epoch != book.reorgs {
            view.clock = crate::network_ui::ClockSample { epoch: book.reorgs, daa: book.daa, observed: Instant::now() };
        }
        view.records = book.wallet.records.clone();
        view.created_count = book.wallet.created_count;
        view.offers = book.offers.iter().filter(|o| o.available).map(|o| o.code.clone()).collect();
        view.current = selected.and_then(|id| book.sessions.iter().find(|s| s.id == id).cloned());
        view.sessions = book
            .sessions
            .iter()
            .map(|s| GameSummary {
                id: s.id,
                side: s.side(&book.wallet.owner),
                finished: s.finished(),
                code: s.reservation.host.clone(),
                guest: s.reservation.guest.owner.clone(),
                rematch: book.rematch_offer(s),
                label: if s.recovered {
                    "Queues recovered · result history unavailable"
                } else if s.unavailable {
                    "Recovery needs retained chain history"
                } else {
                    match s.stage {
                        None => "Joining: waiting for chain acceptance",
                        Some(QueueStage::Lobby { .. }) => "Waiting to start",
                        Some(QueueStage::Live(_)) => "Playing",
                        Some(QueueStage::Collecting { .. }) => "Settling queues",
                        Some(QueueStage::Finished { .. }) => "Settled",
                        Some(QueueStage::Cancelled(_)) => "Cancelled",
                    }
                }
                .into(),
            })
            .collect();
        view.pending = book
            .pending()
            .into_iter()
            .map(|s| Pending {
                id: s.authorization.transaction.id(),
                game: match s.effect {
                    Effect::Game { session } => Some(session),
                    _ => None,
                },
                purpose: s.authorization.purpose.clone(),
                inputs: s.authorization.transaction.inputs.iter().map(|i| i.previous_outpoint).collect(),
                fee: s.fee,
            })
            .collect();
        view.conflicted = book
            .signed
            .iter()
            .filter_map(|s| {
                let id = s.authorization.transaction.id();
                book.is_conflicted(id).then_some(id)
            })
            .collect();
        view.confirmed = book
            .signed
            .iter()
            .filter_map(|s| {
                let id = s.authorization.transaction.id();
                (!book.is_pending(id) && !book.is_conflicted(id)).then_some(id)
            })
            .collect();
        Ok(())
    }
    pub async fn edit<T: Send + 'static>(&self, change: impl FnOnce(&mut Book) -> Result<T> + Send + 'static) -> Result<T> {
        let service = self.clone();
        tokio::task::spawn_blocking(move || {
            let result = (|| {
                let mut book = service.0.book.lock().map_err(|_| "queue journal lock poisoned")?;
                let mut candidate = book.clone();
                let value = change(&mut candidate)?;
                candidate.save(&service.0.path)?;
                *book = candidate;
                let ready = service.snapshot().ready;
                service.publish_book(&book, ready, None)?;
                Ok(value)
            })();
            result.map_err(|e: Box<dyn std::error::Error>| e.to_string())
        })
        .await?
        .map_err(Into::into)
    }
    pub async fn select(&self, id: Hash) -> Result<()> {
        *self.0.selection.lock().map_err(|_| "selection lock poisoned")? = Some(id);
        self.edit(move |b| {
            b.session(id)?;
            Ok(())
        })
        .await
    }
    pub async fn wait_ready(&self) -> Result<View> {
        loop {
            let view = self.snapshot();
            if view.ready && view.observed.elapsed() < crate::network_ui::SNAPSHOT_LIFETIME {
                return Ok(view);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    pub fn available(view: &View) -> Vec<&Set> {
        view.records
            .iter()
            .filter(|r| {
                r.phase == queue_sets::Phase::Available
                    && r.set.idle()
                    && !view.offers.iter().any(|o| o.queue.id == r.set.id().unwrap() && o.queue.generation == r.set.generation())
                    && !view.pending.iter().any(|p| p.inputs.iter().any(|i| r.set.lanes.iter().any(|c| c.outpoint == *i)))
            })
            .map(|r| &r.set)
            .collect()
    }
    pub async fn quote<T: Send + 'static>(
        &self,
        sources: Vec<(TransactionOutpoint, UtxoEntry)>,
        mut make: impl FnMut(&TxBuilder<'_>, &[Funding], u64) -> Result<(Transaction, T)> + Send + 'static,
    ) -> Result<Quote<T>> {
        self.quote_mode(sources, false, move |b, c, f| make(b, c, f)).await
    }
    pub async fn quote_all<T: Send + 'static>(
        &self,
        sources: Vec<(TransactionOutpoint, UtxoEntry)>,
        make: impl FnMut(&TxBuilder<'_>, &[Funding], u64) -> Result<(Transaction, T)> + Send + 'static,
    ) -> Result<Quote<T>> {
        self.quote_mode(sources, true, make).await
    }
    pub fn wallet_summary(network: &str, owner: &str) -> Option<(usize, u64)> {
        let key = format!("{network}:{owner}");
        let service = SERVICES.get()?.lock().ok()?.get(&key)?.clone();
        let view = service.snapshot();
        let sets = view.records.iter().filter(|r| r.phase != queue_sets::Phase::Closed).collect::<Vec<_>>();
        Some((sets.len(), sets.iter().map(|r| r.set.balance()).sum()))
    }
    async fn quote_mode<T: Send + 'static>(
        &self,
        sources: Vec<(TransactionOutpoint, UtxoEntry)>,
        all: bool,
        mut make: impl FnMut(&TxBuilder<'_>, &[Funding], u64) -> Result<(Transaction, T)> + Send + 'static,
    ) -> Result<Quote<T>> {
        let node = self.node().await?;
        let reserved = reservations::reserved()?;
        let coins = node.funding_candidates(&pubkey(&self.0.player), 1, self.0.config.coinbase_maturity_daa, &reserved).await?;
        let config = self.0.config.clone();
        let (transaction, data, chosen, fee) = tokio::task::spawn_blocking(move || {
            let result = (|| {
                let a = reusable::artifact()?;
                let b = TxBuilder::new(&a)?;
                let count = coins.len();
                queue_sets::funded_transaction(&config, &coins, |coins, fee| {
                    if all && coins.len() != count {
                        return Err("manual consolidation includes all available ordinary coins".into());
                    }
                    make(&b, coins, fee)
                })
            })();
            result.map_err(|e: Box<dyn std::error::Error>| e.to_string())
        })
        .await??;
        let claim = Claim::inputs(chosen.iter().map(|f| f.outpoint).collect())?;
        let inputs = transaction
            .inputs
            .iter()
            .map(|i| {
                sources
                    .iter()
                    .find(|(out, _)| *out == i.previous_outpoint)
                    .map(|(_, u)| u.clone())
                    .or_else(|| chosen.iter().find(|f| f.outpoint == i.previous_outpoint).map(|f| f.utxo.clone()))
                    .ok_or_else(|| "quote lacks exact source metadata".into())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Quote { transaction, inputs, data, fee, claim })
    }
    pub async fn authorize<T>(
        &self,
        quote: Quote<T>,
        purpose: &str,
        effect: Effect,
        successors: Vec<Set>,
        creates_set: bool,
        session: Option<Session>,
    ) -> Result<Hash> {
        let Quote { transaction, inputs, fee, claim, .. } = quote;
        let node = self.node().await?;
        if !node.sources_unspent(&transaction, &inputs).await? {
            return Err("funding or queues changed during preparation; refreshing is required".into());
        }
        let id = transaction.id();
        let config = self.0.config.clone();
        let purpose = purpose.to_owned();
        self.edit(move |book| {
            let cells = book.all_cells();
            for (i, input) in transaction.inputs.iter().enumerate().filter(|(i, _)| inputs[*i].covenant_id.is_some()) {
                if !cells.iter().any(|(out, u)| *out == input.previous_outpoint && super::tracker::same_utxo(u, &inputs[i])) {
                    return Err("queue state changed before authorization".into());
                }
            }
            if matches!(effect, Effect::Wallet { .. }) {
                let available = book.available().iter().map(|s| s.id().unwrap()).collect::<HashSet<_>>();
                if book.wallet.records.iter().any(|r| {
                    transaction.inputs.iter().any(|i| r.set.lanes.iter().any(|c| c.outpoint == i.previous_outpoint))
                        && !available.contains(&r.set.id().unwrap())
                }) {
                    return Err("queue set is reserved or settling".into());
                }
            }
            if let Some(session) = session {
                book.add_session(session)?;
            }
            book.authorize(
                Signed {
                    authorization: Authorization { transaction, inputs, purpose, successors, creates_set },
                    effect,
                    attempted_ms: 0,
                    fee,
                },
                &config,
            )
        })
        .await?;
        drop(claim);
        Ok(id)
    }
}

#[derive(Clone, Debug)]
pub enum GameAction {
    Start,
    Cancel,
    Move { lane: usize, direction: i64 },
    Resolve { side: usize, until: i64 },
    End { timeout: bool },
    Collect,
}
impl GameAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Cancel => "cancel invitation",
            Self::Move { .. } => "move",
            Self::Resolve { .. } => "resolve",
            Self::End { timeout: true } => "timeout",
            Self::End { .. } => "settle",
            Self::Collect => "collect queues",
        }
    }
}

impl Service {
    async fn clock(&self) {
        loop {
            let epoch = self.snapshot().epoch;
            if let Ok(n) = self.node().await.map_err(|e| e.to_string()) {
                if let Ok(info) = n.info().await.map_err(|e| e.to_string()) {
                    if let Ok(mut view) = self.0.view.lock() {
                        if view.epoch == epoch {
                            view.clock =
                                crate::network_ui::ClockSample { epoch, daa: info.virtual_daa_score, observed: Instant::now() };
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    async fn follow(&self, mut needs_anchor: bool) {
        let mut connection = self.0.connection.clone();
        let mut last_save = Instant::now();
        let mut last_coherent = Instant::now();
        loop {
            let result = async {
                let n = connection.get(&self.0.config).await.map_err(|e| e.to_string())?;
                if needs_anchor {
                    let cursor = n.accepted_cursor().await.map_err(|e| e.to_string())?;
                    let info = n.info().await.map_err(|e| e.to_string())?;
                    self.edit(move |book| {
                        book.cursor = cursor;
                        book.daa = info.virtual_daa_score;
                        Ok(())
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                    needs_anchor = false;
                }
                let _cursor_guard = self.0.cursor_gate.lock().await;
                let cursor = self.0.book.lock().map_err(|_| "journal lock poisoned")?.cursor;
                let (delta, info) = tokio::try_join!(async { n.delta(cursor).await.map_err(|e| e.to_string()) }, async {
                    n.info().await.map_err(|e| e.to_string())
                })?;
                let service = self.clone();
                let save_due = last_save.elapsed() > Duration::from_secs(2);
                let (changed, cells) = tokio::task::spawn_blocking(move || {
                    let result = (|| {
                        let a = reusable::artifact()?;
                        let mut book = service.0.book.lock().map_err(|_| "journal lock poisoned")?;
                        if book.cursor != cursor {
                            return Ok((false, book.owned_cells()));
                        }
                        let changed = book.apply(&a, &delta.removed, &delta.added)?;
                        book.daa = info.virtual_daa_score.max(delta.added.last().map(|b| b.daa).unwrap_or(0));
                        if changed || save_due {
                            book.save(&service.0.path)?;
                        }
                        service.publish_book(&book, false, Some("Checking accepted queue state…".into()))?;
                        Ok((changed, book.owned_cells()))
                    })();
                    result.map_err(|e: Box<dyn std::error::Error>| e.to_string())
                })
                .await
                .map_err(|e| e.to_string())??;
                if changed || save_due {
                    last_save = Instant::now();
                }
                let found = if cells.is_empty() {
                    vec![]
                } else {
                    n.utxos(&cells.iter().map(|(_, u)| u.script_public_key.clone()).collect::<Vec<_>>())
                        .await
                        .map_err(|e| e.to_string())?
                };
                let coherent =
                    cells.iter().all(|(out, u)| found.iter().any(|f| f.outpoint == *out && super::tracker::same_utxo(&f.utxo, u)));
                if coherent {
                    last_coherent = Instant::now();
                } else if last_coherent.elapsed() > Duration::from_secs(10) {
                    return Err("saved queue snapshot needs reconciliation".to_owned());
                }
                self.publish(
                    coherent,
                    Some(if coherent { "Following accepted chain" } else { "Catching up with queue changes…" }.into()),
                )
                .map_err(|e| e.to_string())?;
                Ok::<(), String>(())
            }
            .await;
            if let Err(error) = result {
                let _ = self.publish(false, Some(format!("Synchronizing queues: {error}")));
                connection.retain(None);
                if error.contains("retained")
                    || error.contains("prun")
                    || error.contains("not found")
                    || error.contains("incoherent queue")
                    || error.contains("needs reconciliation")
                {
                    if let Ok(n) = connection.get(&self.0.config).await.map_err(|e| e.to_string()) {
                        let _cursor_guard = self.0.cursor_gate.lock().await;
                        let scripts = self
                            .0
                            .book
                            .lock()
                            .ok()
                            .and_then(|book| {
                                let a = reusable::artifact().ok()?;
                                let b = TxBuilder::new(&a).ok()?;
                                book.recovery_scripts(&b).ok()
                            })
                            .unwrap_or_default();
                        if let (Ok(cursor), Ok(info), Ok(found)) = (
                            n.accepted_cursor().await.map_err(|e| e.to_string()),
                            n.info().await.map_err(|e| e.to_string()),
                            n.utxos(&scripts).await.map_err(|e| e.to_string()),
                        ) {
                            if self.edit(move |book| book.bootstrap(cursor, info.virtual_daa_score, &found)).await.is_ok() {
                                last_coherent = Instant::now();
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            } else {
                tokio::time::sleep(Duration::from_millis(self.0.config.poll_ms.max(25))).await;
            }
        }
    }
    async fn repair(&self) {
        loop {
            let sessions = self
                .0
                .book
                .lock()
                .ok()
                .map(|book| book.sessions.iter().filter(|s| s.unavailable && !s.recovered).cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            for session in sessions {
                if let Ok(n) = self.node().await.map_err(|e| e.to_string()) {
                    let _ = self.replay_session(&n, session).await;
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
    async fn replay_session(&self, n: &Node, session: Session) -> std::result::Result<(), String> {
        let delta = n.delta(session.anchor).await.map_err(|e| e.to_string())?;
        let end = delta.added.last().map(|b| b.hash).unwrap_or(session.anchor);
        let blocks = delta.added;
        let id = session.id;
        let wanted = self
            .0
            .book
            .lock()
            .map_err(|_| "journal lock poisoned")?
            .signed
            .iter()
            .filter(|s| matches!(s.effect,Effect::Game{session} if session==id))
            .map(|s| {
                (
                    s.authorization.transaction.id(),
                    s.authorization.transaction.inputs.iter().map(|i| i.previous_outpoint).collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        let tail_wanted = wanted.clone();
        let (stage, mut confirmed, mut conflicted) = tokio::task::spawn_blocking(move || {
            let result = (|| {
                let a = reusable::artifact()?;
                let b = TxBuilder::new(&a)?;
                let mut stage = None;
                let mut confirmed = vec![];
                let mut conflicted = vec![];
                for block in blocks {
                    for tx in block.transactions {
                        for (id, inputs) in &wanted {
                            if tx.tx.id() == *id {
                                confirmed.push(*id);
                            } else if tx.tx.inputs.iter().any(|i| inputs.contains(&i.previous_outpoint)) {
                                conflicted.push(*id);
                            }
                        }
                        if tx.tx.id() == session.id {
                            let (lobby, guest) = session.reservation.accepted(&b, &tx, block.daa)?;
                            stage = Some(QueueStage::Lobby { lobby, sets: [guest, session.host_set.clone()] });
                        } else if let Some(stage) = &mut stage {
                            stage.apply(&a, &b, &tx, block.daa)?;
                        }
                    }
                }
                Ok((stage.ok_or("reservation not present in retained history")?, confirmed, conflicted))
            })();
            result.map_err(|e: Box<dyn std::error::Error>| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;
        // Origin replay runs independently. Only align the short tails under
        // the cursor gate; a history timeout cannot hold normal game following.
        let _cursor_guard = self.0.cursor_gate.lock().await;
        let cursor = self.0.book.lock().map_err(|_| "journal lock poisoned")?.cursor;
        let (tail, global) = tokio::try_join!(async { n.delta(end).await.map_err(|e| e.to_string()) }, async {
            n.delta(cursor).await.map_err(|e| e.to_string())
        })?;
        let tail_end = tail.added.last().map(|b| b.hash).unwrap_or(end);
        let global_end = global.added.last().map(|b| b.hash).unwrap_or(cursor);
        if !tail.removed.is_empty() || tail_end != global_end {
            return Err("chain advanced or reorganized during game recovery; retrying".into());
        }
        let (stage, confirmed, conflicted) = tokio::task::spawn_blocking(move || {
            let result = (|| {
                let a = reusable::artifact()?;
                let b = TxBuilder::new(&a)?;
                let mut stage = stage;
                for block in tail.added {
                    for tx in block.transactions {
                        for (id, inputs) in &tail_wanted {
                            if tx.tx.id() == *id {
                                confirmed.push(*id);
                            } else if tx.tx.inputs.iter().any(|i| inputs.contains(&i.previous_outpoint)) {
                                conflicted.push(*id);
                            }
                        }
                        stage.apply(&a, &b, &tx, block.daa)?;
                    }
                }
                Ok((stage, confirmed, conflicted))
            })();
            result.map_err(|e: Box<dyn std::error::Error>| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;
        let cells = stage.cells();
        let found =
            n.utxos(&cells.iter().map(|(_, u)| u.script_public_key.clone()).collect::<Vec<_>>()).await.map_err(|e| e.to_string())?;
        if !cells.iter().all(|(out, u)| found.iter().any(|f| f.outpoint == *out && super::tracker::same_utxo(&f.utxo, u))) {
            return Err("game advanced during recovery".into());
        }
        // Bring the global journal to the same canonical head before installing
        // the reconstructed game. A later head is retried on the next pass.
        self.edit(move |book| {
            let a = reusable::artifact()?;
            book.apply(&a, &global.removed, &global.added)?;
            book.restore_session(id, stage, tail_end, confirmed, conflicted)
        })
        .await
        .map_err(|e| e.to_string())
    }
    async fn submit_pending(&self) {
        loop {
            let pending = self
                .0
                .book
                .lock()
                .ok()
                .map(|b| {
                    b.pending()
                        .into_iter()
                        .filter(|p| super::client::now_ms().saturating_sub(p.attempted_ms) > 5000)
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for signed in pending {
                let result = async {
                    let n = self.node().await.map_err(|e| e.to_string())?;
                    if !n
                        .sources_unspent(&signed.authorization.transaction, &signed.authorization.inputs)
                        .await
                        .map_err(|e| e.to_string())?
                    {
                        return Ok::<(), String>(());
                    }
                    let id = signed.authorization.transaction.id();
                    self.edit(move |b| {
                        if let Some(s) = b.signed.iter_mut().find(|s| s.authorization.transaction.id() == id) {
                            s.attempted_ms = super::client::now_ms();
                        }
                        Ok(())
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                    n.submit(&signed.authorization.transaction).await.map_err(|e| e.to_string())?;
                    Ok(())
                }
                .await;
                if let Err(error) = result {
                    super::rpc::startup::diagnostic(&format!("queue submission retained: {error}"));
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    async fn recover(&self) {
        loop {
            let view = self.snapshot();
            if view.ready && view.observed.elapsed() < crate::network_ui::SNAPSHOT_LIFETIME {
                let (sessions, offers) = self
                    .0
                    .book
                    .lock()
                    .ok()
                    .map(|b| {
                        (b.sessions.iter().filter(|s| !s.finished() && !s.unavailable).cloned().collect::<Vec<_>>(), b.offers.clone())
                    })
                    .unwrap_or_default();
                for session in sessions {
                    if let Some(action) = recovery_action(&session, view.daa, &pubkey(&self.0.player), &offers) {
                        if view.pending.iter().any(|p| p.game == Some(session.id) && !p.purpose.starts_with("move")) {
                            continue;
                        }
                        let result = self.game_action(session, action).await.map_err(|e| e.to_string());
                        if let Err(error) = result {
                            super::rpc::startup::diagnostic(&format!("queue recovery deferred: {error}"));
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    pub async fn game_action(&self, session: Session, action: GameAction) -> Result<Hash> {
        let quote = self.prepare_action(session.clone(), action.clone()).await?;
        self.authorize(quote, action.label(), Effect::Game { session: session.id }, vec![], false, None).await
    }
    pub async fn prepare_action(&self, session: Session, action: GameAction) -> Result<Quote<()>> {
        if session.unavailable || session.recovered {
            return Err("this game needs recovery before further play".into());
        }
        let owner = self.0.player;
        let sources = session.stage.as_ref().ok_or("game not accepted yet")?.cells();
        self.quote(sources, move |b, coins, fee| {
            let tx = match (&session.stage, &action) {
                (Some(QueueStage::Lobby { lobby, sets }), GameAction::Start) => {
                    reusable::start(
                        b,
                        lobby,
                        &sets[0],
                        &sets[1],
                        &owner,
                        sets[1].balance().max(reusable::required_capital(&sets[1], lobby.state.lobby.stake_per_player as u64)?),
                        coins,
                        fee,
                    )?
                    .0
                }
                (Some(QueueStage::Lobby { lobby, sets }), GameAction::Cancel) => {
                    reusable::cancel(b, lobby, &sets[0], &owner, coins, fee)?.0
                }
                (Some(QueueStage::Live(m)), GameAction::Move { lane, direction }) => {
                    reusable::command(b, &m.sets[session.side(&pubkey(&owner))].lanes[*lane], *direction, &owner, coins, fee)?.0
                }
                (Some(QueueStage::Live(m)), GameAction::Resolve { side, until }) => {
                    reusable::checkpoint(b, m, *side, *until, &owner, coins, fee)?.0
                }
                (Some(QueueStage::Live(m)), GameAction::End { timeout }) => reusable::end(b, m, &owner, *timeout, coins, fee)?.0,
                (Some(QueueStage::Collecting { result, sets }), GameAction::Collect) => {
                    reusable::collect(b, result, sets, &owner, coins, fee)?.0
                }
                _ => return Err("game state changed; refreshing".into()),
            };
            Ok((tx, ()))
        })
        .await
    }
}
fn recovery_action(session: &Session, daa: u64, owner: &[u8], offers: &[super::queue_book::LocalOffer]) -> Option<GameAction> {
    match session.stage.as_ref()? {
        QueueStage::Lobby { .. }
            if session.side(owner) == 0
                && (session.cancel_requested
                    || super::client::now_ms().saturating_sub(session.created_ms) > super::hosted::EXPIRY_MS) =>
        {
            Some(GameAction::Cancel)
        }
        QueueStage::Lobby { .. }
            if !session.cancel_requested
                && session.side(owner) == 1
                && offers.iter().any(|o| {
                    o.automatic
                        && o.available
                        && o.code == session.reservation.host
                        && o.expected_guest.as_ref() == Some(&session.reservation.guest.owner)
                }) =>
        {
            Some(GameAction::Start)
        }
        QueueStage::Live(m) if m.court.state.world.ended() => Some(GameAction::End { timeout: false }),
        QueueStage::Live(m) if timeout_result(&m.snapshot()).is_ok_and(|r| r.end_daa >= 0 && daa >= r.end_daa as u64) => {
            Some(GameAction::End { timeout: true })
        }
        QueueStage::Collecting { .. } => Some(GameAction::Collect),
        _ => None,
    }
}
