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
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, atomic::{AtomicU64, Ordering}},
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
    pub acknowledged: bool,
}
#[derive(Clone)]
pub struct View {
    pub ready: bool,
    pub observed: Instant,
    pub chain_observed: Option<Instant>,
    pub daa: u64,
    pub epoch: u64,
    pub records: Vec<Record>,
    pub offers: Vec<HostCode>,
    pub sessions: Vec<GameSummary>,
    pub clock: crate::network_ui::ClockSample,
    pub current: Option<Session>,
    pub movement: Option<reusable::Match>,
    movement_parent: Option<Hash>,
    pub pending: Vec<Pending>,
    pub confirmed: HashSet<Hash>,
    pub conflicted: HashSet<Hash>,
    pub status: String,
    pub created_count: u64,
    pub auto_create: bool,
    chain_epoch:u64,
    clock_from_observer: bool,
    pub recent_transactions:std::collections::VecDeque<super::audit::TransactionRecord>,
}
pub struct Quote<T> {
    pub transaction: Transaction,
    pub inputs: Vec<UtxoEntry>,
    pub data: T,
    pub fee: u64,
    pub claim: Claim,
}

/// Record timings off the gameplay and broadcast workers. Join events by txid.
pub fn gameplay_timing(phase: &str, txid: Hash, fields: serde_json::Value) {
    let row = serde_json::json!({
        "type": "reusable_gameplay_timing",
        "phase": phase,
        "txid": txid,
        "observed_unix_ms": super::client::now_ms(),
        "timing": fields,
    });
    tokio::task::spawn_blocking(move || super::rpc::startup::diagnostic(&row.to_string()));
}

struct Inner {
    book: Mutex<Book>,
    view: Mutex<View>,
    selection: Mutex<Option<Hash>>,
    path: PathBuf,
    config: Config,
    reserve_target: AtomicU64,
    player: Keypair,
    connection: Connection,
    live_node: Mutex<Option<Node>>,
    fee_coins: Mutex<Option<(Instant, Vec<Funding>)>>,
    lane_fee_hints: [Mutex<Option<(u64, u64)>>; 2],
    cursor_gate: tokio::sync::Mutex<()>,
    selection_changed: tokio::sync::Notify,
    submission_ready: tokio::sync::Notify,
    refresh_chain: tokio::sync::Notify,
    timeout_acknowledged: Mutex<HashSet<Hash>>,
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
        let unanchored = Hash::from_bytes([0; 32]);
        let mut book = if path.exists() {
            Book::load(&path, &config.network, &pubkey(&player))?
        } else {
            Book::new(config.network.clone(), pubkey(&player), unanchored)
        };
        let migrated = book.import_previous(&[
            &queue_sets::previous_path(config, &player),
            &queue_sets::v28_path(config, &player),
            &queue_sets::v29_path(config, &player),
            &queue_sets::v30_path(config, &player),
            &queue_sets::v31_path(config, &player),
            &queue_sets::v32_path(config, &player),
            &queue_sets::v33_path(config, &player),
            &queue_sets::v34_path(config, &player),
            &queue_sets::v35_path(config, &player),
            &queue_sets::v36_path(config, &player),
            &queue_sets::v37_path(config, &player),
            &queue_sets::v38_path(config, &player),
            &queue_sets::v39_path(config, &player),
        ])?;
        reusable::current_runtime()?;
        book.default_fee_budget = config.fee_budget_sompi;
        let recovered_gameplay = book.load_gameplay_outbox(&path)?;
        if migrated || recovered_gameplay {
            book.checkpoint(&path)?;
        }
        let needs_anchor = book.cursor == unanchored;
        let view = View {
            ready: false,
            observed: Instant::now(),
            chain_observed: None,
            daa: book.daa,
            epoch: book.reorgs,
            chain_epoch:book.reorgs,
            clock_from_observer: false,
            recent_transactions:Default::default(),
            clock: crate::network_ui::ClockSample { epoch: book.reorgs, daa: book.daa, observed: Instant::now() },
            records: vec![],
            offers: vec![],
            sessions: vec![],
            current: None,
            movement: None,
            movement_parent: None,
            pending: vec![],
            confirmed: HashSet::new(),
            conflicted: HashSet::new(),
            status: "Synchronizing reusable queues…".into(),
            created_count: book.wallet.created_count,
            auto_create: book.may_auto_create(),
        };
        let service = Self(Arc::new(Inner {
            book: Mutex::new(book),
            view: Mutex::new(view),
            selection: Mutex::new(None),
            path,
            config: config.clone(),
            reserve_target: AtomicU64::new(config.queue_reserve_target()?),
            player,
            connection,
            live_node: Mutex::new(None),
            fee_coins: Mutex::new(None),
            lane_fee_hints: Default::default(),
            cursor_gate: tokio::sync::Mutex::new(()),
            selection_changed: tokio::sync::Notify::new(),
            submission_ready: tokio::sync::Notify::new(),
            refresh_chain: tokio::sync::Notify::new(),
            timeout_acknowledged: Mutex::new(HashSet::new()),
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
            worker.refresh_fee_coins().await;
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
    pub fn queue_reserve_target(&self) -> u64 {
        self.0.reserve_target.load(Ordering::Relaxed)
    }
    pub fn set_queue_reserve_target(&self, config_path: &Path, kas: u64) -> Result<()> {
        let target = super::save_queue_reserve_target(config_path, kas)?;
        self.0.reserve_target.store(target, Ordering::Relaxed);
        Ok(())
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
    async fn broadcast_node(&self) -> Result<Node> {
        let live = self.0.live_node.lock().map_err(|_| "game node lock poisoned")?.clone();
        match live {
            Some(node) => Ok(node),
            None => self.node().await,
        }
    }
    async fn refresh_fee_coins(&self) {
        loop {
            if self.snapshot().current.as_ref().is_some_and(|s| matches!(s.stage, Some(QueueStage::Live(_)))) {
                let result = async {
                    let node = self.broadcast_node().await?;
                    node.funding_candidates(&pubkey(&self.0.player), 1, self.0.config.coinbase_maturity_daa, &[]).await
                }.await;
                if let Ok(coins) = result {
                    if let Ok(mut cache) = self.0.fee_coins.lock() {
                        *cache = Some((Instant::now(), coins));
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
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
        let current=selected.and_then(|id|book.sessions.iter().find(|s|s.id==id).cloned());
        if view.chain_epoch!=book.reorgs {
            if game_state_changed(&view.current,&current){view.epoch=view.epoch.wrapping_add(1);}
            view.chain_epoch=book.reorgs;
        }
        if view.clock.epoch != view.epoch {
            view.clock = crate::network_ui::ClockSample { epoch: view.epoch, daa: book.daa, observed: Instant::now() };
        }
        view.records = book.wallet.records.clone();
        view.created_count = book.wallet.created_count;
        view.auto_create = book.may_auto_create();
        view.offers = book.offers.iter().filter(|o| o.available).map(|o| o.code.clone()).collect();
        let parent = current.as_ref().and_then(|s| book.pending_resolve(s.id)).map(|p| p.authorization.transaction.id());
        if parent.is_none() {
            view.movement = None;
        } else if view.movement_parent != parent || game_state_changed(&view.current, &current) {
            view.movement = current.as_ref().and_then(|s| book.resolve_successor(s.id).ok().flatten());
        }
        view.movement_parent = parent;
        view.current = current;
        view.recent_transactions=book.recent_transactions.iter().filter(|r|Some(r.game)==selected).cloned().collect();
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
                label: if s.recovered || (s.unavailable && book.superseded_session(s)) {
                    "Queues recovered · result history unavailable"
                } else if s.unavailable {
                    "Recovery needs retained chain history"
                } else {
                    match s.stage {
                        None => "Joining: waiting for chain acceptance",
                        Some(QueueStage::Lobby { .. }|QueueStage::Invitation{..}) => "Waiting to start",
                        Some(QueueStage::Live(_)) => "Playing",
                        Some(QueueStage::Collecting { .. }) => "Settling queues",
                        _ if s.stage.as_ref().and_then(|stage| stage.restoring_set(&book.wallet.owner)).is_some() => "Restoring your queue Bank",
                        Some(QueueStage::Finished { .. }) => "Settled",
                        Some(QueueStage::Cancelled(_)) => "Cancelled",
                        Some(QueueStage::RecoveredReset(_)) => "Queues recovered · result history unavailable",
                    }
                }
                .into(),
            })
            .collect();
        let mut timeout_acknowledged = self.0.timeout_acknowledged.lock().map_err(|_| "timeout acknowledgment lock poisoned")?;
        timeout_acknowledged.retain(|id| book.is_pending(*id));
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
                acknowledged: timeout_acknowledged.contains(&s.authorization.transaction.id()),
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
        self.edit_with_outbox(None, change).await
    }

    async fn edit_with_outbox<T: Send + 'static>(
        &self, gameplay: Option<Hash>, change: impl FnOnce(&mut Book) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let service = self.clone();
        tokio::task::spawn_blocking(move || {
            let result = (|| {
                let mut book = service.0.book.lock().map_err(|_| "queue journal lock poisoned")?;
                let mut candidate = book.clone();
                let value = change(&mut candidate)?;
                if let Some(id) = gameplay {
                    if !book.has_authorization(id) {
                        let signed = candidate.signed.last().filter(|s| s.authorization.transaction.id() == id)
                            .ok_or("missing signed gameplay transaction")?;
                        Book::save_gameplay_authorization(&service.0.path, signed)?;
                    }
                    candidate.restore(&service.0.path)?;
                } else {
                    candidate.checkpoint(&service.0.path)?;
                }
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
        let book = self.0.book.lock().map_err(|_| "journal lock poisoned")?;
        book.session(id)?;
        *self.0.selection.lock().map_err(|_| "selection lock poisoned")? = Some(id);
        self.0.selection_changed.notify_waiters();
        self.publish_book(&book, self.snapshot().ready, None)
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
        self.quote_mode(sources, None, false, false, None, move |b, c, f, _| make(b, c, f)).await
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
        movement_session: Option<Hash>,
        contact: bool,
        mut try_without_wallet: bool,
        mass_selection_topup: Option<u64>,
        mut make: impl FnMut(&TxBuilder<'_>, &[Funding], u64, bool) -> Result<(Transaction, T)> + Send + 'static,
    ) -> Result<Quote<T>> {
        let mut funding_time = Duration::ZERO;
        let mut pricing_time = Duration::ZERO;
        let mut pricing_passes = 0;
        let mut used_wallet_rpc = false;
        let reserved = reservations::reserved()?;
        let mut config = self.0.config.clone();
        if movement_session.is_some() { config.fee_sompi = config.fee_sompi.min(10_000_000); }
        let mut cached = if movement_session.is_some() || contact {
            self.0.fee_coins.lock().map_err(|_| "fee coin cache lock poisoned")?.as_ref()
                .filter(|(at, _)| at.elapsed() < Duration::from_secs(2))
                .map(|(_, coins)| coins.iter().filter(|f| !reserved.contains(&f.outpoint)).cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        } else { vec![] };
        if !cached.is_empty() {
            let book = self.0.book.lock().map_err(|_| "journal lock poisoned")?;
            for signed in book.signed.iter().filter(|s| book.was_confirmed(s.authorization.transaction.id())) {
                cached.retain(|coin| !signed.authorization.transaction.inputs.iter().any(|i| i.previous_outpoint == coin.outpoint));
            }
        }
        let mut pending = if let Some(id) = movement_session {
            self.0.book.lock().map_err(|_| "journal lock poisoned")?.pending_fee_coins(id)
                .into_iter().filter(|f| !reserved.contains(&f.outpoint)).collect::<Vec<_>>()
        } else { vec![] };
        let (transaction, data, chosen, fee) = loop {
            let using_local = try_without_wallet || !cached.is_empty() || !pending.is_empty();
            let coins = if std::mem::take(&mut try_without_wallet) {
                vec![]
            } else if !cached.is_empty() {
                std::mem::take(&mut cached)
            } else if !pending.is_empty() {
                std::mem::take(&mut pending)
            } else {
                let started = Instant::now();
                used_wallet_rpc = true;
                let node = self.node().await?;
                let coins = node.funding_candidates(&pubkey(&self.0.player), 1, config.coinbase_maturity_daa, &reserved).await?;
                funding_time += started.elapsed();
                coins
            };
            let attempt_config = config.clone();
            let mass_sources = mass_selection_topup.map(|_| sources.clone());
            let service = self.clone();
            let started = Instant::now();
            let (result, returned_make, passes) = tokio::task::spawn_blocking(move || {
                let mut passes = 0;
                let result = (|| {
                    let (_, b) = reusable::current_runtime()?;
                    let build = |coins: &[Funding], fee| {
                        passes += 1;
                        make(b, coins, fee, true)
                    };
                    if coins.is_empty() && (movement_session.is_some() || contact) {
                        let hint = &service.0.lane_fee_hints[usize::from(contact)];
                        let rate = super::fee_policy::selected_rate(&attempt_config);
                        let previous = hint.lock().ok().and_then(|saved| *saved).filter(|(old, _)| *old == rate);
                        let quote = price_lane_action(&attempt_config, rate, previous.map(|(_, fee)| fee), build)?;
                        if let Ok(mut saved) = hint.lock() { *saved = Some((rate, quote.3)); }
                        return Ok(quote);
                    }
                    match (mass_selection_topup, mass_sources.as_deref()) {
                        (Some(topup), Some(sources)) => queue_sets::funded_by_mass(&attempt_config, &coins, sources, topup, build),
                        _ => queue_sets::funded_transaction(&attempt_config, &coins, build).or_else(|error| {
                            if coins.is_empty() || (movement_session.is_none() && !contact) {
                                return Err(error);
                            }
                            // A refill can leave expensive wallet change. Preserve
                            // the existing fee-only path with the same funding snapshot.
                            queue_sets::funded_transaction(&attempt_config, &coins, |coins, fee| {
                                passes += 1;
                                make(b, coins, fee, false)
                            })
                        }),
                    }
                })();
                (result.map_err(|e: Box<dyn std::error::Error>| e.to_string()), make, passes)
            })
            .await?;
            pricing_time += started.elapsed();
            pricing_passes += passes;
            make = returned_make;
            match result {
                Ok(quote) => break quote,
                Err(_) if using_local => continue,
                Err(error) => return Err(error.into()),
            }
        };
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
        if movement_session.is_some() || contact {
            gameplay_timing("signed", transaction.id(), serde_json::json!({
                "funding_rpc": used_wallet_rpc,
                "funding_rpc_ms": funding_time.as_micros() as f64 / 1000.0,
                "pricing_and_signing_ms": pricing_time.as_micros() as f64 / 1000.0,
                "pricing_and_signing_passes": pricing_passes,
            }));
        }
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
        let Quote { transaction, mut inputs, fee, claim, .. } = quote;
        let direct = session.is_none() && successors.is_empty() && !creates_set
            && matches!(effect, Effect::Game { .. }) && matches!(purpose, "move" | "resolve");
        if !direct {
            let node = self.node().await?;
            let parents = self.0.book.lock().map_err(|_| "journal lock poisoned")?.source_parents(&transaction, &inputs)?;
            if !node.sources_available(&transaction, &inputs, &parents).await? {
                return Err("funding or queues changed during preparation; refreshing is required".into());
            }
        }
        let id = transaction.id();
        let config = self.0.config.clone();
        let purpose = purpose.to_owned();
        let player = self.0.player;
        let started = Instant::now();
        self.edit_with_outbox(direct.then_some(id), move |book| {
            let mut cells = book.all_cells();
            cells.extend(book.command_successors(&transaction, &inputs)?);
            for (i, input) in transaction.inputs.iter().enumerate() {
                if inputs[i].covenant_id.is_none() { continue; }
                let Some((_, actual)) = cells.iter().find(|(out, u)| *out == input.previous_outpoint
                    && (super::tracker::same_utxo(u, &inputs[i])
                        || (book.was_confirmed(out.transaction_id) && super::rpc::accepted_source_executes(&transaction, &inputs, i, u)))) else {
                    return Err("queue state changed before authorization".into());
                };
                // Validate the complete authorization against the current input
                // metadata, including any cross-input checks in the Court.
                inputs[i].clone_from(actual);
            }
            if let Effect::Wallet{ref closes}=effect {
                let available = book.available().iter().map(|s| s.id().unwrap()).collect::<HashSet<_>>();
                if book.wallet.records.iter().any(|r| {
                    transaction.inputs.iter().any(|i| r.set.lanes.iter().any(|c| c.outpoint == i.previous_outpoint))
                        && !available.contains(&r.set.id().unwrap())
                        && !(r.phase==queue_sets::Phase::Legacy && r.set.idle() && closes.contains(&r.set.id().unwrap()))
                }) {
                    return Err("queue set is reserved or settling".into());
                }
            }
            if let Some(session) = session {
                book.add_session(session)?;
            }
            let signed = Signed {
                authorization: Authorization { transaction, inputs, purpose, successors, creates_set },
                effect, attempted_ms: 0, fee,
            };
            let restoration = book.prepare_collection_restoration(&signed, &player, &config).ok().flatten();
            book.authorize(signed, &config)?;
            // The collection remains valid if restoration needs wallet funds or a higher fee budget.
            if let Some(child) = restoration {
                let child_id = child.authorization.transaction.id();
                if book.authorize(child, &config).is_ok() {
                    book.direct_send.insert(child_id);
                }
            }
            if direct && book.is_pending(id) && book.signed.last().is_some_and(|s| {
                s.authorization.transaction.id() == id && s.attempted_ms == 0
            }) {
                book.direct_send.insert(id);
            }
            Ok(())
        })
        .await?;
        drop(claim);
        if direct {
            gameplay_timing("authorized", id, serde_json::json!({"elapsed_ms": started.elapsed().as_micros() as f64 / 1000.0}));
        }
        self.0.submission_ready.notify_one();
        Ok(id)
    }
}

pub(super) fn price_lane_action<T>(
    config: &Config,
    rate: u64,
    previous: Option<u64>,
    mut build: impl FnMut(&[Funding], u64) -> Result<(Transaction, T)>,
) -> Result<(Transaction, T, Vec<Funding>, u64)> {
    if let Some(fee) = previous {
        if let Ok((tx, data)) = build(&[], fee) {
            if super::client::relay_fee_at_rate(&tx, config, rate).is_ok_and(|required| required == fee) {
                return Ok((tx, data, vec![], fee));
            }
        }
    }
    queue_sets::funded_prefixes(config, &[], rate, &mut build)
}

#[derive(Clone, Debug)]
pub enum GameAction {
    Start,
    Cancel,
    Move { lane: usize, direction: i64 },
    Resolve { side: usize, until: i64 },
    Timeout,
    Collect,
    RestoreBank,
}
impl GameAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Cancel => "cancel invitation",
            Self::Move { .. } => "move",
            Self::Resolve { .. } => "resolve",
            Self::Timeout => "timeout",
            Self::Collect => "collect queues",
            Self::RestoreBank => "restore queue Bank",
        }
    }
    pub(super) fn recovery_ready(&self, wallet_ready: bool, chain_ready: bool) -> bool {
        wallet_ready || (chain_ready && matches!(self, Self::Timeout | Self::Collect | Self::RestoreBank))
    }
}

impl Service {
    async fn clock(&self) {
        loop {
            let epoch = self.0.view.lock().ok().map(|view| view.epoch);
            let observer = self.0.live_node.lock().ok().and_then(|node| node.clone());
            if let (Some(epoch), Some(n)) = (epoch, observer) {
                if let Ok(info) = n.info().await.map_err(|e| e.to_string()) {
                    let current = self.0.live_node.lock().ok().and_then(|node| node.clone());
                    if current.is_some_and(|node| Arc::ptr_eq(&n.rpc, &node.rpc))
                        && let Ok(mut view) = self.0.view.lock()
                        && view.epoch == epoch
                    {
                        view.clock = crate::network_ui::ClockSample { epoch, daa: info.virtual_daa_score, observed: Instant::now() };
                        view.clock_from_observer = true;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    async fn follow(&self, mut needs_anchor: bool) {
        let mut connection = self.0.connection.clone();
        let mut last_save = Instant::now();
        let mut incoherent_since = None::<Instant>;
        let mut failures = 0u32;
        let mut notifications = None::<super::rpc::VirtualChainFeed>;
        let mut subscription = None::<tokio::task::JoinHandle<Option<super::rpc::VirtualChainFeed>>>;
        let mut subscription_attempt = None::<Instant>;
        let mut notification_at = None::<Instant>;
        let mut first_notification = true;
        let mut health = None::<tokio::task::JoinHandle<std::result::Result<(), String>>>;
        let mut last_health = Instant::now();
        let mut check = None::<tokio::task::JoinHandle<std::result::Result<Option<bool>, String>>>;
        let mut check_failures = 0;
        let mut last_check = None::<Instant>;
        let mut unchecked = Vec::new();
        let poll = Duration::from_millis(self.0.config.poll_ms.max(25));
        let mut idle_bootstrap = !needs_anchor && self.0.book.lock().map(|b| b.can_bootstrap_idle()).unwrap_or(false);
        loop {
            let result = async {
                let n = connection.observer(&self.0.config).await.map_err(|e| e.to_string())?;
                let replaced = {
                    let mut current = self.0.live_node.lock().map_err(|_| "game node lock poisoned")?;
                    let replaced = current.as_ref().is_none_or(|old| !Arc::ptr_eq(&old.rpc, &n.rpc));
                    if replaced {
                        let mut view = self.0.view.lock().map_err(|_| "queue view lock poisoned")?;
                        view.chain_observed = None;
                        view.clock_from_observer = false;
                        view.ready = false;
                        *current = Some(n.clone());
                    }
                    replaced
                };
                if replaced {
                    if let Some(task) = health.take() { task.abort(); }
                    if let Some(task) = subscription.take() { task.abort(); }
                    if let Some(task) = check.take() { task.abort(); }
                    notifications = None;
                    subscription_attempt = None;
                    notification_at = None;
                    first_notification = true;
                    last_health = Instant::now();
                    incoherent_since = None;
                    check_failures = 0;
                }
                if check.as_ref().is_some_and(|task| task.is_finished()) {
                    match check.take().unwrap().await.map_err(|e| e.to_string()).and_then(|result| result) {
                        Ok(coherent) => {
                            check_failures = 0;
                            match coherent {
                                Some(true) => incoherent_since = None,
                                Some(false) if incoherent_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(10) =>
                                    return Err("saved queue snapshot needs reconciliation".to_owned()),
                                _ => {}
                            }
                        }
                        Err(error) => {
                            check_failures += 1;
                            if check_failures >= 3 {
                                return Err(format!("repeated queue output check failures: {error}"));
                            }
                            super::rpc::startup::diagnostic(&format!("queue output check deferred: {error}"));
                        }
                    }
                }
                if health.as_ref().is_some_and(|task| task.is_finished()) {
                    health.take().unwrap().await.map_err(|e| e.to_string())??;
                }
                if health.is_none() && last_health.elapsed() >= Duration::from_secs(5) {
                    last_health = Instant::now();
                    let node = n.clone();
                    let config = self.0.config.clone();
                    health = Some(tokio::spawn(async move {
                        node.qualify(&config, false).await.map_err(|e| format!("queue observer health check failed: {e}"))
                    }));
                }
                if subscription.as_ref().is_some_and(|task| task.is_finished()) {
                    notifications = subscription.take().unwrap().await.ok().flatten();
                }
                if notifications.is_none() && subscription.is_none()
                    && subscription_attempt.is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
                {
                    subscription_attempt = Some(Instant::now());
                    let node = n.clone();
                    subscription = Some(tokio::spawn(async move {
                        let started = Instant::now();
                        match tokio::time::timeout(Duration::from_secs(3), node.virtual_chain_feed()).await {
                            Ok(Ok(feed)) => {
                                super::rpc::startup::phase("queue_chain_subscription_ready", started);
                                Some(feed)
                            }
                            Ok(Err(error)) => {
                                super::rpc::startup::diagnostic(&format!("queue chain subscription failed; polling: {error}"));
                                None
                            }
                            Err(_) => {
                                super::rpc::startup::phase("queue_chain_subscription_timed_out", started);
                                None
                            }
                        }
                    }));
                }
                if idle_bootstrap {
                    self.publish(false, Some("Verifying idle queues directly from accepted outputs…".into())).map_err(|e| e.to_string())?;
                    self.reconcile(&n).await?;
                    idle_bootstrap = false;
                    needs_anchor = false;
                }
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
                let notification_at = notification_at.take();
                let read_started = Instant::now();
                let delta = n.delta(cursor).await.map_err(|e| e.to_string())?;
                let read_time = read_started.elapsed();
                let service = self.clone();
                let save_due = last_save.elapsed() >= Duration::from_secs(30);
                let (changed, saved, cells, accepted) = tokio::task::spawn_blocking(move || {
                    let result = (|| {
                        let (a, b) = reusable::current_runtime()?;
                        let mut book = service.0.book.lock().map_err(|_| "journal lock poisoned")?;
                        if book.cursor != cursor {
                            return Ok((false, false, book.owned_cells(), vec![]));
                        }
                        let tracked = book.pending().into_iter().filter(|s| matches!(s.authorization.purpose.as_str(), "move" | "resolve"))
                            .map(|s| s.authorization.transaction.id()).collect::<Vec<_>>();
                        let selected = *service.0.selection.lock().map_err(|_| "queue selection lock poisoned")?;
                        let live = |book: &Book| selected.and_then(|id| book.session(id).ok()).is_some_and(|s| {
                            !s.unavailable && matches!(s.stage, Some(QueueStage::Live(_)))
                        });
                        let was_live = live(&book);
                        let previous_daa = book.daa;
                        let changed = book.apply_with_builder(a, b, &delta.removed, &delta.added)?;
                        let accepted = tracked.into_iter().filter(|id| book.was_confirmed(*id)).collect::<Vec<_>>();
                        for id in &accepted {
                            gameplay_timing("accepted", *id, serde_json::json!({
                                "chain_read_ms": read_time.as_secs_f64() * 1000.0,
                                "read_start_to_replay_ms": read_started.elapsed().as_secs_f64() * 1000.0,
                                "notification_to_replay_ms": notification_at.map(|at| at.elapsed().as_secs_f64() * 1000.0),
                            }));
                        }
                        if !delta.removed.is_empty() {
                            if let Ok(mut coins) = service.0.fee_coins.lock() { *coins = None; }
                        }
                        if delta.removed.is_empty() {
                            let view = service.0.view.lock().map_err(|_| "queue view lock poisoned")?;
                            let clock_daa = (view.clock_from_observer && view.clock.observed.elapsed() <= Duration::from_secs(2))
                                .then_some(view.clock.daa).unwrap_or(0);
                            book.daa = book.daa.max(previous_daa).max(clock_daa);
                        }
                        // The signed records cover live play. Save the full
                        // book on stage changes outside it, and periodically
                        // keep the resume cursor fresh for open invitations.
                        let live_update = was_live && live(&book);
                        let saved = !live_update && (changed || (save_due && !book.can_bootstrap_idle()));
                        let save_started = Instant::now();
                        if saved {
                            book.checkpoint(&service.0.path)?;
                        } else if changed {
                            book.restore(&service.0.path)?;
                        }
                        let local_time = read_started.elapsed().saturating_sub(read_time);
                        if read_time > Duration::from_millis(750) || local_time > Duration::from_millis(250) {
                            let diagnostic = serde_json::json!({
                                "type": "queue_replay_slow",
                                "observed_unix_ms": super::client::now_ms(),
                                "rpc_ms": read_time.as_millis(),
                                "local_ms": local_time.as_millis(),
                                "journal_ms": if saved { save_started.elapsed().as_millis() } else { 0 },
                            });
                            tokio::task::spawn_blocking(move || super::rpc::startup::diagnostic(&diagnostic.to_string()));
                        }
                        let keep_ready=!changed && service.snapshot().ready;
                        service.publish_book(&book, keep_ready, None)?;
                        service.0.view.lock().map_err(|_| "queue view lock poisoned")?.chain_observed = Some(Instant::now());
                        Ok((changed, saved, book.owned_cells(), accepted))
                    })();
                    result.map_err(|e: Box<dyn std::error::Error>| e.to_string())
                })
                .await
                .map_err(|e| e.to_string())??;
                if save_due || saved {
                    last_save = Instant::now();
                }
                unchecked.extend(accepted);
                if check.is_none() && (changed || last_check.is_none_or(|at| at.elapsed() >= poll)) {
                    last_check = Some(Instant::now());
                    let service = self.clone();
                    let accepted = std::mem::take(&mut unchecked);
                    check = Some(tokio::spawn(async move { service.check_wallet_outputs(n, cells, accepted).await }));
                }
                Ok::<(), String>(())
            }
            .await;
            if let Err(error) = result {
                if let Some(task) = check.take() { task.abort(); }
                if let Some(task) = subscription.take() { task.abort(); }
                if let Some(task) = health.take() { task.abort(); }
                notifications = None;
                subscription_attempt = None;
                notification_at = None;
                unchecked.clear();
                incoherent_since = None;
                check_failures = 0;
                if let Ok(mut view) = self.0.view.lock() { view.chain_observed = None; }
                if let Ok(mut node) = self.0.live_node.lock() { *node = None; }
                failures = failures.saturating_add(1);
                let _ = self.publish(false, Some(format!("Synchronizing queues: {error}")));
                super::rpc::startup::diagnostic(&format!("queue synchronization deferred: {error}"));
                let reconcile = needs_queue_reconciliation(&error, failures);
                if !reconcile { connection.retain(None); }
                if reconcile {
                    let _ = self.publish(false, Some("Verifying current queue outputs; the saved history could not be followed…".into()));
                    if let Ok(n) = connection.get(&self.0.config).await.map_err(|e| e.to_string()) {
                        match self.reconcile(&n).await {
                            Ok(()) => {
                                needs_anchor = false;
                                idle_bootstrap = false;
                                failures = 0;
                                let _ = self.publish(false, Some("Queues checked; synchronizing recent chain changes…".into()));
                            }
                            Err(error) => {
                                let _ = self.publish(false, Some(format!("Queue recovery: {error}")));
                                super::rpc::startup::diagnostic(&format!("queue reconciliation deferred: {error}"));
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            } else {
                failures = 0;
                tokio::select! {
                    _ = self.0.refresh_chain.notified() => {}
                    result = async { notifications.as_ref().unwrap().changed().await }, if notifications.is_some() => {
                        if result.is_ok() {
                            let now = Instant::now();
                            notification_at = Some(now);
                            if first_notification {
                                first_notification = false;
                                tokio::task::spawn_blocking(move || super::rpc::startup::phase("queue_chain_first_notification", now));
                            }
                        } else {
                            notifications = None;
                            super::rpc::startup::diagnostic("queue chain notification stream closed; polling");
                        }
                    }
                    _ = tokio::time::sleep(poll) => {}
                }
            }
        }
    }
    async fn check_wallet_outputs(
        &self, n: Node, cells: Vec<(TransactionOutpoint, UtxoEntry)>, accepted: Vec<Hash>,
    ) -> std::result::Result<Option<bool>, String> {
        let started = Instant::now();
        let found = if cells.is_empty() {
            Ok(vec![])
        } else {
            n.utxos(&cells.iter().map(|(_, u)| u.script_public_key.clone()).collect::<Vec<_>>()).await.map_err(|e| e.to_string())
        };
        let coherent = found.as_ref().is_ok_and(|found| cells.iter().all(|(out, u)| {
            found.iter().any(|f| f.outpoint == *out && super::tracker::same_utxo(&f.utxo, u))
        }));
        let book = self.0.book.lock().map_err(|_| "queue journal lock poisoned")?;
        let current = book.owned_cells();
        let observer = self.0.live_node.lock().map_err(|_| "game node lock poisoned")?;
        let current_check = observer.as_ref().is_some_and(|peer| Arc::ptr_eq(&peer.rpc, &n.rpc))
            && current.len() == cells.len() && current.iter().zip(&cells)
                .all(|((out, u), (checked, entry))| out == checked && super::tracker::same_utxo(u, entry));
        if current_check {
            let mut view = self.0.view.lock().map_err(|_| "queue view lock poisoned")?;
            view.ready = coherent;
            if coherent { view.observed = Instant::now(); }
            view.status = if coherent { "Following accepted chain" } else { "Catching up with queue changes…" }.into();
        }
        drop(observer);
        drop(book);
        for id in accepted {
            gameplay_timing("queue_utxo_checked", id, serde_json::json!({
                "coherent": coherent, "current": current_check, "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
            }));
        }
        found?;
        Ok(current_check.then_some(coherent))
    }
    async fn reconcile(&self, n: &Node) -> std::result::Result<(), String> {
        let _cursor_guard = self.0.cursor_gate.lock().await;
        let scripts = {
            let book = self.0.book.lock().map_err(|_| "journal lock poisoned")?;
            let (_, b) = reusable::current_runtime().map_err(|e| e.to_string())?;
            book.recovery_scripts(b).map_err(|e| e.to_string())?
        };
        // Anchor first, then prove outputs. Later spends are replayed from
        // this cursor; absence never releases a queue or an uncertain spend.
        let cursor = n.accepted_cursor().await.map_err(|e| e.to_string())?;
        let (info, found) = tokio::try_join!(async { n.info().await.map_err(|e| e.to_string()) }, async { n.utxos(&scripts).await.map_err(|e| e.to_string()) })?;
        self.edit(move |book| book.bootstrap(cursor, info.virtual_daa_score, &found)).await.map_err(|e| e.to_string())
    }
    async fn repair(&self) {
        loop {
            let selection_changed = self.0.selection_changed.notified();
            let selected = self.0.selection.lock().ok().and_then(|selected| *selected);
            let sessions = self
                .0
                .book
                .lock()
                .ok()
                .map(|book| book.repair_candidates(selected))
                .unwrap_or_default();
            tokio::select! {
                biased;
                _ = selection_changed => {},
                _ = async {
                    for session in sessions {
                        if let Some(n) = self.node().await.ok() {
                            let _ = self.replay_session(&n, session).await;
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                } => {},
            }
        }
    }
    async fn replay_session(&self, n: &Node, session: Session) -> std::result::Result<(), String> {
        {
            let book = self.0.book.lock().map_err(|_| "journal lock poisoned")?;
            let selected = *self.0.selection.lock().map_err(|_| "queue selection lock poisoned")?;
            if !book.session(session.id).is_ok_and(|s| book.can_repair_session(s, selected)) {
                return Ok(());
            }
        }
        let started = Instant::now();
        let delta = n.delta(session.anchor).await.map_err(|e| e.to_string());
        let elapsed = started.elapsed();
        if elapsed > Duration::from_millis(750) {
            let row = serde_json::json!({"type": "queue_repair_history_slow", "game": session.id,
                "observed_unix_ms": super::client::now_ms(), "elapsed_ms": elapsed.as_millis()});
            tokio::task::spawn_blocking(move || super::rpc::startup::diagnostic(&row.to_string()));
        }
        let delta = delta?;
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
                let (a, b) = reusable::current_runtime()?;
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
                            let (lobby, guest) = session.reservation.accepted(b, &tx, block.daa)?;
                            stage = Some(if let Some(host)=&session.host_set{QueueStage::Lobby { lobby, sets: [guest,host.clone()] }}else{QueueStage::Invitation{lobby,guest}});
                        } else if let Some(stage) = &mut stage {
                            stage.apply(a, b, &tx, block.daa)?;
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
        let cursor = {
            let book = self.0.book.lock().map_err(|_| "journal lock poisoned")?;
            let selected = *self.0.selection.lock().map_err(|_| "queue selection lock poisoned")?;
            if !book.session(id).is_ok_and(|session| book.can_repair_session(session, selected)) {
                return Ok(());
            }
            book.cursor
        };
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
                let (a, b) = reusable::current_runtime()?;
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
                        stage.apply(a, b, &tx, block.daa)?;
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
        let service = self.clone();
        self.edit(move |book| {
            let selected = *service.0.selection.lock().map_err(|_| "queue selection lock poisoned")?;
            if !book.session(id).is_ok_and(|session| book.can_repair_session(session, selected)) {
                return Ok(());
            }
            if book.cursor != cursor {
                return Err("chain advanced during game recovery; retrying".into());
            }
            let (a, b) = reusable::current_runtime()?;
            book.apply_with_builder(a, b, &global.removed, &global.added)?;
            book.restore_session(id, stage, tail_end, confirmed, conflicted)
        })
        .await
        .map_err(|e| e.to_string())
    }
    async fn submit_pending(&self) {
        let mut tasks = tokio::task::JoinSet::new();
        let mut running = HashSet::new();
        let mut acknowledged = HashSet::new();
        let mut retry_after = HashMap::new();
        let mut epoch = 0;
        loop {
            let (pending, direct, current_epoch) = self
                .0
                .book
                .lock()
                .ok()
                .map(|b| {
                    (b.pending().into_iter().cloned().collect::<Vec<_>>(), b.direct_send.clone(), b.reorgs)
                })
                .unwrap_or_default();
            if epoch != current_epoch {
                acknowledged.clear();
                epoch = current_epoch;
            }
            let pending_ids = pending.iter().map(|p| p.authorization.transaction.id()).collect::<HashSet<_>>();
            let attempted_collections = pending.iter().filter(|s| s.attempted_ms > 0 && s.authorization.purpose == "collect queues")
                .map(|s| s.authorization.transaction.id()).collect::<HashSet<_>>();
            acknowledged.retain(|id| pending_ids.contains(id));
            retry_after.retain(|id, _| pending_ids.contains(id));
            for signed in pending {
                if tasks.len() >= 4 { break; }
                let tx = &signed.authorization.transaction;
                let id = tx.id();
                let direct_restore = signed.authorization.purpose == "restore queue Bank"
                    && signed.attempted_ms == 0 && direct.contains(&id);
                if direct_restore && tx.inputs.iter().any(|input| {
                    let parent = input.previous_outpoint.transaction_id;
                    pending_ids.contains(&parent) && !attempted_collections.contains(&parent)
                }) {
                    continue;
                }
                if running.contains(&id)
                    || super::client::now_ms().saturating_sub(signed.attempted_ms) < submission_retry_ms(&signed)
                    || retry_after.get(&id).is_some_and(|at| Instant::now() < *at)
                    || tx.inputs.iter().any(|i| {
                        let parent = i.previous_outpoint.transaction_id;
                        !direct_restore && pending_ids.contains(&parent) && running.contains(&parent)
                    })
                {
                    continue;
                }
                let unverified_parent = tx.inputs.iter().any(|i| {
                    let parent = i.previous_outpoint.transaction_id;
                    pending_ids.contains(&parent) && !acknowledged.contains(&parent)
                });
                let first_send = signed.attempted_ms == 0 && direct.contains(&id) && (direct_restore || !unverified_parent);
                let retry_delay = Duration::from_millis(submission_retry_ms(&signed));
                let service = self.clone();
                running.insert(id);
                tasks.spawn(async move { (id, epoch, retry_delay, service.send_pending(signed, first_send).await) });
            }
            tokio::select! {
                Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                    match result {
                        Ok((id, started_epoch, delay, result)) => {
                            running.remove(&id);
                            let delay = if matches!(&result, Ok(true)) { Duration::from_secs(5) } else { delay };
                            retry_after.insert(id, Instant::now() + delay);
                            if matches!(&result, Ok(true)) && started_epoch == epoch {
                                acknowledged.insert(id);
                            } else {
                                acknowledged.remove(&id);
                            }
                            if let Err(error) = result {
                                super::rpc::startup::diagnostic(&format!("queue submission retained: {error}"));
                            }
                        }
                        Err(error) => {
                            // A failed task returned no transaction ID. Drain the
                            // other tasks before allowing another send of those IDs.
                            while tasks.join_next().await.is_some() {}
                            running.clear();
                            self.0.refresh_chain.notify_one();
                            super::rpc::startup::diagnostic(&format!("queue submission worker stopped: {error}"));
                        }
                    }
                }
                _ = self.0.submission_ready.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    }
    async fn send_pending(&self, signed: Signed, first_send: bool) -> std::result::Result<bool, String> {
        let Signed { authorization, .. } = signed;
        let resolves = authorization.purpose == "resolve";
        let collects = authorization.purpose == "collect queues";
        let timeout = authorization.purpose == "timeout";
        let timed = resolves || authorization.purpose == "move";
        let tx = authorization.transaction;
        let id = tx.id();
        let node = self.broadcast_node().await.map_err(|e| e.to_string())?;
        if !first_send {
            let parents = self.0.book.lock().map_err(|_| "journal lock poisoned")?
                .source_parents(&tx, &authorization.inputs).map_err(|e| e.to_string())?;
            if !node.sources_available(&tx, &authorization.inputs, &parents).await.map_err(|e| e.to_string())? {
                self.0.refresh_chain.notify_one();
                return Ok(false);
            }
        }
        // Authorization already saved the exact transaction and reserved its inputs.
        // If the attempt timestamp is lost on crash, recovery rechecks and resends
        // those same bytes.
        let service = self.clone();
        let active = tokio::task::spawn_blocking(move || {
            let mut book = service.0.book.lock().map_err(|_| "journal lock poisoned".to_owned())?;
            if !book.is_pending(id) || (first_send && !book.direct_send.remove(&id)) {
                return Ok(false);
            }
            book.direct_send.remove(&id);
            let signed = book.signed.iter_mut().find(|s| s.authorization.transaction.id() == id)
                .ok_or("signed transaction missing before submission")?;
            signed.attempted_ms = super::client::now_ms();
            Ok::<_, String>(true)
        }).await.map_err(|e| e.to_string())??;
        if !active { return Ok(false); }
        if collects { self.0.submission_ready.notify_one(); }
        if timed {
            gameplay_timing("broadcast_handoff", id, serde_json::json!({"direct": first_send}));
        }
        match node.submit(&tx).await {
            Ok(_) => {
                self.0.refresh_chain.notify_one();
                if resolves || timeout {
                    let service = self.clone();
                    // Refresh replacement lanes outside the broadcast handoff.
                    tokio::task::spawn_blocking(move || {
                        let Ok(book) = service.0.book.lock() else { return };
                        if timeout && book.is_pending(id) {
                            if let Ok(mut acknowledged) = service.0.timeout_acknowledged.lock() { acknowledged.insert(id); }
                        }
                        let ready = service.snapshot().ready;
                        let _ = service.publish_book(&book, ready, None);
                    });
                }
                Ok(true)
            }
            Err(error) => {
                self.0.refresh_chain.notify_one();
                if let Ok(mut coins) = self.0.fee_coins.lock() { *coins = None; }
                Err(error.to_string())
            }
        }
    }
    async fn recover(&self) {
        loop {
            let view = self.snapshot();
            let wallet_ready = view.ready && view.observed.elapsed() < crate::network_ui::SNAPSHOT_LIFETIME;
            let chain_ready = view.chain_observed.is_some_and(|at| at.elapsed() < crate::network_ui::SNAPSHOT_LIFETIME);
            if wallet_ready || chain_ready {
                let actions = self.0.book.lock().ok().map(|book| {
                    book.sessions.iter()
                        .filter(|session| !session.unavailable && !session.recovered && !book.superseded_session(session))
                        .filter_map(|session| {
                            let action = recovery_action(session, view.daa, &book.wallet.owner, &book.offers)?;
                            if !action.recovery_ready(wallet_ready, chain_ready)
                                || view.pending.iter().any(|p| p.game == Some(session.id) && !p.purpose.starts_with("move"))
                            {
                                return None;
                            }
                            Some((session.clone(), action))
                        })
                        .collect::<Vec<_>>()
                }).unwrap_or_default();
                for (session, action) in actions {
                    if let Err(error) = self.game_action(session, action).await {
                        super::rpc::startup::diagnostic(&format!("queue recovery deferred: {error}"));
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
        let stage = session.stage.as_ref().ok_or("game not accepted yet")?;
        let reserve_target = self.queue_reserve_target();
        let sources = stage.cells();
        let movement_session = matches!(action, GameAction::Move { .. }).then_some(session.id);
        let contact = matches!(action, GameAction::Resolve { .. });
        let start_topup = match (stage, &action) {
            (QueueStage::Lobby { lobby, sets }, GameAction::Start) => Some(
                reusable::required_capital(&sets[1], u64::try_from(lobby.state.lobby.stake_per_player)?)?
                    .saturating_sub(sets[1].balance()),
            ),
            _ => None,
        };
        let winner_collects = matches!((stage, &action), (QueueStage::Collecting { result, .. }, GameAction::Collect)
            if usize::try_from(result.state.collection.finish.winner).ok() == Some(session.side(&pubkey(&owner))));
        let lane_fee_first = movement_session.is_some() || contact || winner_collects
            || matches!(action, GameAction::Cancel | GameAction::RestoreBank);
        let mass_topup = if matches!(action, GameAction::Collect | GameAction::RestoreBank) { Some(0) } else { start_topup };
        self.quote_mode(sources, movement_session, contact, lane_fee_first, mass_topup, move |b, coins, fee, refill| {
            let target = if refill { reserve_target } else { 0 };
            let tx = match (&session.stage, &action) {
                (Some(QueueStage::Lobby { lobby, sets }), GameAction::Start) => {
                    reusable::start(
                        b,
                        lobby,
                        &sets[0],
                        &sets[1],
                        &owner,
                        sets[1].balance().checked_add(start_topup.unwrap_or_default()).ok_or("start capital overflow")?,
                        coins,
                        fee,
                    )?
                    .0
                }
                (Some(QueueStage::Invitation{lobby,guest}),GameAction::Cancel)=>reusable::cancel(b,lobby,guest,&owner,coins,fee)?.0,
                (Some(QueueStage::Lobby { lobby, sets }), GameAction::Cancel) => {
                    reusable::cancel(b, lobby, &sets[0], &owner, coins, fee)?.0
                }
                (Some(QueueStage::Live(m)), GameAction::Move { lane, direction }) => {
                    reusable::command_with_reserve_target(b, &m.sets[session.side(&pubkey(&owner))].lanes[*lane], *direction, &owner, coins, fee, target)?.0
                }
                (Some(QueueStage::Live(m)), GameAction::Resolve { side, until }) => {
                    reusable::checkpoint_with_reserve_target(b, m, *side, *until, &owner, coins, fee, target)?.0
                }
                (Some(QueueStage::Live(m)), GameAction::Timeout) => reusable::timeout(b, m, &owner, coins, fee)?.0,
                (Some(QueueStage::Collecting { result, sets }), GameAction::Collect) => {
                    reusable::collect(b, result, sets, &owner, coins, fee)?.0
                }
                (Some(stage), GameAction::RestoreBank) => {
                    let set = stage.restoring_set(&pubkey(&owner)).ok_or("queue Bank already restored")?;
                    reusable::restore_bank(b, set, &owner, coins, fee)?
                }
                _ => return Err("game state changed; refreshing".into()),
            };
            Ok((tx, ()))
        })
        .await
    }
}

fn submission_retry_ms(signed: &Signed) -> u64 {
    if matches!(signed.authorization.purpose.as_str(), "move" | "resolve" | "restore queue Bank") { 1000 } else { 5000 }
}

pub(super) fn needs_queue_reconciliation(error: &str, failures: u32) -> bool {
    let error = error.to_ascii_lowercase();
    ["retained", "prun", "not found", "cannot find header", "incoherent queue", "needs reconciliation"]
        .iter().any(|pattern| error.contains(pattern))
        || (failures >= 3 && error.contains("timed out"))
}
pub(super) const COLLECTION_WINNER_HEAD_START_DAA: u64 = 30;

pub(super) fn recovery_action(session: &Session, daa: u64, owner: &[u8], offers: &[super::queue_book::LocalOffer]) -> Option<GameAction> {
    match session.stage.as_ref()? {
        stage if stage.restoring_set(owner).is_some() => Some(GameAction::RestoreBank),
        QueueStage::Lobby { .. } | QueueStage::Invitation{..}
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
        QueueStage::Live(m) if reusable::response_timeout(&m.snapshot()).is_ok_and(|r| r.end_daa >= 0 && daa >= r.end_daa as u64) => {
            Some(GameAction::Timeout)
        }
        QueueStage::Collecting { result, .. }
            if result.state.collection.finish.winner == session.side(owner) as i64
                || daa >= result.utxo.block_daa_score.saturating_add(COLLECTION_WINNER_HEAD_START_DAA) =>
        {
            Some(GameAction::Collect)
        }
        _ => None,
    }
}

// Cursor movement, fee accounting and wallet-only reorgs do not rewind gameplay.
pub(super) fn game_state_changed(before:&Option<Session>,after:&Option<Session>)->bool {
    let stage=|session:&Option<Session>|session.as_ref().map(|s|serde_json::to_vec(&(s.id,&s.stage)).expect("game state serializes"));
    stage(before)!=stage(after)
}
