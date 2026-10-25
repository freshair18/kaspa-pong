//! Native Kaspa gRPC and Borsh wRPC, pinned with the covenant's consensus types.
use super::super::engine::*;
use super::{
    Config,
    tracker::{AcceptedTx, ChainBlock, Stage, same_utxo},
};
use kaspa_addresses::{Address, Prefix};
use kaspa_consensus_core::{
    Hash,
    network::{NetworkId, NetworkType},
    tx::{Transaction, UtxoEntry},
};
use kaspa_grpc_client::GrpcClient;
use kaspa_notify::scope::{Scope, VirtualChainChangedScope};
use kaspa_rpc_core::{GetBlockDagInfoResponse, GetServerInfoResponse, Notification, RpcDataVerbosityLevel, RpcTransaction, api::rpc::RpcApi};
use kaspa_txscript::extract_script_pub_key_address;
use kaspa_wrpc_client::{
    KaspaRpcClient, Resolver, WrpcEncoding,
    client::{ConnectOptions, ConnectStrategy},
};
use std::{ops::Deref, str::FromStr, sync::Arc, time::Duration};

pub mod startup;

async fn request<T>(future: impl std::future::Future<Output = kaspa_rpc_core::RpcResult<T>>) -> Result<T> {
    Ok(tokio::time::timeout(Duration::from_secs(5), future).await.map_err(|_| "node RPC request timed out")??)
}

pub enum RpcTransport {
    Grpc(GrpcClient),
    Wrpc(KaspaRpcClient),
}
impl Deref for RpcTransport {
    type Target = dyn RpcApi;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Grpc(c) => c,
            Self::Wrpc(c) => c,
        }
    }
}
impl RpcTransport {
    pub async fn disconnect(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), async {
            match self {
                Self::Grpc(c) => c.disconnect().await?,
                Self::Wrpc(c) => c.disconnect().await?,
            }
            Ok::<(), Box<dyn std::error::Error>>(())
        })
        .await
        .map_err(|_| "node disconnect timed out")?
    }
}

#[derive(Clone)]
pub struct Node {
    pub rpc: Arc<RpcTransport>,
    pub endpoint: String,
    pub network: NetworkId,
    pub prefix: Prefix,
    pub candidate: usize,
}
pub struct Delta {
    pub removed: Vec<Hash>,
    pub added: Vec<ChainBlock>,
    pub common: Option<Hash>,
}
pub struct VirtualChainFeed {
    receiver: async_channel::Receiver<Notification>,
}
impl VirtualChainFeed {
    pub async fn changed(&self) -> Result<()> {
        loop {
            match self.receiver.recv().await? {
                Notification::VirtualChainChanged(_) => {
                    // A burst may contain several selected-chain updates while
                    // one Full delta request is in flight. They all lead to
                    // the same fresh cursor read, so coalesce them.
                    while self.receiver.try_recv().is_ok() {}
                    return Ok(());
                }
                _ => continue,
            }
        }
    }
}
impl Node {
    /// Subscribe before tracking so a new selected-chain block wakes the
    /// follower immediately. The follower still periodically checks its cursor
    /// in case a notification is lost by a remote node.
    pub async fn virtual_chain_feed(&self) -> Result<VirtualChainFeed> {
        let scope = Scope::VirtualChainChanged(VirtualChainChangedScope::new(false));
        request(self.rpc.start_notify(0, scope)).await?;
        let receiver = match self.rpc.as_ref() {
            RpcTransport::Grpc(client) => client.notification_channel_receiver(),
            RpcTransport::Wrpc(client) => client.notification_channel_receiver(),
        };
        Ok(VirtualChainFeed { receiver })
    }
    pub async fn qualify(&self, config: &Config, full: bool) -> Result<()> {
        if self.network.to_string() != config.network { return Err("connection configuration network changed".into()); }
        let info = self.server_info().await?;
        if !config.allow_unsynced && !info.is_synced { return Err("node is not synchronized".into()); }
        if full { self.recent_delta().await?; }
        Ok(())
    }
    pub async fn connect(config: &Config) -> Result<Self> {
        Self::connect_from(config, 0).await
    }
    /// Start after a failed candidate, then try the configured list once around.
    pub async fn connect_from(config: &Config, first: usize) -> Result<Self> {
        config.validate()?;
        let endpoints: Vec<_> = std::iter::once(&config.endpoint).chain(config.fallback_endpoints.iter()).collect();
        let mut errors = Vec::new();
        for offset in 0..endpoints.len() {
            let candidate = (first % endpoints.len() + offset) % endpoints.len();
            let endpoint = endpoints[candidate];
            // A Resolver may choose a different public peer on another query.
            let attempts = if endpoint == "resolver" { 3 } else { 1 };
            for _ in 0..attempts {
                match Self::connect_one(config, endpoint).await {
                    Ok(mut node) => {
                        node.candidate = candidate;
                        return Ok(node);
                    }
                    Err(error) => errors.push(format!("{endpoint}: {error}")),
                }
            }
        }
        Err(format!("unable to connect to a compatible node: {}", errors.join("; ")).into())
    }
    async fn connect_one(config: &Config, endpoint: &str) -> Result<Self> {
        let network = NetworkId::from_str(&config.network)?;
        let (rpc, endpoint) = if endpoint.starts_with("grpc://") {
            let rpc = tokio::time::timeout(
                Duration::from_secs(10),
                GrpcClient::connect_with_args(
                    kaspa_rpc_core::notify::mode::NotificationMode::Direct,
                    endpoint.to_string(),
                    None,
                    false,
                    None,
                    false,
                    Some(5000),
                    Default::default(),
                ),
            )
            .await
            .map_err(|_| "gRPC connection timed out")??;
            (RpcTransport::Grpc(rpc), endpoint.to_string())
        } else {
            let url = if endpoint == "resolver" {
                let resolver = Resolver::new(
                    (!config.resolver_urls.is_empty()).then(|| config.resolver_urls.iter().cloned().map(Arc::new).collect()),
                    true,
                );
                let url = tokio::time::timeout(Duration::from_secs(5), resolver.get_url(WrpcEncoding::Borsh, network))
                    .await
                    .map_err(|_| "public-node discovery timed out")??;
                if !url.starts_with("wss://") {
                    return Err("Resolver returned a non-TLS node URL".into());
                }
                url
            } else {
                endpoint.to_string()
            };
            // Pin this connection to the qualified URL. A later rediscovery
            // must go through the network/index/Full checks again.
            let client = KaspaRpcClient::new(WrpcEncoding::Borsh, Some(&url), None, Some(network), None)?;
            let options = ConnectOptions {
                strategy: ConnectStrategy::Fallback,
                block_async_connect: true,
                connect_timeout: Some(Duration::from_secs(5)),
                ..Default::default()
            };
            match tokio::time::timeout(Duration::from_secs(10), client.connect(Some(options))).await {
                Ok(Ok(_)) => {}
                result => {
                    let _ = tokio::time::timeout(Duration::from_secs(2), client.disconnect()).await;
                    return Err(format!("Borsh wRPC connection failed: {result:?}").into());
                }
            }
            let resolved = client.url().unwrap_or(url);
            (RpcTransport::Wrpc(client), resolved)
        };
        let info = match tokio::time::timeout(Duration::from_secs(5), rpc.get_server_info()).await {
            Ok(Ok(info)) => info,
            result => {
                let _ = rpc.disconnect().await;
                return Err(format!("node handshake failed: {result:?}").into());
            }
        };
        if info.network_id != network {
            let _ = rpc.disconnect().await;
            return Err(format!("node network {} differs from configured {}", info.network_id, network).into());
        }
        if !info.has_utxo_index {
            let _ = rpc.disconnect().await;
            return Err("node must be started with --utxoindex".into());
        }
        if !config.allow_unsynced && !info.is_synced {
            let _ = rpc.disconnect().await;
            return Err("node is not synchronized".into());
        }
        let prefix = match network.network_type {
            NetworkType::Mainnet => Prefix::Mainnet,
            NetworkType::Testnet => Prefix::Testnet,
            NetworkType::Devnet => Prefix::Devnet,
            NetworkType::Simnet => Prefix::Simnet,
        };
        let node = Self { rpc: Arc::new(rpc), endpoint, network, prefix, candidate: 0 };
        if let Err(error) = node.recent_delta().await.map_err(|e| e.to_string()) {
            let _ = node.rpc.disconnect().await;
            return Err(format!("{}: accepted-chain Full API check failed: {error}", node.endpoint).into());
        }
        Ok(node)
    }
    pub fn current_endpoint(&self) -> String {
        match self.rpc.as_ref() {
            RpcTransport::Wrpc(c) => c.url().unwrap_or_else(|| self.endpoint.clone()),
            _ => self.endpoint.clone(),
        }
    }
    pub async fn server_info(&self) -> Result<GetServerInfoResponse> {
        let info = request(self.rpc.get_server_info()).await?;
        // The SDK can reconnect in the background. Recheck peer identity before
        // allowing a synchronized snapshot to authorize another transaction.
        if info.network_id != self.network || !info.has_utxo_index {
            return Err("reconnected node has a different network or no UTXO index".into());
        }
        Ok(info)
    }
    pub async fn info(&self) -> Result<GetBlockDagInfoResponse> {
        request(self.rpc.get_block_dag_info()).await
    }
    pub async fn synced(&self) -> Result<bool> {
        Ok(self.server_info().await?.is_synced)
    }
    pub async fn recent_delta(&self) -> Result<(GetBlockDagInfoResponse, Delta)> {
        let dag = self.info().await?;
        let cursor = self.accepted_cursor_for(&dag).await?;
        Ok((dag, self.delta(cursor).await?))
    }
    /// Snapshot the selected-chain cursor before an inbound code is displayed.
    /// Later Full deltas then contain only games created in response to it.
    pub async fn accepted_cursor(&self) -> Result<Hash> {
        let dag = self.info().await?;
        self.accepted_cursor_for(&dag).await
    }
    async fn accepted_cursor_for(&self, dag: &GetBlockDagInfoResponse) -> Result<Hash> {
        let block = request(self.rpc.get_block(dag.sink, false)).await?;
        Ok(if block.header.parents_by_level.is_empty() {
            dag.sink
        } else {
            block.verbose_data.ok_or("sink lacks selected parent")?.selected_parent_hash
        })
    }
    pub async fn delta(&self, cursor: Hash) -> Result<Delta> {
        // Historical Full batches can contain thousands of blocks. Give catch-up
        // more time than a small live query; the UI/quit thread stays responsive.
        let response = tokio::time::timeout(
            Duration::from_secs(30),
            self.rpc.get_virtual_chain_from_block_v2(cursor, Some(RpcDataVerbosityLevel::Full), None),
        )
        .await
        .map_err(|_| "accepted-chain history request timed out after 30 seconds")??;
        if response.added_chain_block_hashes.len() != response.chain_block_accepted_transactions.len() {
            return Err("incomplete accepted-chain RPC batch".into());
        }
        let mut added = Vec::new();
        for (hash, data) in response.added_chain_block_hashes.iter().zip(response.chain_block_accepted_transactions.iter()) {
            if data.chain_block_header.hash != Some(*hash) {
                return Err("accepting header hash mismatch".into());
            }
            let daa = data.chain_block_header.daa_score.ok_or("accepting header lacks DAA")?;
            let mut transactions = Vec::new();
            for raw in &data.accepted_transactions {
                let mut inputs = Vec::new();
                for input in &raw.inputs {
                    let u = input
                        .verbose_data
                        .as_ref()
                        .and_then(|v| v.utxo_entry.as_ref())
                        .ok_or("RPC Full response lacks historical input UTXO")?;
                    inputs.push(UtxoEntry::new(
                        u.amount.ok_or("missing source value")?,
                        u.script_public_key.clone().ok_or("missing source script")?,
                        u.block_daa_score.ok_or("missing consensus acceptance DAA")?,
                        u.is_coinbase.ok_or("missing source coinbase flag")?,
                        u.covenant_id,
                    ));
                }
                transactions.push(AcceptedTx { tx: Transaction::try_from(raw.clone())?, inputs });
            }
            added.push(ChainBlock { hash: *hash, daa, transactions });
        }
        let common = if let Some(last) = response.removed_chain_block_hashes.last() {
            Some(
                request(self.rpc.get_block(*last, false))
                    .await?
                    .verbose_data
                    .ok_or("removed block lacks selected parent")?
                    .selected_parent_hash,
            )
        } else {
            None
        };
        Ok(Delta { removed: response.removed_chain_block_hashes.as_ref().clone(), added, common })
    }
    pub async fn submit(&self, tx: &Transaction) -> Result<Hash> {
        let returned = request(self.rpc.submit_transaction(RpcTransaction::from(tx), false)).await?;
        if returned != tx.id() {
            return Err("RPC returned a different submitted transaction ID".into());
        }
        Ok(returned)
    }
    pub fn address(&self, script: &kaspa_consensus_core::tx::ScriptPublicKey) -> Result<Address> {
        Ok(extract_script_pub_key_address(script, self.prefix)?)
    }
    pub async fn utxos(&self, scripts: &[kaspa_consensus_core::tx::ScriptPublicKey]) -> Result<Vec<Funding>> {
        let addresses = scripts.iter().map(|s| self.address(s)).collect::<Result<Vec<_>>>()?;
        Ok(request(self.rpc.get_utxos_by_addresses(addresses))
            .await?
            .into_iter()
            .map(|e| Funding { outpoint: e.outpoint.into(), utxo: e.utxo_entry.into() })
            .collect())
    }
    pub async fn consistent(&self, stage: &Stage) -> Result<bool> {
        let cells = stage.cells();
        if cells.is_empty() {
            return Ok(true);
        }
        let found = self.utxos(&cells.iter().map(|(_, u)| u.script_public_key.clone()).collect::<Vec<_>>()).await?;
        Ok(cells.iter().all(|(out, u)| found.iter().any(|f| f.outpoint == *out && same_utxo(u, &f.utxo))))
    }
    /// Bootstrap an old but still open invitation using this node's accepted
    /// UTXO index. The directory cannot supply acceptance metadata or principal.
    /// Capture the cursor first so a concurrent join/cancel is replayed later.
    pub async fn invitation_lobby(&self, a: &argent_runtime::Artifact, invite: &super::tracker::Invitation) -> Result<(Hash, Cell<Lobby>)> {
        invite.validate(a)?;
        if invite.network != self.network.to_string() { return Err("invitation network differs from node".into()); }
        // connect()/Cache::sync enforce Config.allow_unsynced before keys are
        // enabled; do not override its explicit disposable-devnet setting.
        let anchor = self.info().await?.sink;
        let output = &invite.genesis.outputs[0];
        let coins = self.utxos(&[output.script_public_key.clone()]).await?;
        Ok((anchor, super::invitations::accepted_lobby(a, invite, &coins)?))
    }
    pub async fn funding(&self, public: &[u8], minimum: u64, maturity: u64) -> Result<Funding> {
        self.funding_excluding(public, minimum, maturity, &[]).await
    }
    pub async fn funding_excluding(
        &self,
        public: &[u8],
        minimum: u64,
        maturity: u64,
        reserved: &[kaspa_consensus_core::tx::TransactionOutpoint],
    ) -> Result<Funding> {
        self.funding_candidates(public, minimum, maturity, reserved).await?.into_iter().next().ok_or_else(|| {
            format!(
                "no single mature fee UTXO has {minimum} sompi; fund {}",
                Address::new(self.prefix, kaspa_addresses::Version::PubKey, public)
            )
            .into()
        })
    }
    /// Return eligible coins in deterministic order; callers with a transaction
    /// shape can select by its actual storage mass instead of smallest amount.
    pub async fn funding_candidates(
        &self,
        public: &[u8],
        minimum: u64,
        maturity: u64,
        reserved: &[kaspa_consensus_core::tx::TransactionOutpoint],
    ) -> Result<Vec<Funding>> {
        let scripts = [p2pk(public)];
        let (info, mut found) = tokio::try_join!(async { self.info().await.map_err(|e| e.to_string()) }, async {
            self.utxos(&scripts).await.map_err(|e| e.to_string())
        })?;
        let daa = info.virtual_daa_score;
        found.retain(|f| {
            !reserved.contains(&f.outpoint)
                && f.utxo.amount >= minimum
                && f.utxo.covenant_id.is_none()
                && f.utxo.block_daa_score != u64::MAX
                && (!f.utxo.is_coinbase || daa >= f.utxo.block_daa_score.saturating_add(maturity))
        });
        found.sort_by_key(|f| f.utxo.amount);
        Ok(found)
    }
    pub async fn pending_exists(&self, txid: Hash, payer: &[u8]) -> Result<bool> {
        let address = self.address(&p2pk(payer))?;
        let entries = request(self.rpc.get_mempool_entries_by_addresses(vec![address], true, false)).await?;
        for entry in entries {
            for pending in entry.sending.iter().chain(entry.receiving.iter()) {
                if Transaction::try_from(pending.transaction.clone())?.id() == txid {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    pub async fn sources_unspent(&self, tx: &Transaction, entries: &[UtxoEntry]) -> Result<bool> {
        let scripts = entries.iter().map(|u| u.script_public_key.clone()).collect::<Vec<_>>();
        let found = self.utxos(&scripts).await?;
        Ok(tx
            .inputs
            .iter()
            .zip(entries)
            .all(|(i, u)| found.iter().any(|f| f.outpoint == i.previous_outpoint && same_utxo(u, &f.utxo))))
    }
    /// Covenant inputs must still match accepted metadata exactly. Ordinary
    /// fee change may instead come from one of our already-submitted parents.
    pub async fn sources_available(&self, tx: &Transaction, entries: &[UtxoEntry], parents: &[Transaction]) -> Result<bool> {
        if tx.inputs.len() != entries.len() {
            return Ok(false);
        }
        let scripts = entries.iter().map(|u| u.script_public_key.clone()).collect::<Vec<_>>();
        let found = self.utxos(&scripts).await?;
        for (input, entry) in tx.inputs.iter().zip(entries) {
            if found.iter().any(|f| {
                f.outpoint == input.previous_outpoint
                    && if entry.covenant_id.is_some() {
                        same_utxo(entry, &f.utxo)
                    } else {
                        f.utxo.amount == entry.amount
                            && f.utxo.script_public_key == entry.script_public_key
                            && f.utxo.covenant_id.is_none()
                            && f.utxo.is_coinbase == entry.is_coinbase
                    }
            }) {
                continue;
            }
            if entry.covenant_id.is_some() {
                return Ok(false);
            }
            let Some(parent) = parents.iter().find(|p| p.id() == input.previous_outpoint.transaction_id) else {
                return Ok(false);
            };
            let Some(output) = parent.outputs.get(input.previous_outpoint.index as usize) else {
                return Ok(false);
            };
            if output.covenant.is_some() || output.value != entry.amount || output.script_public_key != entry.script_public_key {
                return Ok(false);
            }
            // This is P2PK change; derive its public key from the standard script.
            let address = self.address(&entry.script_public_key)?;
            if !self.pending_exists(parent.id(), &address.payload).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
