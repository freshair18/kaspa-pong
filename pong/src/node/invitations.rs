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
    sync::{Arc, RwLock, atomic::{AtomicBool, Ordering}},
    time::Duration,
};

const MAGIC: &[u8] = b"PONGI";
const DOMAIN: &[u8] = b"KaspaPong/creation-intent/24\0";
const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const BASE58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const MAX_RESPONSE: u64 = 8 * 1024 * 1024;
pub const RENDEZVOUS_BYTES: usize = 8;
/// Six tag bytes: speed/balls/goals/stake terms (16 bits) and 32 random bits.
const RENDEZVOUS_CODE_TAG_BYTES: usize = 6;
pub const UNDO_LIMIT: usize = 2048;

/// Host-selected terms carried by the code. Auto goals are resolved by the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GameTerms {
    pub speed: GameSpeed,
    pub ball_count: u8,
    pub win_score: u8,
    pub stake_kas: u8,
}
impl GameTerms {
    pub fn validate(self) -> Result<()> {
        if !(1..=3).contains(&self.ball_count) || !(1..=9).contains(&self.win_score) {
            return Err("game code requires 1–3 balls and a winning score of 1–9".into());
        }
        if self.stake_kas != 5 {
            return Err("this client requires a 5 KAS stake per player".into());
        }
        Ok(())
    }
    fn tag(self) -> Result<[u8; 2]> {
        self.validate()?;
        Ok([(self.speed.code() << 6) | ((self.ball_count - 1) << 4) | self.win_score, self.stake_kas])
    }
    pub fn from_tag(tag: &[u8; RENDEZVOUS_BYTES]) -> Result<Self> {
        if tag[RENDEZVOUS_CODE_TAG_BYTES..].iter().any(|byte| *byte != 0) {
            return Err("unsupported reserved game-code bytes".into());
        }
        let terms = Self { speed: GameSpeed::from_code(tag[0] >> 6)?, ball_count: ((tag[0] >> 4) & 3) + 1,
            win_score: tag[0] & 15, stake_kas: tag[1] };
        terms.validate()?;
        Ok(terms)
    }
    pub fn matches(self, lobby: &Lobby) -> bool {
        lobby.ball_tick_daa == self.speed.tick_daa() && lobby.ball_count == i64::from(self.ball_count)
            && lobby.win_score == i64::from(self.win_score) && lobby.stake_per_player == i64::from(self.stake_kas) * 100_000_000
    }
    pub fn from_lobby(lobby: &Lobby) -> Result<Self> {
        let terms = Self {
            speed: GameSpeed::from_tick_daa(lobby.ball_tick_daa).ok_or("unsupported game speed")?,
            ball_count: u8::try_from(lobby.ball_count)?, win_score: u8::try_from(lobby.win_score)?,
            stake_kas: u8::try_from(lobby.stake_per_player / 100_000_000)?,
        };
        terms.validate()?;
        if !terms.matches(lobby) { return Err("game terms require a whole-KAS stake".into()); }
        Ok(terms)
    }
    pub fn summary(self) -> String {
        format!("{} · {} ball{} · First to {} · {} KAS each", self.speed.label(), self.ball_count,
            if self.ball_count == 1 { "" } else { "s" }, self.win_score, self.stake_kas)
    }
}

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
    identity_digits_bytes(&id.as_bytes())
}
fn identity_digits_bytes(bytes: &[u8]) -> String {
    (0..52)
        .map(|i| {
            let n = (0..5).fold(0u8, |n, j| {
                let bit = i * 5 + j;
                (n << 1) | if bit < bytes.len() * 8 { (bytes[bit / 8] >> (7 - bit % 8)) & 1 } else { 0 }
            });
            ALPHABET[n as usize] as char
        })
        .collect()
}
/// A self-contained code for a fresh inbound player's public key. Unlike an
/// invitation-ID prefix it needs no directory or retained transaction history.
pub fn player_code(public: &[u8]) -> Result<String> {
    XOnlyPublicKey::from_slice(public)?;
    let text = identity_digits_bytes(public);
    let combined = format!("{text}{}", checksum(&text));
    Ok(combined.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("-"))
}
pub fn parse_player_code(text: &str) -> Result<Vec<u8>> {
    let text: String = text.chars().filter(|c| *c != '-' && !c.is_ascii_whitespace()).map(|c| c.to_ascii_uppercase()).collect();
    if text.len() != 56 || !text.bytes().all(|c| ALPHABET.contains(&c)) {
        return Err("expected the 56-character host game public-key code".into());
    }
    let (identity, check) = text.split_at(52);
    if checksum(identity) != check { return Err("game code checksum failed; check the copied code".into()); }
    let mut public = [0u8; 32];
    for (i, byte) in identity.bytes().enumerate() {
        let value = ALPHABET.iter().position(|c| *c == byte).ok_or("invalid game code")? as u8;
        for bit in 0..5 {
            let position = i * 5 + bit;
            if position < 256 { public[position / 8] |= ((value >> (4 - bit)) & 1) << (7 - position % 8); }
        }
    }
    XOnlyPublicKey::from_slice(&public)?;
    Ok(public.to_vec())
}
/// A compact self-contained host key plus public rendezvous tag. Its one
/// checksum detects copy errors; the tag distinguishes concurrent invitations.
pub fn rendezvous_code(public: &[u8], tag: &[u8; RENDEZVOUS_BYTES]) -> Result<String> {
    XOnlyPublicKey::from_slice(public)?;
    GameTerms::from_tag(tag)?;
    if tag[RENDEZVOUS_CODE_TAG_BYTES..].iter().any(|byte| *byte != 0) {
        return Err("cannot encode a rendezvous tag with nonzero omitted bytes".into());
    }
    let mut payload = Vec::with_capacity(32 + RENDEZVOUS_CODE_TAG_BYTES + 4);
    payload.extend(public);
    payload.extend(&tag[..RENDEZVOUS_CODE_TAG_BYTES]);
    payload.extend(&digest(&payload)[..4]);
    Ok(base58_encode(&payload))
}
pub fn parse_rendezvous_code(text: &str) -> Result<(Vec<u8>, [u8; RENDEZVOUS_BYTES], GameTerms)> {
    let compact: String = text.chars().filter(|c| *c != '-' && !c.is_ascii_whitespace()).collect();
    let payload = base58_decode(&compact)?;
    if payload.len() != 32 + RENDEZVOUS_CODE_TAG_BYTES + 4 {
        return Err("expected a compact host game code".into());
    }
    let (signed, check) = payload.split_at(payload.len() - 4);
    if digest(signed)[..4] != *check { return Err("game code checksum failed; check the copied code".into()); }
    let public = signed[..32].to_vec();
    XOnlyPublicKey::from_slice(&public)?;
    let mut tag = [0; RENDEZVOUS_BYTES];
    tag[..RENDEZVOUS_CODE_TAG_BYTES].copy_from_slice(&signed[32..]);
    Ok((public, tag, GameTerms::from_tag(&tag)?))
}
pub fn new_rendezvous_tag(terms: GameTerms) -> Result<[u8; RENDEZVOUS_BYTES]> {
    let mut tag = [0; RENDEZVOUS_BYTES];
    tag[..2].copy_from_slice(&terms.tag()?);
    getrandom::getrandom(&mut tag[2..RENDEZVOUS_CODE_TAG_BYTES]).map_err(|_| "operating system randomness is unavailable")?;
    Ok(tag)
}
fn checksum(data: &str) -> String {
    let hash = digest(data.as_bytes());
    (0..4).map(|i| ALPHABET[((hash[i] as u16 * 32) / 256) as usize] as char).collect()
}
fn base58_encode(bytes: &[u8]) -> String {
    let zeros = bytes.iter().take_while(|byte| **byte == 0).count();
    let mut source = bytes.to_vec();
    let mut start = zeros;
    let mut encoded = Vec::new();
    while start < source.len() {
        let mut remainder = 0u32;
        for byte in &mut source[start..] {
            let value = remainder * 256 + u32::from(*byte);
            *byte = (value / 58) as u8;
            remainder = value % 58;
        }
        encoded.push(BASE58[remainder as usize]);
        while start < source.len() && source[start] == 0 { start += 1; }
    }
    encoded.extend(std::iter::repeat_n(b'1', zeros));
    encoded.reverse();
    String::from_utf8(encoded).unwrap()
}
fn base58_decode(text: &str) -> Result<Vec<u8>> {
    if text.is_empty() { return Err("expected a compact host game code".into()); }
    let zeros = text.bytes().take_while(|byte| *byte == b'1').count();
    let mut decoded = Vec::<u8>::new();
    for character in text.bytes() {
        let mut carry = BASE58.iter().position(|byte| *byte == character).ok_or("game code has an invalid character")? as u32;
        for byte in decoded.iter_mut().rev() {
            let value = u32::from(*byte) * 58 + carry;
            *byte = value as u8;
            carry = value >> 8;
        }
        while carry > 0 {
            decoded.insert(0, carry as u8);
            carry >>= 8;
        }
    }
    let mut result = vec![0; zeros];
    result.extend(decoded);
    Ok(result)
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
    let mut tag = [0; RENDEZVOUS_BYTES];
    tag[..2].copy_from_slice(&GameTerms::from_lobby(lobby)?.tag()?);
    create_with_rendezvous(a, network, anchor, lobby, funding, host, payer, fee, tag)
}
pub fn create_with_rendezvous(
    a: &Artifact,
    network: &str,
    anchor: Hash,
    lobby: &Lobby,
    funding: &Funding,
    host: &Keypair,
    payer: &Keypair,
    fee: u64,
    rendezvous: [u8; RENDEZVOUS_BYTES],
) -> Result<Invitation> {
    if pubkey(host) != lobby.left || network.len() > 32 || network.is_empty() || funding.utxo.covenant_id.is_some() {
        return Err("invalid invitation creator, network or ordinary funding source".into());
    }
    network.parse::<kaspa_consensus_core::network::NetworkId>()?;
    if !GameTerms::from_tag(&rendezvous)?.matches(lobby) {
        return Err("game code terms differ from the signed Lobby".into());
    }
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
        lobby.ball_count as u64,
        lobby.win_score as u64,
        funding.utxo.amount,
        fee,
    ] {
        payload.extend(amount.to_le_bytes());
    }
    payload.extend(funding.outpoint.transaction_id.as_bytes());
    payload.extend(funding.outpoint.index.to_le_bytes());
    payload.extend(rendezvous);
    let sig = host.sign_schnorr(Message::from_digest(digest(&payload)));
    payload.extend(sig.as_ref());
    let deposit = stake_amount(lobby.stake_per_player)?;
    let ctx = TxContext::new().payload(payload).actor_genesis_output(0, "launch::pong", "Lobby", lobby.state(), deposit);
    let genesis = build(&TxBuilder::new(a)?, &funded(ctx, funding, payer, deposit.checked_add(fee).ok_or("fee overflow")?)?)?;
    let invite = Invitation {
        version: GAME_VERSION,
        network: network.into(),
        artifact_id: a.id.to_string(),
        anchor,
        genesis,
        lobby: lobby.clone(),
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
        ball_count: i64::try_from(r.u64()?)?,
        win_score: i64::try_from(r.u64()?)?,
    };
    let amount = r.u64()?;
    let fee = r.u64()?;
    let outpoint = TransactionOutpoint::new(r.hash()?, u32::from_le_bytes(r.take(4)?.try_into()?));
    let tag: [u8; RENDEZVOUS_BYTES] = r.take(RENDEZVOUS_BYTES)?.try_into()?;
    if !GameTerms::from_tag(&tag)?.matches(&lobby) {
        return Err("signed Lobby differs from its game code terms".into());
    }
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
    let invite = Invitation { version, network, artifact_id: a.id.to_string(), anchor, genesis: tx.clone(), lobby };
    // Validate creation shape and its ordinary funding signature independently.
    if tx.inputs.len() != 1 || tx.inputs[0].previous_outpoint != outpoint || tx.outputs.is_empty() || tx.outputs.len() > 2 {
        return Err("creation must use exactly the signed ordinary funding source".into());
    }
    let deposit = stake_amount(invite.lobby.stake_per_player)?;
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

/// Returns the signed public rendezvous tag after `from_transaction` has
/// authenticated the manifest.
pub fn rendezvous_tag(tx: &Transaction) -> Result<[u8; RENDEZVOUS_BYTES]> {
    let mut r = Reader(&tx.payload);
    if r.take(MAGIC.len())? != MAGIC { return Err("not a Pong invitation".into()); }
    r.take(4)?;
    let network_len = r.take(1)?[0] as usize;
    r.take(network_len + 32 + 4 * 32 + 7 * 8 + 32 + 4)?;
    Ok(r.take(RENDEZVOUS_BYTES)?.try_into()?)
}

pub fn authenticate(a: &Artifact, invite: &Invitation) -> Result<()> {
    let decoded = decode_transaction(a, &invite.genesis)?;
    if decoded.version != invite.version
        || decoded.network != invite.network
        || decoded.anchor != invite.anchor
        || decoded.lobby.state() != invite.lobby.state()
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
    r.take(6 * 8)?;
    Ok(super::fees::Payment { sompi: r.u64()?, payer: p2pk(payer) })
}

#[cfg(test)]
mod code_tests {
    use super::*;
    use secp256k1::{Keypair, Secp256k1, SecretKey};

    #[test]
    fn player_code_round_trips_the_complete_xonly_key() {
        let secret = SecretKey::from_slice(&[7; 32]).unwrap();
        let key = Keypair::from_secret_key(&Secp256k1::new(), &secret);
        let public = key.x_only_public_key().0.serialize().to_vec();
        let code = player_code(&public).unwrap();
        assert_eq!(code.len(), 69); // 56 characters grouped in fourteen blocks.
        assert_eq!(parse_player_code(&code).unwrap(), public);
        let mut corrupted = code.clone();
        let replacement = if &corrupted[0..1] == "0" { "1" } else { "0" };
        corrupted.replace_range(0..1, replacement);
        assert!(parse_player_code(&corrupted).is_err());
    }

    #[test]
    fn rendezvous_code_round_trips_the_host_key_and_tag() {
        let public = [9; 32];
        for speed in [GameSpeed::SuperFast, GameSpeed::Fast, GameSpeed::Medium, GameSpeed::Slow] {
            for ball_count in 1..=3 {
                for win_score in 1..=9 {
                    let terms = GameTerms { speed, ball_count, win_score, stake_kas: 5 };
                    let mut tag = [0; RENDEZVOUS_BYTES];
                    tag[..RENDEZVOUS_CODE_TAG_BYTES].fill(4);
                    tag[..2].copy_from_slice(&terms.tag().unwrap());
                    let code = rendezvous_code(&public, &tag).unwrap();
                    assert!((56..=58).contains(&code.len()), "{}", code.len());
                    assert_eq!(parse_rendezvous_code(&code).unwrap(), (public.to_vec(), tag, terms));
                    let mut corrupted = code.clone();
                    let last = corrupted.pop().unwrap();
                    corrupted.push(if last == '1' { '2' } else { '1' });
                    assert!(parse_rendezvous_code(&corrupted).is_err());
                }
            }
        }
        let legacy_tag = [4; RENDEZVOUS_BYTES];
        let mut legacy_payload = public.to_vec();
        legacy_payload.extend(legacy_tag);
        legacy_payload.extend(&digest(&legacy_payload)[..4]);
        assert!(parse_rendezvous_code(&base58_encode(&legacy_payload)).is_err());
        assert!(rendezvous_code(&public, &legacy_tag).is_err());
    }

    #[test]
    fn authenticated_codes_reject_unsupported_stakes_and_invalid_game_terms() {
        let valid = GameTerms { speed: GameSpeed::SuperFast, ball_count: 3, win_score: 6, stake_kas: 5 };
        let tag = new_rendezvous_tag(valid).unwrap();
        assert_eq!(GameTerms::from_tag(&tag).unwrap(), valid);
        assert_eq!(&tag[6..], &[0, 0]);
        for (first, stake) in [(tag[0], 0), (tag[0], 4), (tag[0], 6),
            ((tag[0] & 0xf0), 5), ((tag[0] & 0xf0) | 10, 5), ((tag[0] & 0xcf) | 0x30, 5)] {
            let mut payload = vec![9; 32];
            let mut bad = tag;
            bad[0] = first;
            bad[1] = stake;
            payload.extend(&bad[..6]);
            payload.extend(&digest(&payload)[..4]);
            assert!(parse_rendezvous_code(&base58_encode(&payload)).is_err());
        }
    }
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
    fn apply_tracked(&mut self, a: &Artifact, removed: &[Hash], added: &[ChainBlock], common: Option<Hash>) -> Result<()> {
        // A tip chosen for a fresh index can already have left the selected
        // chain by the first RPC. No indexed observations exist to undo yet.
        // Rebase only that empty initial boundary, never an established index.
        if self.cursor == self.coverage_anchor && self.history.is_empty() && self.records.is_empty()
            && removed.first() == Some(&self.cursor) {
            let common = common.ok_or("initial directory rollback lacks a common ancestor")?;
            let mut rebased = Self::new(&self.network, common);
            rebased.apply(a, &[], added)?;
            eprintln!("Directory initial anchor left selected chain; rebased empty coverage to {common}");
            *self = rebased;
            return Ok(());
        }
        self.apply(a, removed, added)
    }
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
    fn closed_code(&self, text: &str) -> Result<bool> {
        let prefix = parse_code(text)?;
        let matching: Vec<_> = self.records.iter().filter(|(id, _)| identity_digits(**id).starts_with(&prefix)).collect();
        Ok(!matching.is_empty() && matching.iter().all(|(_, record)| !record.open))
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
#[derive(Debug)]
struct DirectoryFailure { message: String, temporary: bool }
impl std::fmt::Display for DirectoryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.message) }
}
impl std::error::Error for DirectoryFailure {}
pub fn retryable_lookup_error(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<DirectoryFailure>().is_some_and(|e| e.temporary)
}
fn directory_unavailable(authority: &str, phase: &str, error: impl std::fmt::Display) -> Box<dyn std::error::Error> {
    Box::new(DirectoryFailure { message: format!("invitation directory {authority} {phase} failed: {error}"), temporary: true })
}
fn fetch(url: &str, route: &str) -> Result<Vec<Transaction>> {
    let authority = endpoint(url)?;
    let addresses = authority.to_socket_addrs().map_err(|e| directory_unavailable(authority, "DNS lookup", e))?;
    let mut stream = None;
    let mut last_error = String::from("no resolved addresses");
    for address in addresses {
        match TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
            Ok(s) => { stream = Some(s); break; }
            Err(error) => last_error = error.to_string(),
        }
    }
    let mut stream = stream.ok_or_else(|| directory_unavailable(authority, "connection", last_error))?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| directory_unavailable(authority, "read setup", e))?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(|e| directory_unavailable(authority, "write setup", e))?;
    // Send a complete header rather than several small formatted writes into
    // the server's bounded header-read window.
    let request = format!("GET {route} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).map_err(|e| directory_unavailable(authority, "request", e))?;
    let mut bytes = Vec::new();
    stream.take(MAX_RESPONSE + 1).read_to_end(&mut bytes).map_err(|e| directory_unavailable(authority, "response", e))?;
    if bytes.len() as u64 > MAX_RESPONSE {
        return Err("directory response exceeds limit".into());
    }
    if bytes.is_empty() { return Err(directory_unavailable(authority, "response", "connection closed without a response")); }
    let split = bytes.windows(4).position(|s| s == b"\r\n\r\n").ok_or("invalid directory HTTP response")?;
    if !bytes.starts_with(b"HTTP/1.0 200 ") {
        if [429, 500, 502, 503, 504].iter().any(|status| bytes.starts_with(format!("HTTP/1.0 {status} ").as_bytes())) {
            return Err(directory_unavailable(authority, "response", String::from_utf8_lossy(&bytes[..split]).lines().next().unwrap_or("temporary HTTP failure")));
        }
        if bytes.starts_with(b"HTTP/1.0 410 ") {
            return Err("invitation already joined or cancelled; ask the host for a fresh game code".into());
        }
        let status = String::from_utf8_lossy(&bytes[..split]).lines().next().unwrap_or("unknown HTTP status").to_owned();
        return Err(format!("invitation directory rejected lookup ({status}); ensure the directory uses current rules v{GAME_VERSION} and the correct network").into());
    }
    Ok(serde_json::from_slice(&bytes[split + 4..])?)
}

/// HTTP never waits on a chain RPC or a cache write. Each request sees one
/// immutable, coherent directory snapshot, which carries no spend authority.
struct DirectoryHttp {
    snapshot: Arc<RwLock<Arc<Index>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<std::result::Result<(), String>>>,
}
impl DirectoryHttp {
    fn start(a: &Artifact, index: &Index, listener: TcpListener) -> Result<Self> {
        listener.set_nonblocking(true)?;
        let snapshot = Arc::new(RwLock::new(Arc::new(index.clone())));
        let stop = Arc::new(AtomicBool::new(false));
        let shared = snapshot.clone();
        let stopped = stop.clone();
        let artifact = a.clone();
        let worker = std::thread::Builder::new().name("pong-directory-http".into()).spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let current = shared.read().map_err(|_| "directory snapshot lock poisoned")?.clone();
                // One bounded request per iteration keeps shutdown responsive.
                serve_requests(&artifact, &current, &listener, 1).map_err(|e| e.to_string())?;
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        })?;
        Ok(Self { snapshot, stop, worker: Some(worker) })
    }
    fn publish(&self, index: &Index) -> Result<()> {
        let next = Arc::new(index.clone());
        let previous = std::mem::replace(&mut *self.snapshot.write().map_err(|_| "directory snapshot lock poisoned")?, next);
        drop(previous);
        Ok(())
    }
}
impl Drop for DirectoryHttp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
    }
}

#[cfg(test)]
mod directory_http_tests {
    use super::*;
    fn reply_once(response: &'static [u8]) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap(); header.push(byte[0]);
                assert!(header.len() <= 1024);
            }
            stream.write_all(response).unwrap();
        });
        (url, worker)
    }
    #[test]
    fn transient_transport_and_http_failures_retry_but_invalid_data_does_not() {
        for response in [
            b"".as_slice(),
            b"HTTP/1.0 429 Busy\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.0 500 Error\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.0 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.0 503 Unavailable\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.0 504 Timeout\r\nContent-Length: 0\r\n\r\n",
        ] {
            let (url, worker) = reply_once(response);
            let error = fetch(&url, "/lookup/test").unwrap_err();
            assert!(retryable_lookup_error(error.as_ref()), "{error}"); worker.join().unwrap();
        }
        for response in [
            b"HTTP/1.0 410 Gone\r\nContent-Length: 2\r\n\r\n[]".as_slice(),
            b"HTTP/1.0 400 Bad Request\r\nContent-Length: 2\r\n\r\n[]",
            b"HTTP/1.0 200 OK\r\nContent-Length: 3\r\n\r\nbad",
            b"not an HTTP response",
        ] {
            let (url, worker) = reply_once(response);
            let error = fetch(&url, "/lookup/test").unwrap_err();
            assert!(!retryable_lookup_error(error.as_ref()), "{error}"); worker.join().unwrap();
        }
        let error = fetch("https://invalid-directory", "/lookup/test").unwrap_err();
        assert!(!retryable_lookup_error(error.as_ref()));
    }
    #[tokio::test]
    async fn async_lookup_preserves_retry_classification_and_can_recover() {
        let a: Artifact = serde_json::from_str(include_str!("../../artifacts/artifact.json")).unwrap();
        let game_code = code(Hash::from_bytes([7;32]));
        let (url, worker) = reply_once(b"HTTP/1.0 503 Unavailable\r\nContent-Length: 0\r\n\r\n");
        let mut config = Config { invitation_index: Some(url.clone()), ..Default::default() };
        let error = discover_pending(&a, &config, &[0;32], &game_code).await.unwrap_err();
        assert!(retryable_lookup_error(error.as_ref())); worker.join().unwrap();
        let error = discover_pending(&a, &config, &[0;32], &game_code).await.unwrap_err();
        assert!(retryable_lookup_error(error.as_ref()), "closed listener must remain retryable");
        let (url, worker) = reply_once(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n[]");
        config.invitation_index = Some(url);
        assert!(discover_pending(&a, &config, &[0;32], &game_code).await.unwrap().is_none()); worker.join().unwrap();
        let (url, worker) = reply_once(b"HTTP/1.0 410 Gone\r\nContent-Length: 2\r\n\r\n[]");
        config.invitation_index = Some(url);
        let error = discover_pending(&a, &config, &[0;32], &game_code).await.unwrap_err();
        assert!(!retryable_lookup_error(error.as_ref())); worker.join().unwrap();
    }
    #[test]
    fn initial_anchor_rollback_rebases_only_an_empty_index() {
        let a: Artifact = serde_json::from_str(include_str!("../../artifacts/artifact.json")).unwrap();
        let anchor = Hash::from_bytes([1;32]);
        let common = Hash::from_bytes([0;32]);
        let added = [ChainBlock { hash: Hash::from_bytes([2;32]), daa: 10, transactions: vec![] }];
        let mut index = Index::new("devnet", anchor);
        assert!(index.apply_tracked(&a, &[anchor], &added, None).is_err());
        assert_eq!(index.cursor, anchor);
        index.apply_tracked(&a, &[anchor], &added, Some(common)).unwrap();
        assert_eq!((index.coverage_anchor, index.cursor), (common, added[0].hash));
        let before = serde_json::to_value(&index).unwrap();
        assert!(index.apply_tracked(&a, &[added[0].hash, common], &[], Some(anchor)).is_err());
        assert_eq!(serde_json::to_value(index).unwrap(), before, "established deep rollback still fails atomically");
    }
    #[test]
    fn lookup_remains_responsive_without_chain_progress_and_publishes_coherent_snapshots() {
        let a: Artifact = serde_json::from_str(include_str!("../../artifacts/artifact.json")).unwrap();
        let index = Index::new("devnet", Hash::from_bytes([0;32]));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let http = DirectoryHttp::start(&a, &index, listener).unwrap();
        // The caller supplies no chain progress at all: serving owns its own
        // thread and cannot be gated by an awaited delta or diagnostic save.
        for _ in 0..3 { assert!(fetch(&url, &format!("/lookup/{}", code(Hash::from_bytes([7;32])))).unwrap().is_empty()); }
        let mut next = index.clone(); next.cursor = Hash::from_bytes([1;32]);
        http.publish(&next).unwrap();
        assert_eq!(http.snapshot.read().unwrap().cursor, next.cursor);
        assert!(fetch(&url, &format!("/lookup/{}", code(Hash::from_bytes([8;32])))).unwrap().is_empty());
        drop(http);
        assert!(TcpStream::connect(endpoint(&url).unwrap()).is_err(), "listener closes on shutdown");
    }
    #[test]
    fn closed_code_fails_immediately_and_reopened_snapshot_is_discoverable() {
        let a: Artifact = serde_json::from_str(include_str!("../../artifacts/artifact.json")).unwrap();
        let mut index = Index::new("devnet", Hash::from_bytes([0;32]));
        let tx = Transaction::new(0, vec![], vec![], 0, kaspa_consensus_core::subnets::SUBNETWORK_ID_NATIVE, 0, vec![]);
        let game_code = code(tx.id());
        index.records.insert(tx.id(), Indexed { transaction: tx, open: false });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let http = DirectoryHttp::start(&a, &index, listener).unwrap();
        let error = fetch(&url, &format!("/lookup/{game_code}")).unwrap_err().to_string();
        assert!(error.contains("already joined or cancelled"));
        for record in index.records.values_mut() { record.open = true; }
        http.publish(&index).unwrap();
        assert_eq!(fetch(&url, &format!("/lookup/{game_code}")).unwrap().len(), 1, "reorg reopening is not permanently cached");
    }
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
    let transactions = tokio::task::spawn_blocking(move || fetch(&url, &format!("/lookup/{code}")).map_err(|e|
        DirectoryFailure { temporary: retryable_lookup_error(e.as_ref()), message: e.to_string() }))
        .await??;
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
        if route == "/health" {
            let body = serde_json::to_vec(&serde_json::json!({"version": index.version, "network": index.network, "artifact_id": a.id.to_string(), "cursor": index.cursor, "coverage_anchor": index.coverage_anchor}))?;
            let _ = write!(stream, "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = stream.write_all(&body);
            served += 1;
            continue;
        }
        if let Some(text) = route.strip_prefix("/lookup/") {
            if index.closed_code(text).unwrap_or(false) {
                let _ = stream.write_all(b"HTTP/1.0 410 Gone\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]");
                served += 1;
                continue;
            }
        }
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
    let mut node = Node::connect(c).await?;
    let result = async {
        let mut index: Index = if path.exists() {
            super::load(path)?
        } else {
            // New deployments are for future invitations, not an implicit
            // multi-day Full-history replay. Historical coverage is explicit.
            Index::new(&c.network, anchor.unwrap_or(node.info().await?.sink))
        };
        if index.version != GAME_VERSION || index.network != c.network || anchor.is_some_and(|h| h != index.coverage_anchor) {
            return Err("directory belongs to another version/network/coverage anchor; choose a fresh path".into());
        }
        let listener = TcpListener::bind(listen)?;
        let http = DirectoryHttp::start(a, &index, listener)?;
        eprintln!("Invitation directory listening on {listen}; coverage begins after {}; no wallet is used.", index.coverage_anchor);
        loop {
            if http.worker.as_ref().is_some_and(|worker| worker.is_finished()) {
                return Err("invitation directory HTTP worker stopped".into());
            }
            let delta = match node.delta(index.cursor).await {
                Ok(delta) => delta,
                Err(error) => {
                    eprintln!("Directory chain tracking interrupted; retaining lookup snapshot and reconnecting: {error}");
                    let _ = node.rpc.disconnect().await;
                    match Node::connect(c).await {
                        Ok(reconnected) => node = reconnected,
                        Err(error) => eprintln!("Directory reconnect deferred: {error}"),
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            if !delta.removed.is_empty() || !delta.added.is_empty() {
                index.apply_tracked(a, &delta.removed, &delta.added, delta.common)?;
                super::save(path, &index)?;
                http.publish(&index)?;
            }
            // The HTTP worker serves independently of tracking and persistence.
            tokio::time::sleep(Duration::from_millis(c.poll_ms)).await;
        }
    }
    .await;
    let _ = node.rpc.disconnect().await;
    result
}
