//! Resolver-backed submission-only RPC pool. The observer remains authoritative.
use super::{Node, RpcTransport, request};
use crate::{
    engine::Result,
    node::{Config, client::now_ms},
};
use kaspa_consensus_core::{Hash, tx::Transaction};
use kaspa_rpc_core::RpcTransaction;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    io::Write,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{Semaphore, oneshot, watch},
    task::JoinSet,
};

const CONNECT_PARALLELISM: usize = 16;
const REFRESH: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Total submission targets, INCLUDING the normal observer. One is the control.
    pub nodes: usize,
    /// Optional explicit gRPC or Borsh wRPC targets in addition to the resolver.
    pub endpoints: Vec<String>,
    pub log: PathBuf,
}
impl Default for Settings {
    fn default() -> Self {
        Self { nodes: 5, endpoints: Vec::new(), log: "target/broadcast.jsonl".into() }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if !(1..=8).contains(&self.nodes) {
            return Err("broadcast.nodes must be between 1 and 8 (including the observer)".into());
        }
        for endpoint in &self.endpoints {
            endpoint_key(endpoint)?;
        }
        if self.log.as_os_str().is_empty() {
            return Err("broadcast.log must name a JSONL file".into());
        }
        Ok(())
    }
}

fn endpoint_key(endpoint: &str) -> Result<String> {
    let mut url = url::Url::parse(endpoint)?;
    if !matches!(url.scheme(), "grpc" | "ws" | "wss")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("broadcast endpoint must be a gRPC or Borsh wRPC URL without credentials or fragments".into());
    }
    if url.path() == "/" {
        url.set_path("");
    }
    Ok(url.to_string())
}

/// Disk work is isolated from submission and the UI. A full log queue never blocks a move.
struct Log {
    sender: Option<mpsc::SyncSender<Value>>,
    worker: Option<std::thread::JoinHandle<()>>,
    run: String,
    dropped: AtomicUsize,
}
impl Log {
    fn open(path: &std::path::Path) -> Result<Arc<Self>> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        // Windows LockFileEx needs read/write access; append-only handles fail.
        let file = std::fs::OpenOptions::new().create(true).read(true).append(true).open(path)?;
        let (sender, receiver) = mpsc::sync_channel::<Value>(8192);
        let worker = std::thread::spawn(move || {
            let mut file = file;
            loop {
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(row) => {
                        let result = (|| -> std::io::Result<()> {
                            let mut bytes = serde_json::to_vec(&row)?;
                            bytes.push(b'\n');
                            // Two terminal clients may share the default log.
                            file.lock()?;
                            let written = file.write_all(&bytes);
                            let unlocked = file.unlock();
                            written.and(unlocked)
                        })();
                        if let Err(error) = result {
                            super::startup::diagnostic(&format!("broadcast log write failed: {error}"));
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if file.flush().is_err() {
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            if let Err(error) = file.flush() {
                super::startup::diagnostic(&format!("broadcast log flush failed: {error}"));
            }
        });
        Ok(Arc::new(Self {
            sender: Some(sender),
            worker: Some(worker),
            run: format!("{}-{}", std::process::id(), now_ms()),
            dropped: AtomicUsize::new(0),
        }))
    }
    fn event(&self, mut row: Value) {
        row["run"] = self.run.clone().into();
        row["observed_unix_ms"] = now_ms().into();
        if self.sender.as_ref().is_none_or(|s| s.try_send(row).is_err()) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
impl Drop for Log {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let dropped = self.dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            super::startup::diagnostic(&format!("broadcast log dropped {dropped} timing records"));
        }
    }
}

#[derive(Clone)]
struct Peer {
    node: Node,
    id: String,
    // Slow peers cannot accumulate an unbounded queue of stale transactions.
    slots: Arc<Semaphore>,
}
struct State {
    peers: Mutex<Vec<Peer>>,
    stopping: AtomicBool,
    active: AtomicUsize,
    slots: Arc<Semaphore>,
    log: Arc<Log>,
    target: usize,
    gate: Mutex<()>,
}
pub struct Pool {
    state: Arc<State>,
    stop: watch::Sender<bool>,
    worker: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Pool {
    pub fn start(config: &Config, primary: &Node) -> Result<Option<Arc<Self>>> {
        // Disposable devnet tests never contact public services implicitly.
        let public = matches!(
            primary.network.network_type,
            kaspa_consensus_core::network::NetworkType::Mainnet | kaspa_consensus_core::network::NetworkType::Testnet
        );
        if !public && config.broadcast.endpoints.is_empty() {
            return Ok(None);
        }
        let log = Log::open(&config.broadcast.log)?;
        let state = Arc::new(State {
            peers: Mutex::new(Vec::new()),
            stopping: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            slots: Arc::new(Semaphore::new(8)),
            log,
            target: config.broadcast.nodes,
            gate: Mutex::new(()),
        });
        state.log.event(json!({"type":"broadcast_start", "network":config.network, "target_nodes":state.target,
            "primary":primary.endpoint}));
        let (stop, receiver) = watch::channel(false);
        let worker = if state.target > 1 {
            Some(tokio::spawn(maintain(state.clone(), config.clone(), primary.clone(), receiver)))
        } else {
            None
        };
        Ok(Some(Arc::new(Self { state, stop, worker: tokio::sync::Mutex::new(worker) })))
    }
    pub fn status(&self) -> Value {
        json!({"target_nodes":self.state.target, "connected_nodes":1 + self.state.peers.lock().unwrap().len(),
            "active_submissions":self.state.active.load(Ordering::Relaxed), "log_run":self.state.log.run})
    }
    pub async fn submit(&self, primary: &Node, tx: &Transaction) -> Result<Hash> {
        let permit = self.state.slots.clone().acquire_owned().await.map_err(|_| "broadcast pool closed")?;
        let _gate = self.state.gate.lock().unwrap();
        if self.state.stopping.load(Ordering::Acquire) {
            return Err("broadcast pool is stopping; no submission started".into());
        }
        let peers = self.state.peers.lock().unwrap().clone();
        let id = tx.id();
        let rpc_tx = Arc::new(RpcTransaction::from(tx));
        let mut attempts = vec![attempt(primary.rpc.clone(), primary.current_endpoint(), true, rpc_tx.clone(), id, None)];
        let mut skipped = 0;
        for peer in peers {
            match peer.slots.clone().try_acquire_owned() {
                Ok(slot) => attempts.push(attempt(peer.node.rpc.clone(), peer.node.endpoint, false, rpc_tx.clone(), id, Some(slot))),
                Err(_) => skipped += 1,
            }
        }
        self.state.log.event(json!({"type":"broadcast_submit", "txid":id, "target_nodes":self.state.target,
            "attempted_nodes":attempts.len(), "busy_nodes_skipped":skipped,
            "signature_script_bytes":tx.inputs.iter().map(|i| i.signature_script.len()).sum::<usize>()}));
        self.state.active.fetch_add(1, Ordering::AcqRel);
        let state = self.state.clone();
        let (sender, receiver) = oneshot::channel();
        // Keep sending the already-authorized, identical bytes after the first
        // ack or caller cancellation. A failure is returned only after ALL attempts end.
        tokio::spawn(async move {
            let _permit = permit;
            run_attempts(attempts, id, state.log.clone(), sender).await;
            state.active.fetch_sub(1, Ordering::AcqRel);
        });
        drop(_gate);
        receiver.await.map_err(|_| "broadcast worker stopped; submission outcome uncertain")?.map_err(Into::into)
    }
    pub async fn close(&self) {
        {
            let _gate = self.state.gate.lock().unwrap();
            self.state.stopping.store(true, Ordering::Release);
            self.state.slots.close();
        }
        let _ = self.stop.send(true);
        // Every attempt is bounded to five seconds. Finish exposed submissions
        // before disconnecting their transports, including after the first ack.
        while self.state.active.load(Ordering::Acquire) != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if let Some(worker) = self.worker.lock().await.take() {
            let _ = worker.await;
        }
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        self.state.stopping.store(true, Ordering::Release);
        self.state.slots.close();
        let _ = self.stop.send(true);
    }
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Acknowledged,
    AlreadyKnown,
    Failed(String),
}
fn classify(result: std::result::Result<Hash, String>, expected: Hash) -> Outcome {
    match result {
        Ok(hash) if hash == expected => Outcome::Acknowledged,
        Ok(_) => Outcome::Failed("RPC returned a different transaction ID".into()),
        Err(error) => {
            let lower = error.to_ascii_lowercase();
            let exact = format!("transaction {expected}");
            if lower.contains(&format!("{exact} is already in the mempool"))
                || lower.contains(&format!("{exact} was already accepted by the consensus"))
            {
                Outcome::AlreadyKnown
            } else {
                Outcome::Failed(error)
            }
        }
    }
}
struct Attempt {
    endpoint: String,
    primary: bool,
    future: Pin<Box<dyn Future<Output = Outcome> + Send>>,
}
fn attempt(
    rpc: Arc<RpcTransport>,
    endpoint: String,
    primary: bool,
    tx: Arc<RpcTransaction>,
    id: Hash,
    slot: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Attempt {
    Attempt {
        endpoint,
        primary,
        future: Box::pin(async move {
            let _slot = slot;
            classify(request(rpc.submit_transaction((*tx).clone(), false)).await.map_err(|e| e.to_string()), id)
        }),
    }
}
async fn run_attempts(attempts: Vec<Attempt>, id: Hash, log: Arc<Log>, sender: oneshot::Sender<std::result::Result<Hash, String>>) {
    let start = Instant::now();
    let count = attempts.len();
    let mut jobs = JoinSet::new();
    for attempt in attempts {
        jobs.spawn(async move {
            let began = Instant::now();
            let result = tokio::time::timeout(Duration::from_secs(5), attempt.future)
                .await
                .unwrap_or_else(|_| Outcome::Failed("submission timed out; outcome uncertain".into()));
            (attempt.endpoint, attempt.primary, began.elapsed().as_micros() as u64, result)
        });
    }
    let mut sender = Some(sender);
    let mut successes = 0;
    let mut known = 0;
    let mut failed = 0;
    let mut first_ack_us = None;
    let mut primary_ack_us = None;
    while let Some(result) = jobs.join_next().await {
        let (endpoint, primary, request_us, outcome) = match result {
            Ok(result) => result,
            Err(error) => {
                failed += 1;
                log.event(json!({"type":"broadcast_peer", "txid":id, "status":"worker_failed", "error":error.to_string()}));
                continue;
            }
        };
        let elapsed = start.elapsed().as_micros() as u64;
        let (status, error) = match &outcome {
            Outcome::Acknowledged => {
                successes += 1;
                ("acknowledged", None)
            }
            Outcome::AlreadyKnown => {
                known += 1;
                ("already_known", None)
            }
            Outcome::Failed(error) => {
                failed += 1;
                ("failed", Some(error))
            }
        };
        if !matches!(outcome, Outcome::Failed(_)) {
            first_ack_us.get_or_insert(elapsed);
            if primary {
                primary_ack_us = Some(elapsed);
            }
            if let Some(sender) = sender.take() {
                let _ = sender.send(Ok(id));
            }
        }
        log.event(json!({"type":"broadcast_peer", "txid":id, "endpoint":endpoint, "primary":primary,
            "request_us":request_us, "elapsed_us":elapsed, "status":status, "error":error}));
    }
    log.event(json!({"type":"broadcast_complete", "txid":id, "attempted_nodes":count,
        "acknowledged":successes, "already_known":known, "failed":failed,
        "first_ack_us":first_ack_us, "primary_ack_us":primary_ack_us, "elapsed_us":start.elapsed().as_micros() as u64}));
    // Do not propagate a single peer's "rejected transaction" text: the Lobby
    // creator interprets it as proof of non-exposure and releases reservations.
    if let Some(sender) = sender {
        let _ = sender.send(Err(format!("no broadcast node acknowledged transaction {id} ({count} attempts); outcome uncertain; recovery authorization retained; see broadcast log")));
    }
}

async fn maintain(state: Arc<State>, config: Config, primary: Node, mut stop: watch::Receiver<bool>) {
    let primary_key = endpoint_key(&primary.endpoint).unwrap_or_else(|_| primary.endpoint.clone());
    let mut cooldown: HashMap<String, Instant> = HashMap::new();
    let public = config.network == "mainnet" || config.network.starts_with("testnet-");
    let resolver = super::Resolver::new(
        (!config.resolver_urls.is_empty()).then(|| config.resolver_urls.iter().cloned().map(Arc::new).collect()),
        true,
    );
    loop {
        if *stop.borrow() {
            break;
        }
        let round_started = Instant::now();
        cooldown.retain(|_, at| at.elapsed() < Duration::from_secs(120));
        let identity = request(primary.rpc.get_info()).await.map_err(|e| e.to_string());
        let primary_id = match identity {
            Ok(info) if !info.p2p_id.is_empty() => info.p2p_id,
            _ => {
                state.log.event(
                    json!({"type":"broadcast_discovery_error", "error":"cannot identify observer; deferring fan-out discovery"}),
                );
                tokio::select! { _ = stop.changed() => break, _ = tokio::time::sleep(Duration::from_secs(15)) => {} }
                continue;
            }
        };
        let candidates = config.broadcast.endpoints.iter().filter_map(|u| endpoint_key(u).ok()).collect::<Vec<_>>();
        if *stop.borrow() {
            break;
        }
        // Requalify in the background; no status RPC is inserted before each send.
        let mut health = JoinSet::new();
        for peer in state.peers.lock().unwrap().clone() {
            health.spawn(async move {
                let healthy =
                    request(peer.node.rpc.get_server_info()).await.is_ok_and(|i| i.network_id == peer.node.network && i.is_synced);
                (peer, healthy)
            });
        }
        while let Some(Ok((peer, healthy))) = health.join_next().await {
            if !healthy {
                state.peers.lock().unwrap().retain(|p| p.node.endpoint != peer.node.endpoint);
                let _ = peer.node.rpc.disconnect().await;
            }
        }
        let peers = state.peers.lock().unwrap().clone();
        let mut ids: HashSet<_> = peers.iter().map(|p| p.id.clone()).chain(std::iter::once(primary_id)).collect();
        let mut endpoints: HashSet<_> = peers.iter().map(|p| p.node.endpoint.clone()).collect();
        endpoints.insert(primary_key.clone());
        let mut candidates = candidates
            .into_iter()
            .filter(|u| !endpoints.contains(u) && cooldown.get(u).is_none_or(|at| at.elapsed() >= Duration::from_secs(120)))
            .collect::<Vec<_>>()
            .into_iter();
        let seen = Arc::new(Mutex::new(endpoints));
        let cooling: Arc<HashSet<_>> =
            Arc::new(cooldown.iter().filter(|(_, at)| at.elapsed() < Duration::from_secs(120)).map(|(u, _)| u.clone()).collect());
        // Bounded discovery rather than claiming a requested N is available.
        // Connected load-balancing endpoints are deduplicated before opening sockets.
        let mut lookups_left = if public { state.target * 4 } else { 0 };
        let mut resolver_queries = 0;
        let mut stalled = 0;
        let mut duplicates = 0;
        let mut connecting = JoinSet::new();
        loop {
            while !*stop.borrow()
                && connecting.len() < CONNECT_PARALLELISM
                && state.peers.lock().unwrap().len() + connecting.len() < state.target - 1
                && stalled < 64
            {
                let explicit = candidates.next();
                if explicit.is_none() {
                    if lookups_left == 0 {
                        break;
                    }
                    lookups_left -= 1;
                    resolver_queries += 1;
                }
                let c = config.clone();
                let resolver = resolver.clone();
                let seen = seen.clone();
                let cooling = cooling.clone();
                connecting.spawn(async move {
                    let url = match explicit {
                        Some(url) => url,
                        None => match tokio::time::timeout(
                            Duration::from_secs(5),
                            resolver.get_url(super::WrpcEncoding::Borsh, primary.network),
                        )
                        .await
                        {
                            Ok(Ok(url)) if url.starts_with("wss://") => match endpoint_key(&url) {
                                Ok(url) => url,
                                Err(_) => return None,
                            },
                            _ => return Some(("resolver".into(), Err("public-node discovery failed or timed out".into()))),
                        },
                    };
                    if cooling.contains(&url) || !seen.lock().unwrap().insert(url.clone()) {
                        // Avoid hammering the resolver when it repeatedly elects the same least-loaded node.
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        return None;
                    }
                    let connected = Node::connect_one(&c, &url, false).await.map_err(|e| e.to_string());
                    let result = match connected {
                        Ok(node) => match request(node.rpc.get_info()).await.map_err(|e| e.to_string()) {
                            Ok(info) if !info.p2p_id.is_empty() => {
                                Ok(Peer { node, id: info.p2p_id, slots: Arc::new(Semaphore::new(3)) })
                            }
                            result => {
                                let _ = node.rpc.disconnect().await;
                                Err(format!("missing peer identity: {result:?}"))
                            }
                        },
                        Err(error) => Err(error),
                    };
                    Some((url, result))
                });
            }
            let Some(result) = connecting.join_next().await else {
                break;
            };
            stalled += 1;
            if let Ok(Some((url, result))) = result {
                match result {
                    Ok(peer) if !*stop.borrow() && ids.insert(peer.id.clone()) => {
                        state.log.event(json!({"type":"broadcast_connected", "endpoint":url, "peer_id":peer.id}));
                        state.peers.lock().unwrap().push(peer);
                        stalled = 0;
                    }
                    Ok(peer) => {
                        let _ = peer.node.rpc.disconnect().await;
                    }
                    Err(error) => {
                        state.log.event(json!({"type":"broadcast_connect_error", "endpoint":url, "error":error}));
                        cooldown.insert(url, Instant::now());
                    }
                }
            } else {
                duplicates += 1;
            }
        }
        state.log.event(json!({"type":"broadcast_pool", "target_nodes":state.target,
            "connected_nodes":1 + state.peers.lock().unwrap().len(), "resolver_queries":resolver_queries,
            "duplicate_elections":duplicates, "round_ms":round_started.elapsed().as_millis() as u64}));
        let refresh = if state.peers.lock().unwrap().len() + 1 < state.target { Duration::from_secs(15) } else { REFRESH };
        tokio::select! { _ = stop.changed() => break, _ = tokio::time::sleep(refresh) => {} }
    }
    while state.active.load(Ordering::Acquire) != 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let peers = std::mem::take(&mut *state.peers.lock().unwrap());
    let mut closing = JoinSet::new();
    for peer in peers {
        closing.spawn(async move {
            let _ = peer.node.rpc.disconnect().await;
        });
    }
    while closing.join_next().await.is_some() {}
    state.log.event(json!({"type":"broadcast_closed"}));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicates_only_count_when_the_exact_transaction_is_known() {
        let id = Hash::from_bytes([1; 32]);
        assert_eq!(classify(Ok(id), id), Outcome::Acknowledged);
        assert_eq!(
            classify(Err(format!("Rejected transaction {id}: transaction {id} is already in the mempool")), id),
            Outcome::AlreadyKnown
        );
        assert_eq!(classify(Err(format!("transaction {id} was already accepted by the consensus")), id), Outcome::AlreadyKnown);
        assert!(matches!(classify(Ok(Hash::from_bytes([2; 32])), id), Outcome::Failed(_)));
        assert!(matches!(classify(Err("output already spent by transaction".into()), id), Outcome::Failed(_)));
        assert!(matches!(classify(Err("transaction unrelated is already in the mempool".into()), id), Outcome::Failed(_)));
    }
    #[test]
    fn settings_enforce_fanout_bounds_and_explicit_endpoint_schemes() {
        let mut s = Settings::default();
        assert_eq!(s.nodes, 5);
        assert!(s.validate().is_ok());
        s.nodes = 0;
        assert!(s.validate().is_err());
        s.nodes = 9;
        assert!(s.validate().is_err());
        s.nodes = 1;
        s.endpoints = vec!["resolver".into()];
        assert!(s.validate().is_err());
        assert_eq!(endpoint_key("wss://NODE.example:443/").unwrap(), endpoint_key("wss://node.example").unwrap());
        assert!(endpoint_key("wss://user:secret@node.example").is_err());
    }
    fn fake(primary: bool, future: impl Future<Output = Outcome> + Send + 'static) -> Attempt {
        Attempt { endpoint: format!("fake-{primary}"), primary, future: Box::pin(future) }
    }
    #[tokio::test]
    async fn fastest_success_returns_before_slow_primary_and_remaining_sends_finish() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log::open(&dir.path().join("log")).unwrap();
        let id = Hash::from_bytes([3; 32]);
        let (slow, waiting) = oneshot::channel();
        let (sender, result) = oneshot::channel();
        let task = tokio::spawn(run_attempts(
            vec![
                fake(true, async {
                    let _ = waiting.await;
                    Outcome::Acknowledged
                }),
                fake(false, async { Outcome::Acknowledged }),
            ],
            id,
            log.clone(),
            sender,
        ));
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), result).await.unwrap().unwrap().unwrap(), id);
        assert!(!task.is_finished());
        slow.send(()).unwrap();
        task.await.unwrap();
        drop(log);
        let text = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert_eq!(text.lines().filter(|s| s.contains("broadcast_peer")).count(), 2);
        assert!(text.contains("broadcast_complete"));
    }
    #[tokio::test]
    async fn primary_rejection_waits_for_other_outcomes_and_failure_preserves_authorization() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log::open(&dir.path().join("log")).unwrap();
        let id = Hash::from_bytes([4; 32]);
        let (release, pending) = oneshot::channel();
        let (sender, mut result) = oneshot::channel();
        let task = tokio::spawn(run_attempts(
            vec![
                fake(true, async { Outcome::Failed("rejected transaction".into()) }),
                fake(false, async {
                    let _ = pending.await;
                    Outcome::Failed("timeout".into())
                }),
            ],
            id,
            log,
            sender,
        ));
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut result).await.is_err());
        release.send(()).unwrap();
        let error = result.await.unwrap().unwrap_err();
        assert!(error.contains("outcome uncertain"));
        assert!(!error.contains("rejected transaction"));
        task.await.unwrap();
    }
    #[tokio::test]
    async fn caller_cancellation_does_not_drop_other_authorized_sends() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log::open(&dir.path().join("log")).unwrap();
        let (sender, receiver) = oneshot::channel();
        drop(receiver);
        let done = Arc::new(AtomicBool::new(false));
        let finished = done.clone();
        run_attempts(
            vec![fake(false, async move {
                finished.store(true, Ordering::Relaxed);
                Outcome::AlreadyKnown
            })],
            Hash::from_bytes([5; 32]),
            log,
            sender,
        )
        .await;
        assert!(done.load(Ordering::Relaxed));
    }
    #[tokio::test]
    async fn eight_targets_start_concurrently_and_all_complete() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log::open(&dir.path().join("log")).unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(9));
        let completed = Arc::new(AtomicUsize::new(0));
        let attempts = (0..8)
            .map(|i| {
                let barrier = barrier.clone();
                let completed = completed.clone();
                fake(i == 0, async move {
                    barrier.wait().await;
                    completed.fetch_add(1, Ordering::Relaxed);
                    Outcome::Acknowledged
                })
            })
            .collect();
        let id = Hash::from_bytes([6; 32]);
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(run_attempts(attempts, id, log.clone(), sender));
        tokio::time::timeout(Duration::from_secs(2), barrier.wait()).await.unwrap();
        assert_eq!(receiver.await.unwrap().unwrap(), id);
        task.await.unwrap();
        drop(log);
        assert_eq!(completed.load(Ordering::Relaxed), 8);
        let rows: Vec<Value> =
            std::fs::read_to_string(dir.path().join("log")).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(rows.last().unwrap()["acknowledged"], 8);
        assert_eq!(rows.iter().filter(|r| r["type"] == "broadcast_peer").count(), 8);
    }
}
