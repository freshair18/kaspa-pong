//! Game terms and encoding of compact host codes.
use crate::engine::{GameSpeed, Result};
use secp256k1::XOnlyPublicKey;

const BASE58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
pub const RENDEZVOUS_BYTES: usize = 8;
/// Code-format version, independent of the covenant version.
pub const RENDEZVOUS_CODE_VERSION: u8 = 15;
/// Eight tag bytes: version/speed/balls/goals/stake (24 bits), then randomness.
const RENDEZVOUS_RANDOM_BITS: u32 = 40;
const RENDEZVOUS_RANDOM_MASK: u64 = (1 << RENDEZVOUS_RANDOM_BITS) - 1;
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
        if !(1..=15).contains(&self.speed.tick_daa()) {
            return Err("invalid game speed: expected 1–15 DAA per tick".into());
        }
        if !(1..=8).contains(&self.ball_count) || !(1..=15).contains(&self.win_score) {
            return Err("game code requires 1–8 balls and a winning score of 1–15".into());
        }
        if !(5..=254).contains(&self.stake_kas) {
            return Err("game codes require 5–254 whole KAS per player".into());
        }
        Ok(())
    }
    fn tag(self) -> Result<[u8; RENDEZVOUS_BYTES]> {
        self.validate()?;
        // Network byte order, from most to least significant: 5/4/3/4/8/40 bits.
        Ok(((u64::from(RENDEZVOUS_CODE_VERSION) << 59)
            | (u64::from(self.speed.code()) << 55)
            | (u64::from(self.ball_count - 1) << 52)
            | (u64::from(self.win_score) << 48)
            | (u64::from(self.stake_kas) << RENDEZVOUS_RANDOM_BITS))
            .to_be_bytes())
    }
    pub fn from_tag(tag: &[u8; RENDEZVOUS_BYTES]) -> Result<Self> {
        let bits = u64::from_be_bytes(*tag);
        let version = (bits >> 59) as u8;
        if version != RENDEZVOUS_CODE_VERSION {
            return Err(
                format!("unsupported game code version {version}; this client supports version {RENDEZVOUS_CODE_VERSION}").into()
            );
        }
        let terms = Self {
            speed: GameSpeed::from_code(((bits >> 55) & 15) as u8)?,
            ball_count: ((bits >> 52) & 7) as u8 + 1,
            win_score: ((bits >> 48) & 15) as u8,
            stake_kas: ((bits >> RENDEZVOUS_RANDOM_BITS) & 255) as u8,
        };
        terms.validate()?;
        Ok(terms)
    }
    pub fn summary(self) -> String {
        format!(
            "{} · {} ball{} · First to {} · {} KAS each",
            self.speed.label(),
            self.ball_count,
            if self.ball_count == 1 { "" } else { "s" },
            self.win_score,
            self.stake_kas
        )
    }
}

// Retain the checksum domain used by existing compact host codes.
const DOMAIN: &[u8] = b"KaspaPong/creation-intent/26\0";

fn digest(bytes: &[u8]) -> [u8; 32] {
    let h = blake2b_simd::Params::new().hash_length(32).to_state().update(DOMAIN).update(bytes).finalize();
    h.as_bytes().try_into().unwrap()
}

pub fn rendezvous_code(public: &[u8], tag: &[u8; RENDEZVOUS_BYTES]) -> Result<String> {
    XOnlyPublicKey::from_slice(public)?;
    GameTerms::from_tag(tag)?;
    let mut payload = Vec::with_capacity(32 + RENDEZVOUS_BYTES + 4);
    payload.extend(public);
    payload.extend(tag);
    payload.extend(&digest(&payload)[..4]);
    Ok(base58_encode(&payload))
}
pub fn parse_rendezvous_code(text: &str) -> Result<(Vec<u8>, [u8; RENDEZVOUS_BYTES], GameTerms)> {
    let compact: String = text.chars().filter(|c| *c != '-' && !c.is_ascii_whitespace()).collect();
    let payload = base58_decode(&compact)?;
    if payload.len() != 32 + RENDEZVOUS_BYTES + 4 {
        return Err("expected a compact host game code".into());
    }
    let (signed, check) = payload.split_at(payload.len() - 4);
    if digest(signed)[..4] != *check {
        return Err("game code checksum failed; check the copied code".into());
    }
    let public = signed[..32].to_vec();
    XOnlyPublicKey::from_slice(&public)?;
    let mut tag = [0; RENDEZVOUS_BYTES];
    tag.copy_from_slice(&signed[32..]);
    Ok((public, tag, GameTerms::from_tag(&tag)?))
}
pub fn new_rendezvous_tag(terms: GameTerms) -> Result<[u8; RENDEZVOUS_BYTES]> {
    let mut random = [0; RENDEZVOUS_BYTES];
    getrandom::getrandom(&mut random).map_err(|_| "operating system randomness is unavailable")?;
    Ok((u64::from_be_bytes(terms.tag()?) | (u64::from_be_bytes(random) & RENDEZVOUS_RANDOM_MASK)).to_be_bytes())
}
pub(crate) fn base58_encode(bytes: &[u8]) -> String {
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
        while start < source.len() && source[start] == 0 {
            start += 1;
        }
    }
    encoded.extend(std::iter::repeat_n(b'1', zeros));
    encoded.reverse();
    String::from_utf8(encoded).unwrap()
}
pub(crate) fn base58_decode(text: &str) -> Result<Vec<u8>> {
    if text.is_empty() {
        return Err("expected a compact host game code".into());
    }
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
