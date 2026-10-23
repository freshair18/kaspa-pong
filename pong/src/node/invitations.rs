//! Public payload discovery. The directory locates data; the player's signature
//! and the guest's own node/VM establish identity, accepted funds and spendability.
use super::{
    Config,
    rpc::Node,
    tracker::{ChainBlock, Invitation},
};
use crate::engine::*;
use argent_runtime::{Artifact, TxBuilder, TxContext};
use kaspa_consensus_core::{
    Hash,
    hashing::covenant_id::covenant_id,
    tx::{Transaction, TransactionOutpoint, UtxoEntry},
};
use secp256k1::{Keypair, Message, Secp256k1, XOnlyPublicKey, schnorr::Signature};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    path::Path,
    time::Duration,
};

const MAGIC: &[u8] = b"PONGI";
const DOMAIN: &[u8] = b"KaspaPong/creation-intent/19\0";
const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const MAX_RESPONSE: u64 = 8 * 1024 * 1024;
pub const UNDO_LIMIT: usize = 2048;

fn digest(bytes: &[u8]) -> [u8; 32] {
    let h = blake2b_simd::Params::new().hash_length(32).to_state().update(DOMAIN).update(bytes).finalize();
    h.as_bytes().try_into().unwrap()
}

/// Prefixes have 60 identity bits plus 20 checksum bits. A collision requires
/// selecting the full identity (52 base32 digits plus the same checksum).
pub fn code(id: Hash) -> String {
    encode_code(id, 12)
}
pub fn full_code(id: Hash) -> String {
    encode_code(id, 52)
}
fn identity_digits(id: Hash) -> String {
    (0..52)
        .map(|i| {
            let n = (0..5).fold(0u8, |n, j| {
                let bit = i * 5 + j;
                (n << 1) | if bit < 256 { (id.as_bytes()[bit / 8] >> (7 - bit % 8)) & 1 } else { 0 }
            });
            ALPHABET[n as usize] as char
        })
        .collect()
}
fn checksum(data: &str) -> String {
    let hash = digest(data.as_bytes());
    (0..4).map(|i| ALPHABET[((hash[i] as u16 * 32) / 256) as usize] as char).collect()
}
fn encode_code(id: Hash, digits: usize) -> String {
    let text = identity_digits(id)[..digits].to_string();
    let combined = format!("{text}{}", checksum(&text));
    combined.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("-")
}
pub fn parse_code(text: &str) -> Result<String> {
    let text: String = text.chars().filter(|c| *c != '-' && !c.is_ascii_whitespace()).map(|c| c.to_ascii_uppercase()).collect();
    if ![16, 56].contains(&text.len()) || !text.bytes().all(|c| ALPHABET.contains(&c)) {
        return Err("expected a 16-character game code (or the full collision-resolution code)".into());
    }
    let (id, check) = text.split_at(text.len() - 4);
    if checksum(id) != check {
        return Err("game code checksum failed; check the copied code".into());
    }
    Ok(id.to_string())
}
pub fn resolve_code(text: &str, ids: impl IntoIterator<Item = Hash>) -> Result<Hash> {
    let prefix = parse_code(text)?;
    let found: std::collections::BTreeSet<_> = ids.into_iter().filter(|id| identity_digits(*id).starts_with(&prefix)).collect();
    if found.len() > 1 {
        return Err("ambiguous short game code; ask the host for the full game code".into());
    }
    found
        .into_iter()
        .next()
        .ok_or_else(|| "invitation not found, not yet indexed, already joined/cancelled, or outside directory history coverage".into())
}

/// `coins` MUST come from the joining node, never the invitation directory.
pub fn accepted_lobby(a: &Artifact, invite: &Invitation, coins: &[Funding]) -> Result<Cell<Lobby>> {
    invite.validate(a)?;
    let output = &invite.genesis.outputs[0];
    let outpoint = TransactionOutpoint::new(invite.genesis.id(), 0);
    let actual = coins
        .iter()
        .find(|f| f.outpoint == outpoint)
        .ok_or("invitation is not accepted, already joined/cancelled, or was reorganized out")?;
    if actual.utxo.amount != output.value
        || actual.utxo.script_public_key != output.script_public_key
        || actual.utxo.covenant_id != output.covenant.map(|c| c.covenant_id)
        || actual.utxo.is_coinbase
        || actual.utxo.block_daa_score == u64::MAX
    {
        return Err("accepted lobby differs from authenticated creation".into());
    }
    Ok(Cell { outpoint, utxo: actual.utxo.clone(), state: invite.lobby.clone() })
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err("truncated invitation manifest".into());
        }
        let (value, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(value)
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
    fn hash(&mut self) -> Result<Hash> {
        Ok(Hash::from_bytes(self.take(32)?.try_into()?))
    }
}

/// One fixed binary format, with exact consumption and bounded network text.
/// Artifact identity is deliberately absent: the fixed version binds the rules;
/// the client additionally requires its sole pinned artifact at reconstruction.
pub fn create(
    a: &Artifact,
    network: &str,
    anchor: Hash,
    lobby: &Lobby,
    funding: &Funding,
    host: &Keypair,
    payer: &Keypair,
    fee: u64,
) -> Result<Invitation> {
    if pubkey(host) != lobby.left || network.len() > 32 || network.is_empty() || funding.utxo.covenant_id.is_some() {
        return Err("invalid invitation creator, network or ordinary funding source".into());
    }
    network.parse::<kaspa_consensus_core::network::NetworkId>()?;
    let mut payload = MAGIC.to_vec();
    payload.extend(GAME_VERSION.to_le_bytes());
    payload.push(network.len() as u8);
    payload.extend(network.as_bytes());
    payload.extend(anchor.as_bytes());
    for key in [&lobby.left, &lobby.right, &lobby.refund, &pubkey(payer)] {
        payload.extend(key);
    }
    for amount in [
        lobby.control_value as u64,
        lobby.ball_tick_daa as u64,
        lobby.stake_per_player as u64,
        COURT_DEPOSIT,
        funding.utxo.amount,
        fee,
    ] {
        payload.extend(amount.to_le_bytes());
    }
    payload.extend(funding.outpoint.transaction_id.as_bytes());
    payload.extend(funding.outpoint.index.to_le_bytes());
    let sig = host.sign_schnorr(Message::from_digest(digest(&payload)));
    payload.extend(sig.as_ref());
    let deposit = GAME_DEPOSIT.checked_add(stake_amount(lobby.stake_per_player)?).ok_or("deposit overflow")?;
    let ctx = TxContext::new().payload(payload).actor_genesis_output(0, "launch::pong", "Lobby", lobby.state(), deposit);
    let genesis = build(&TxBuilder::new(a)?, &funded(ctx, funding, payer, deposit.checked_add(fee).ok_or("fee overflow")?)?)?;
    let invite = Invitation {
        version: GAME_VERSION,
        network: network.into(),
        artifact_id: a.id.to_string(),
        anchor,
        genesis,
        lobby: lobby.clone(),
        court_value: COURT_DEPOSIT,
    };
    invite.validate(a)?;
    Ok(invite)
}

/// Decode without trusting any directory-supplied redundant state or identity.
pub fn from_transaction(a: &Artifact, tx: &Transaction) -> Result<Invitation> {
    let invite = decode_transaction(a, tx)?;
    invite.validate(a)?;
    Ok(invite)
}

fn decode_transaction(a: &Artifact, tx: &Transaction) -> Result<Invitation> {
    let mut finalized = tx.clone();
    finalized.finalize();
    if finalized.id() != tx.id() {
        return Err("directory transaction has a forged cached identity".into());
    }
    let mut r = Reader(&tx.payload);
    if r.take(MAGIC.len())? != MAGIC {
        return Err("not a Pong invitation".into());
    }
    let version = u32::from_le_bytes(r.take(4)?.try_into()?);
    let length = r.take(1)?[0] as usize;
    if length == 0 || length > 32 {
        return Err("invalid manifest network length".into());
    }
    let network = std::str::from_utf8(r.take(length)?)?.to_string();
    let anchor = r.hash()?;
    let left = r.take(32)?.to_vec();
    let right = r.take(32)?.to_vec();
    let refund = r.take(32)?.to_vec();
    let payer = r.take(32)?.to_vec();
    let lobby = Lobby {
        left,
        right,
        refund,
        control_value: i64::try_from(r.u64()?)?,
        ball_tick_daa: i64::try_from(r.u64()?)?,
        stake_per_player: i64::try_from(r.u64()?)?,
    };
    let court_value = r.u64()?;
    let amount = r.u64()?;
    let fee = r.u64()?;
    let outpoint = TransactionOutpoint::new(r.hash()?, u32::from_le_bytes(r.take(4)?.try_into()?));
    let signature = Signature::from_slice(r.take(64)?)?;
    if !r.0.is_empty() {
        return Err("trailing invitation manifest bytes".into());
    }
    Secp256k1::verification_only().verify_schnorr(
        &signature,
        &Message::from_digest(digest(&tx.payload[..tx.payload.len() - 64])),
        &XOnlyPublicKey::from_slice(&lobby.left)?,
    )?;
    network.parse::<kaspa_consensus_core::network::NetworkId>()?;
    XOnlyPublicKey::from_slice(&payer)?;
    let invite = Invitation { version, network, artifact_id: a.id.to_string(), anchor, genesis: tx.clone(), lobby, court_value };
    // Validate creation shape and its ordinary funding signature independently.
    if tx.inputs.len() != 1 || tx.inputs[0].previous_outpoint != outpoint || tx.outputs.is_empty() || tx.outputs.len() > 2 {
        return Err("creation must use exactly the signed ordinary funding source".into());
    }
    let deposit = GAME_DEPOSIT.checked_add(stake_amount(invite.lobby.stake_per_player)?).ok_or("deposit overflow")?;
    let change = amount.checked_sub(deposit).and_then(|v| v.checked_sub(fee)).ok_or("creation funding/fee underflow")?;
    if fee == 0
        || tx.outputs.len() != if change > 0 { 2 } else { 1 }
        || tx.outputs.get(1).is_some_and(|o| o.value != change || o.script_public_key != p2pk(&payer) || o.covenant.is_some())
    {
        return Err("creation alters signed principal, fee or funding change".into());
    }
    let output = &tx.outputs[0];
    if output.covenant != Some(kaspa_consensus_core::tx::CovenantBinding::new(0, covenant_id(outpoint, [(0, output)].into_iter()))) {
        return Err("creation covenant identity differs from signed source/output".into());
    }
    argent_runtime::execute_input_with_covenants(tx, vec![UtxoEntry::new(amount, p2pk(&payer), 0, false, None)], 0)?;
    Ok(invite)
}

pub fn authenticate(a: &Artifact, invite: &Invitation) -> Result<()> {
    let decoded = decode_transaction(a, &invite.genesis)?;
    if decoded.version != invite.version
        || decoded.network != invite.network
        || decoded.anchor != invite.anchor
        || decoded.lobby.state() != invite.lobby.state()
        || decoded.court_value != invite.court_value
    {
        return Err("invitation metadata differs from authenticated creation".into());
    }
    Ok(())
}

/// Called only after full invitation validation. Preserve the accepted creation
/// fee when a cold guest bootstraps after that transaction's history was pruned.
pub fn creation_payment(invite: &Invitation) -> Result<super::fees::Payment> {
    let mut r = Reader(&invite.genesis.payload);
    r.take(MAGIC.len() + 4)?;
    let length = r.take(1)?[0] as usize;
    r.take(length + 32 + 96)?;
    let payer = r.take(32)?;
    r.take(5 * 8)?;
    Ok(super::fees::Payment { sompi: r.u64()?, payer: p2pk(payer) })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Indexed {
    pub transaction: Transaction,
    pub open: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexUndo {
    hash: Hash,
    parent: Hash,
    previous: Vec<(Hash, Option<Indexed>)>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Index {
    pub version: u32,
    pub network: String,
    pub coverage_anchor: Hash,
    pub cursor: Hash,
    pub records: BTreeMap<Hash, Indexed>,
    history: Vec<IndexUndo>,
}
impl Index {
    pub fn new(network: &str, anchor: Hash) -> Self {
        Self {
            version: GAME_VERSION,
            network: network.into(),
            coverage_anchor: anchor,
            cursor: anchor,
            records: BTreeMap::new(),
            history: vec![],
        }
    }
    /// Apply on a clone, so an incoherent rollback cannot publish partial data.
    pub fn apply(&mut self, a: &Artifact, removed: &[Hash], added: &[ChainBlock]) -> Result<()> {
        let mut next = self.clone();
        for hash in removed {
            let undo = next.history.pop().ok_or("directory rollback crosses coverage anchor; explicit reindex required")?;
            if undo.hash != *hash || next.cursor != *hash {
                return Err("incoherent directory reorg".into());
            }
            for (id, old) in undo.previous.into_iter().rev() {
                match old {
                    Some(record) => {
                        next.records.insert(id, record);
                    }
                    None => {
                        next.records.remove(&id);
                    }
                }
            }
            next.cursor = undo.parent;
        }
        for block in added {
            let mut undo = IndexUndo { hash: block.hash, parent: next.cursor, previous: vec![] };
            for accepted in &block.transactions {
                for input in &accepted.tx.inputs {
                    let id = input.previous_outpoint.transaction_id;
                    if input.previous_outpoint.index == 0 {
                        if let Some(record) = next.records.get_mut(&id).filter(|r| r.open) {
                            undo.previous.push((id, Some(record.clone())));
                            record.open = false;
                        }
                    }
                }
                if !accepted.tx.payload.starts_with(MAGIC) {
                    continue;
                }
                let Ok(invite) = from_transaction(a, &accepted.tx) else { continue };
                if invite.network != next.network {
                    continue;
                }
                let id = accepted.tx.id();
                undo.previous.push((id, next.records.insert(id, Indexed { transaction: accepted.tx.clone(), open: true })));
            }
            next.cursor = block.hash;
            next.history.push(undo);
            if next.history.len() > UNDO_LIMIT {
                next.history.remove(0);
            }
        }
        *self = next;
        Ok(())
    }
    pub fn lookup(&self, text: &str) -> Result<Vec<Transaction>> {
        let prefix = parse_code(text)?;
        Ok(self
            .records
            .iter()
            .filter(|(id, r)| r.open && identity_digits(**id).starts_with(&prefix))
            .map(|(_, r)| r.transaction.clone())
            .collect())
    }
    pub fn addressed(&self, a: &Artifact, player: &[u8]) -> Vec<Transaction> {
        self.records
            .values()
            .filter(|r| r.open)
            .filter_map(|r| from_transaction(a, &r.transaction).ok())
            .filter(|i| i.lobby.right == player)
            .map(|i| i.genesis)
            .collect()
    }
}

/// Reject ambiguity before selecting any candidate. An index can omit results,
/// so the guest also displays full host/identity information before signing.
pub fn select(a: &Artifact, network: &str, player: &[u8], text: &str, transactions: Vec<Transaction>) -> Result<Invitation> {
    let prefix = parse_code(text)?;
    let mut found = BTreeMap::new();
    for tx in transactions {
        if !identity_digits(tx.id()).starts_with(&prefix) {
            return Err("directory returned a different game identity".into());
        }
        let invite = from_transaction(a, &tx)?;
        if invite.network != network || invite.lobby.right != player {
            return Err("invitation network or invited player differs".into());
        }
        found.insert(tx.id(), invite);
    }
    let id = resolve_code(text, found.keys().copied())?;
    Ok(found.remove(&id).expect("resolved candidate exists"))
}

fn endpoint(url: &str) -> Result<&str> {
    let authority = url
        .strip_prefix("http://")
        .ok_or("invitation_index must be explicit http://HOST:PORT (use a trusted tunnel for remote transport)")?;
    if authority.is_empty() || authority.contains(['/', '?', '#', '@', '\r', '\n']) || !authority.contains(':') {
        return Err("invitation_index must contain only an explicit HTTP host and port".into());
    }
    Ok(authority)
}
pub fn configured(c: &Config) -> Result<&str> {
    let url = c.invitation_index.as_deref().ok_or("configure invitation_index as http://HOST:PORT and run pong-node index before creating invitations; see docs/ONCHAIN_INVITATIONS.md")?;
    endpoint(url)?;
    Ok(url)
}
fn fetch(url: &str, route: &str) -> Result<Vec<Transaction>> {
    let authority = endpoint(url)?;
    let addresses = authority.to_socket_addrs()?;
    let mut stream = None;
    for address in addresses {
        if let Ok(s) = TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
            stream = Some(s);
            break;
        }
    }
    let mut stream = stream.ok_or("invitation directory connection failed")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(stream, "GET {route} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n\r\n")?;
    let mut bytes = Vec::new();
    stream.take(MAX_RESPONSE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RESPONSE {
        return Err("directory response exceeds limit".into());
    }
    let split = bytes.windows(4).position(|s| s == b"\r\n\r\n").ok_or("invalid directory HTTP response")?;
    if !bytes.starts_with(b"HTTP/1.0 200 ") {
        return Err("directory unavailable or lookup rejected".into());
    }
    Ok(serde_json::from_slice(&bytes[split + 4..])?)
}
pub async fn discover(a: &Artifact, c: &Config, player: &[u8], text: &str) -> Result<Invitation> {
    discover_pending(a, c, player, text).await?.ok_or_else(|| "invitation not indexed yet; retry shortly".into())
}
/// An empty directory response is normal while acceptance/indexing catches up.
/// Invalid or ambiguous nonempty responses remain errors, never pending.
pub async fn discover_pending(a: &Artifact, c: &Config, player: &[u8], text: &str) -> Result<Option<Invitation>> {
    let prefix = parse_code(text)?;
    let url = configured(c)?.to_string();
    let code = format!("{prefix}{}", checksum(&prefix));
    let transactions = tokio::task::spawn_blocking(move || fetch(&url, &format!("/lookup/{code}")).map_err(|e| e.to_string()))
        .await?
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    if transactions.is_empty() { return Ok(None); }
    select(a, &c.network, player, text, transactions).map(Some)
}
pub async fn addressed(a: &Artifact, c: &Config, player: &[u8]) -> Result<Vec<Invitation>> {
    let url = configured(c)?.to_string();
    let public = XOnlyPublicKey::from_slice(player)?.to_string();
    let txs = tokio::task::spawn_blocking(move || fetch(&url, &format!("/player/{public}")).map_err(|e| e.to_string()))
        .await?
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    txs.into_iter()
        .map(|tx| {
            let i = from_transaction(a, &tx)?;
            if i.network != c.network || i.lobby.right != player {
                return Err("directory returned wrong participant/network".into());
            }
            Ok(i)
        })
        .collect()
}

/// Bounded read-only HTTP server integrated into the index command. No wallet,
/// registration or write endpoint; every record originates in accepted payloads.
/// Handle bounded requests between accepted-chain batches. A blocking listener
/// is also usable by focused transport tests.
pub fn serve_requests(a: &Artifact, index: &Index, listener: &TcpListener, limit: usize) -> Result<usize> {
    let mut served = 0;
    for _ in 0..limit {
        let (mut stream, _) = match listener.accept() {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e.into()),
        };
        stream.set_read_timeout(Some(Duration::from_millis(200)))?;
        stream.set_write_timeout(Some(Duration::from_millis(500)))?;
        let mut request = [0; 1024];
        let mut n = 0;
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while n < request.len() && !request[..n].windows(4).any(|s| s == b"\r\n\r\n") && std::time::Instant::now() < deadline {
            match stream.read(&mut request[n..]) {
                Ok(0) | Err(_) => break,
                Ok(count) => n += count,
            }
        }
        if !request[..n].windows(4).any(|s| s == b"\r\n\r\n") {
            continue;
        }
        let line = std::str::from_utf8(&request[..n]).unwrap_or("").lines().next().unwrap_or("");
        let route = line.strip_prefix("GET ").and_then(|s| s.split_once(' ')).map(|s| s.0).unwrap_or("");
        let records = if let Some(text) = route.strip_prefix("/lookup/") {
            index.lookup(text)
        } else if let Some(player) = route.strip_prefix("/player/") {
            super::public(player).map(|p| index.addressed(a, &p))
        } else {
            Err("unknown directory route".into())
        };
        let (status, body) = match records {
            Ok(records) => ("200 OK", serde_json::to_vec(&records)?),
            Err(_) => ("400 Bad Request", b"[]".to_vec()),
        };
        if body.len() as u64 > MAX_RESPONSE {
            let _ = stream.write_all(b"HTTP/1.0 413 Too Large\r\n\r\n");
            continue;
        }
        let _ = write!(
            stream,
            "HTTP/1.0 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(&body);
        served += 1;
    }
    Ok(served)
}

pub async fn serve(a: &Artifact, c: &Config, path: &Path, listen: &str, anchor: Option<Hash>) -> Result<()> {
    let _lock = super::lock(path)?;
    let node = Node::connect(c).await?;
    let result = async {
        let mut index: Index = if path.exists() {
            super::load(path)?
        } else {
            Index::new(&c.network, anchor.unwrap_or(node.info().await?.pruning_point_hash))
        };
        if index.version != GAME_VERSION || index.network != c.network || anchor.is_some_and(|h| h != index.coverage_anchor) {
            return Err("directory belongs to another version/network/coverage anchor; choose a fresh path".into());
        }
        let listener = TcpListener::bind(listen)?;
        listener.set_nonblocking(true)?;
        eprintln!("Invitation directory listening on {listen}; coverage begins after {}; no wallet is used.", index.coverage_anchor);
        loop {
            let delta = node.delta(index.cursor).await?;
            if !delta.removed.is_empty() || !delta.added.is_empty() {
                index.apply(a, &delta.removed, &delta.added)?;
                super::save(path, &index)?;
            }
            // Serve between every bounded chain batch, including continuous
            // 10-BPS progress. Classification is advisory; the joining client
            // always checks its own current UTXO set before signing.
            serve_requests(a, &index, &listener, 16)?;
            tokio::time::sleep(Duration::from_millis(c.poll_ms)).await;
        }
    }
    .await;
    let _ = node.rpc.disconnect().await;
    result
}
