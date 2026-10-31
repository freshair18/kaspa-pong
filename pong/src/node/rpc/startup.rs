//! Client-lifetime transport ownership. The caller keeps its runtime alive.
use super::*;
use std::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    endpoint: String,
    network: String,
    resolvers: Vec<String>,
    fallbacks: Vec<String>,
    allow_unsynced: bool,
    broadcast: super::broadcast::Settings,
}
impl From<&Config> for Identity {
    fn from(c: &Config) -> Self {
        Self { endpoint: c.endpoint.clone(), network: c.network.clone(), resolvers: c.resolver_urls.clone(),
            fallbacks: c.fallback_endpoints.clone(), allow_unsynced: c.allow_unsynced, broadcast: c.broadcast.clone() }
    }
}
pub fn phase(name: &str, started: Instant) {
    diagnostic(&serde_json::json!({"type":"startup_timing", "phase":name,
        "observed_unix_ms":crate::node::client::now_ms(), "elapsed_ms":started.elapsed().as_millis() as u64}).to_string());
}

/// Alternate-screen diff rendering cannot tolerate unsolicited stderr writes.
/// Interactive background diagnostics share a small append-only process log;
/// redirected use retains stderr for operators and machine-readable tooling.
pub fn diagnostic(message: &str) {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() || !std::io::stderr().is_terminal() {
        eprintln!("{message}");
        return;
    }
    static LOG: std::sync::OnceLock<std::sync::Mutex<Option<std::fs::File>>> = std::sync::OnceLock::new();
    let log = LOG.get_or_init(|| {
        let file = std::fs::create_dir_all("target").and_then(|_| std::fs::OpenOptions::new().create(true).append(true)
            .open(format!("target/startup-{}.jsonl", std::process::id()))).ok();
        std::sync::Mutex::new(file)
    });
    if let Ok(mut log) = log.lock() {
        if let Some(file) = log.as_mut() {
            if message.starts_with('{') { let _ = writeln!(file, "{message}"); }
            else { let _ = writeln!(file, "{}", serde_json::json!({"type":"recovery_notice", "message":message})); }
        }
    }
}

impl Node {
    /// Capture a continuation cursor before reading the exact accepted Lobby.
    /// Hash-only chain checks detect a removed cursor; repeated UTXO reads
    /// detect competing spends and changed consensus acceptance metadata. The
    /// cursor is deliberately NOT advanced: Full tracking replays all later
    /// transitions, with independent covenant execution, from this root.
    pub async fn bootstrap_lobby(&self, a: &argent_runtime::Artifact, invite: &super::super::tracker::Invitation,
        config: &Config, cancelled: impl Fn() -> bool,
    ) -> Result<Option<super::super::client::Cache>> {
        invite.validate(a)?;
        if invite.network != self.network.to_string() { return Err("invitation network differs from node".into()); }
        let started = Instant::now();
        for _ in 0..3 {
            if cancelled() { return Ok(None); }
            let (cursor, first) = self.invitation_lobby(a, invite).await?;
            if cancelled() { return Ok(None); }
            let chain = request(self.rpc.get_virtual_chain_from_block_v2(cursor, Some(RpcDataVerbosityLevel::None), None)).await?;
            let coins = self.utxos(&[first.utxo.script_public_key.clone()]).await?;
            let fresh = super::super::invitations::accepted_lobby(a, invite, &coins)?;
            if !bootstrap_stable(&first.utxo, &fresh.utxo, chain.removed_chain_block_hashes.len(), chain.added_chain_block_hashes.len()) {
                phase("lobby_bootstrap_retry", started);
                continue;
            }
            self.qualify(config, false).await?;
            if cancelled() { return Ok(None); }
            let mut cache = super::super::client::Cache::new(invite);
            cache.tracker.cursor = cursor;
            cache.tracker.daa = self.info().await?.virtual_daa_score;
            cache.tracker.stage = Stage::Lobby(fresh);
            cache.fee_ledger.payments.insert(invite.genesis.id(), super::super::invitations::creation_payment(invite)?);
            cache.validate(a, invite)?;
            phase("lobby_bootstrap_ready", started);
            return Ok(Some(cache));
        }
        Err("Lobby changed during three bootstrap attempts; retry the invitation".into())
    }
}
fn bootstrap_stable(first: &UtxoEntry, last: &UtxoEntry, removed: usize, added: usize) -> bool {
    removed == 0 && added <= 64 && same_utxo(first, last)
}

pub trait Backend: Clone + Send + Sync + 'static {
    type Peer: Clone + Send + Sync + 'static;
    fn connect(&self, config: Config) -> impl std::future::Future<Output = std::result::Result<Self::Peer, String>> + Send;
    fn qualify(&self, node: Self::Peer, config: Config, full: bool) -> impl std::future::Future<Output = bool> + Send;
    fn disconnect(&self, node: Self::Peer) -> impl std::future::Future<Output = ()> + Send;
    fn failed(&self, _node: &Self::Peer) {}
}
#[derive(Clone, Default)]
pub struct Kaspa(Arc<std::sync::atomic::AtomicUsize>);
impl Backend for Kaspa {
    type Peer = Node;
    async fn connect(&self, config: Config) -> std::result::Result<Node, String> {
        Node::connect_from(&config, self.0.load(std::sync::atomic::Ordering::Relaxed)).await.map_err(|e| e.to_string())
    }
    async fn qualify(&self, node: Node, config: Config, full: bool) -> bool {
        let ready = node.qualify(&config, full).await.is_ok();
        if !ready { self.failed(&node); }
        ready
    }
    fn failed(&self, node: &Node) { self.0.store(node.candidate + 1, std::sync::atomic::Ordering::Relaxed); }
    async fn disconnect(&self, node: Node) { let _ = node.disconnect().await; }
}

pub struct Owner<B: Backend = Kaspa> {
    backend: B,
    identity: Option<Identity>,
    node: Option<B::Peer>,
    warming: Option<tokio::task::JoinHandle<std::result::Result<(B::Peer, Instant), String>>>,
    qualified: Option<Instant>,
}
impl<B: Backend + Default> Default for Owner<B> {
    fn default() -> Self { Self { backend: B::default(), identity: None, node: None, warming: None, qualified: None } }
}
impl<B: Backend> Owner<B> {
    pub fn warm(&mut self, config: &Config) {
        assert!(self.warming.is_none() && self.node.is_none());
        self.identity = Some(config.into());
        let config = config.clone();
        let backend = self.backend.clone();
        self.warming = Some(tokio::spawn(async move {
            let started = Instant::now();
            phase("connection_start", started);
            let result = backend.connect(config).await;
            phase(if result.is_ok() { "connection_ready" } else { "connection_failed" }, started);
            result.map(|node| (node, Instant::now()))
        }));
    }
    async fn settle(&mut self) -> Result<()> {
        if let Some(task) = self.warming.take() {
            let (node, qualified) = task.await?.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            self.node = Some(node);
            self.qualified = Some(qualified);
        }
        Ok(())
    }
    pub async fn get(&mut self, config: &Config) -> Result<B::Peer> {
        let started = Instant::now();
        let same = self.identity.as_ref() == Some(&Identity::from(config));
        // Finish any old open attempt before replacing it: aborting connect can
        // strand SDK background workers before a transport handle is returned.
        let settled = self.settle().await.is_ok();
        if !same {
            if let Some(n) = self.node.take() { self.backend.disconnect(n).await; }
            self.qualified = None;
        } else if !settled {
            self.node = None;
        }
        if let Some(n) = &self.node {
            let stale = self.qualified.is_none_or(|at| at.elapsed() >= Duration::from_secs(5));
            if self.backend.qualify(n.clone(), config.clone(), stale).await {
                if stale { self.qualified = Some(Instant::now()); }
                phase("connection_reuse", started);
                return Ok(n.clone());
            }
            let n = self.node.take().unwrap();
            self.backend.disconnect(n).await;
            phase("connection_reconnect", started);
        }
        self.warm(config);
        self.settle().await?;
        Ok(self.node.as_ref().unwrap().clone())
    }
    /// Gameplay may have replaced a failed peer. Its workers are drained first.
    pub fn retain(&mut self, node: Option<B::Peer>) {
        if node.is_none() { if let Some(old) = &self.node { self.backend.failed(old); } }
        self.node = node;
        self.qualified = None;
    }
    pub async fn close(&mut self) {
        let _ = self.settle().await;
        if let Some(n) = self.node.take() { self.backend.disconnect(n).await; }
    }
}

/// Cloneable client-lifetime owner. Connection work is single-flight; no state
/// mutex is held across RPCs and callers never borrow the owner for a scan.
pub struct Connection<B: Backend = Kaspa> {
    shared: Arc<std::sync::Mutex<Shared<B>>>,
}
struct Shared<B: Backend> {
    owner: Option<Owner<B>>,
    flight: Option<(Identity, tokio::sync::watch::Receiver<Option<std::result::Result<B::Peer, String>>>)>,
    retained: Option<Option<B::Peer>>,
    broadcast_nodes: Option<usize>,
}
impl<B: Backend> Clone for Connection<B> {
    fn clone(&self) -> Self { Self { shared: self.shared.clone() } }
}
impl<B: Backend + Default> Default for Connection<B> {
    fn default() -> Self {
        Self { shared: Arc::new(std::sync::Mutex::new(Shared { owner: Some(Owner::default()), flight: None, retained: None, broadcast_nodes: None })) }
    }
}
impl<B: Backend> Connection<B> {
    /// Session-wide override shared by menu, fee, recovery and game callers.
    pub fn set_broadcast_nodes(&mut self, nodes: usize) -> Result<()> {
        super::broadcast::Settings { nodes, ..Default::default() }.validate()?;
        self.shared.lock().map_err(|_| "connection owner poisoned")?.broadcast_nodes = Some(nodes);
        Ok(())
    }
    pub fn warm(&mut self, config: &Config) {
        let mut s = self.shared.lock().unwrap();
        let mut config = config.clone();
        if let Some(nodes) = s.broadcast_nodes { config.broadcast.nodes = nodes; }
        if let Some(owner) = s.owner.as_mut() {
            if owner.node.is_none() && owner.warming.is_none() { owner.warm(&config); }
        }
    }
    pub async fn get(&mut self, config: &Config) -> Result<B::Peer> {
        loop {
            let mut config = config.clone();
            if let Some(nodes) = self.shared.lock().map_err(|_| "connection owner poisoned")?.broadcast_nodes { config.broadcast.nodes = nodes; }
            let identity = Identity::from(&config);
            let (same, mut rx) = {
                let mut s = self.shared.lock().map_err(|_| "connection owner poisoned")?;
                if let Some((id, rx)) = &s.flight { (*id == identity, rx.clone()) }
                else {
                    let mut owner = s.owner.take().ok_or("connection owner unavailable")?;
                    let config = config.clone();
                    let shared = self.shared.clone();
                    let (tx, rx) = tokio::sync::watch::channel(None);
                    s.flight = Some((identity.clone(), rx.clone()));
                    tokio::spawn(async move {
                        let result = owner.get(&config).await.map_err(|e| format!("gameplay node connection/qualification failed: {e}"));
                        if let Ok(mut s) = shared.lock() {
                            if let Some(peer) = s.retained.take() { owner.retain(peer); }
                            s.owner = Some(owner);
                            s.flight = None;
                        }
                        let _ = tx.send(Some(result));
                    });
                    (true, rx)
                }
            };
            let result = loop {
                if let Some(result) = rx.borrow().clone() { break result; }
                rx.changed().await.map_err(|_| "connection worker stopped")?;
            };
            if same {
                // A menu change may have arrived while this open attempt ran.
                if self.shared.lock().map_err(|_| "connection owner poisoned")?.broadcast_nodes.is_some_and(|nodes| nodes != config.broadcast.nodes) { continue; }
                return result.map_err(Into::into);
            }
        }
    }
    pub fn retain(&mut self, peer: Option<B::Peer>) {
        let mut s = self.shared.lock().unwrap();
        if let Some(owner) = s.owner.as_mut() { owner.retain(peer); }
        else { s.retained = Some(peer); }
    }
    pub async fn close(&mut self) {
        let flight = self.shared.lock().unwrap().flight.as_ref().map(|(_, rx)| rx.clone());
        if let Some(mut rx) = flight {
            while rx.borrow().is_none() { if rx.changed().await.is_err() { break; } }
        }
        let owner = self.shared.lock().unwrap().owner.take();
        if let Some(mut owner) = owner { owner.close().await; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cloned_menu_and_recovery_handles_share_one_connection_without_scan_ownership() {
        let backend = Fake::default();
        let owner = Owner { backend: backend.clone(), ..Default::default() };
        let mut menu = Connection { shared: Arc::new(std::sync::Mutex::new(Shared { owner: Some(owner), flight: None, retained: None, broadcast_nodes: None })) };
        let mut recovery = menu.clone();
        let c = Config::default();
        let (a, b) = tokio::join!(menu.get(&c), recovery.get(&c));
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(backend.0.lock().unwrap().0, 1);
        // Recovery keeps its peer during an indefinitely stalled history read;
        // no owner/connection guard is borrowed by that read.
        let stalled = tokio::spawn(async move { std::future::pending::<()>().await });
        let started = Instant::now();
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), menu.get(&c)).await.unwrap().unwrap(), 1);
        assert!(!stalled.is_finished());
        phase("synthetic_menu_reuse_while_recovery_stalled", started);
        stalled.abort(); let _ = stalled.await;
        menu.close().await;
    }
    #[tokio::test]
    async fn broadcast_limit_replaces_pool_for_all_callers_despite_stale_configs() {
        #[derive(Clone, Default)]
        struct Recording(Arc<std::sync::Mutex<Vec<usize>>>);
        impl Backend for Recording {
            type Peer = usize;
            async fn connect(&self, c: Config) -> std::result::Result<usize, String> {
                let mut nodes = self.0.lock().unwrap(); nodes.push(c.broadcast.nodes); Ok(nodes.len())
            }
            async fn qualify(&self, _: usize, _: Config, _: bool) -> bool { true }
            async fn disconnect(&self, _: usize) {}
        }
        let backend = Recording::default();
        let owner = Owner { backend: backend.clone(), ..Default::default() };
        let mut menu = Connection { shared: Arc::new(std::sync::Mutex::new(Shared { owner: Some(owner), flight: None, retained: None, broadcast_nodes: None })) };
        let mut recovery = menu.clone();
        let config = Config::default();
        assert_eq!(menu.get(&config).await.unwrap(), 1);
        menu.set_broadcast_nodes(1).unwrap();
        assert_eq!(menu.get(&config).await.unwrap(), 2);
        assert_eq!(recovery.get(&config).await.unwrap(), 2, "old recovery config must not restore the default peer count");
        assert!(menu.set_broadcast_nodes(0).is_err());
        assert!(menu.set_broadcast_nodes(9).is_err());
        assert_eq!(recovery.get(&config).await.unwrap(), 2);
        menu.set_broadcast_nodes(8).unwrap();
        assert_eq!(recovery.get(&config).await.unwrap(), 3);
        assert_eq!(*backend.0.lock().unwrap(), vec![5, 1, 8]);
        menu.close().await;
    }
    #[derive(Clone, Default)]
    struct Fake(Arc<std::sync::Mutex<(usize, usize, Vec<bool>, bool)>>);
    impl Backend for Fake {
        type Peer = usize;
        async fn connect(&self, _: Config) -> std::result::Result<usize, String> {
            tokio::task::yield_now().await;
            let mut s = self.0.lock().unwrap(); s.0 += 1; Ok(s.0)
        }
        async fn qualify(&self, _: usize, _: Config, full: bool) -> bool {
            let mut s = self.0.lock().unwrap(); s.2.push(full); !s.3
        }
        async fn disconnect(&self, _: usize) { self.0.lock().unwrap().1 += 1; }
    }
    #[tokio::test]
    async fn lifecycle_warm_reuse_stale_failure_configuration_change_and_close() {
        let backend = Fake::default();
        let mut owner = Owner { backend: backend.clone(), ..Default::default() };
        let config = Config::default();
        owner.warm(&config);
        assert_eq!(owner.get(&config).await.unwrap(), 1);
        assert_eq!(owner.get(&config).await.unwrap(), 1);
        assert_eq!(backend.0.lock().unwrap().0, 1, "warmup and launch share one open attempt");
        owner.qualified = Some(Instant::now() - Duration::from_secs(6));
        assert_eq!(owner.get(&config).await.unwrap(), 1);
        assert_eq!(backend.0.lock().unwrap().2, [false, false, true]);
        backend.0.lock().unwrap().3 = true;
        assert_eq!(owner.get(&config).await.unwrap(), 2, "failed qualification replaces the peer");
        backend.0.lock().unwrap().3 = false;
        let changed = Config { fallback_endpoints: vec!["grpc://other:16610".into()], ..config.clone() };
        assert_eq!(owner.get(&changed).await.unwrap(), 3);
        owner.retain(Some(3));
        assert_eq!(owner.get(&changed).await.unwrap(), 3, "gameplay hands its peer back");
        assert!(*backend.0.lock().unwrap().2.last().unwrap(), "handoff refreshes qualification");
        owner.close().await;
        let s = backend.0.lock().unwrap();
        assert_eq!((s.0,s.1), (3,3));
    }
    #[tokio::test]
    async fn changed_config_drains_inflight_warmup_before_opening_replacement() {
        let backend = Fake::default();
        let mut owner = Owner { backend: backend.clone(), ..Default::default() };
        let c = Config::default();
        owner.warm(&c);
        let changed = Config { endpoint: "resolver".into(), ..c };
        assert_eq!(owner.get(&changed).await.unwrap(), 2);
        assert_eq!(backend.0.lock().unwrap().1, 1);
        owner.close().await;
        assert_eq!(backend.0.lock().unwrap().1, 2);
    }
    #[tokio::test]
    async fn cancelled_bootstrap_never_queries_or_opens_a_transport() {
        use kaspa_consensus_core::tx::TransactionOutpoint;
        use secp256k1::{Keypair, Secp256k1, SecretKey};
        let key = |n| Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n;32]).unwrap());
        let a = serde_json::from_str(include_str!("../../../artifacts/artifact.json")).unwrap();
        let lobby = Lobby { left: pubkey(&key(1)), right: pubkey(&key(2)), refund: pubkey(&key(3)),
            control_value: SIDE_DEPOSIT as i64, ball_tick_daa: DEFAULT_BALL_TICK_DAA, stake_per_player: DEFAULT_STAKE, ball_count: 1, win_score: 3 };
        let funding = Funding { outpoint: TransactionOutpoint::new(Hash::from_bytes([9;32]), 0),
            utxo: UtxoEntry::new(2_000_000_000, p2pk(&pubkey(&key(3))), 1, false, None) };
        let invite = crate::node::invitations::create(&a, "devnet", Hash::from_bytes([8;32]), &lobby, &funding, &key(1), &key(3), 100_000).unwrap();
        // No connect() is called. A non-cancelled query would fail against this
        // deliberately unopened SDK transport instead of touching any node.
        let client = KaspaRpcClient::new(WrpcEncoding::Borsh, Some("ws://127.0.0.1:1"), None, Some("devnet".parse().unwrap()), None).unwrap();
        let n = Node { rpc: Arc::new(RpcTransport::Wrpc(client)), endpoint: "ws://127.0.0.1:1".into(), network: "devnet".parse().unwrap(), prefix: Prefix::Devnet, candidate:0, broadcast: None };
        assert!(n.bootstrap_lobby(&a, &invite, &Config::default(), || true).await.unwrap().is_none());
    }
    #[test]
    fn bootstrap_retries_reorg_retiming_and_excessive_cursor_distance() {
        let entry = UtxoEntry::new(1, crate::engine::p2pk(&[1;32]), 20, false, None);
        assert!(bootstrap_stable(&entry, &entry, 0, 64));
        assert!(!bootstrap_stable(&entry, &entry, 1, 0));
        assert!(!bootstrap_stable(&entry, &entry, 0, 65));
        let mut retimed = entry.clone();
        retimed.block_daa_score += 1;
        assert!(!bootstrap_stable(&entry, &retimed, 0, 0));
    }
    #[test]
    fn configuration_identity_preserves_resolver_and_fallback_policy() {
        let c = Config::default();
        let identity = Identity::from(&c);
        for which in 0..7 {
            let mut changed = c.clone();
            match which {
                0 => changed.endpoint = "resolver".into(),
                1 => changed.network = "testnet-10".into(),
                2 => changed.resolver_urls.push("https://resolver.example".into()),
                3 => changed.fallback_endpoints.push("grpc://localhost:16610".into()),
                4 => changed.allow_unsynced = true,
                5 => changed.broadcast.nodes = 1,
                _ => changed.broadcast.log = "target/other-broadcast.jsonl".into(),
            }
            assert_ne!(identity, Identity::from(&changed));
        }
        let mut directory = c.clone();
        directory.invitation_index = Some("http://directory.example".into());
        assert_eq!(identity, Identity::from(&directory), "directory is a separate service");
    }
}
