//! Versioned compact codes and signed reservation payloads for reusable games.
use super::{
    invitations::GameTerms,
    rpc::Node,
    tracker::AcceptedTx,
};
use crate::{
    engine::*,
    reusable::{self, Lane, Offer, Set},
};
use argent_runtime::TxBuilder;
use kaspa_consensus_core::{Hash, tx::Transaction};
use serde::{Deserialize, Serialize};

pub const MAGIC: &[u8]=b"QGAME9";
const DESCRIPTOR_BYTES: usize = 129;
const HOST_BYTES: usize = 40;
fn digest(domain: &[u8], data: &[u8]) -> Vec<u8> {
    blake2b_simd::Params::new().hash_length(32).to_state().update(domain).update(data).finalize().as_bytes().to_vec()
}
fn network_id(network: &str) -> [u8; 8] {
    digest(b"Pong/queue-network/v2", network.as_bytes())[..8].try_into().unwrap()
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Descriptor {
    pub owner: Vec<u8>,
    pub id: Hash,
    pub last_game: Hash,
    pub generation: i64,
    pub reserves: [u64; 3],
    pub side: u8,
}
impl Descriptor {
    pub fn from_set(set: &Set) -> Result<Self> {
        if !set.idle()
            || set.lanes.iter().any(|c| {
                c.state.queue.count != 0
                    || c.state.queue.y != 16
                    || c.state.queue.through != 0
                    || c.state.queue.times != [0; 4]
                    || c.state.queue.commands != [0; 4]
            })
        {
            return Err("game code requires idle, reset queues".into());
        }
        Ok(Self {
            owner: set.lanes[0].state.queue.owner.clone(),
            id: set.id()?,
            last_game: set.lanes[0].state.game,
            generation: set.generation(),
            reserves: std::array::from_fn(|i| set.lanes[i].state.reserve),
            side: u8::try_from(set.lanes[0].state.queue.side)?,
        })
    }
    pub fn states(&self) -> Result<[Lane; 3]> {
        secp256k1::XOnlyPublicKey::from_slice(&self.owner)?;
        let total = self.reserves.iter().try_fold(0u64, |s, v| s.checked_add(*v).ok_or("reserve overflow"))?;
        if self.side > 1
            || self.generation < 0
            || self.generation == i64::MAX
            || total < reusable::MIN_RESERVE
            || self.reserves.iter().any(|r| *r > i64::MAX as u64)
        {
            return Err("invalid queue descriptor".into());
        }
        Ok(std::array::from_fn(|lane| {
            let mut queue = Queue::empty(self.owner.clone(), self.side as i64);
            queue.lane = lane;
            Lane { queue, game: self.last_game, generation: self.generation, reserve: self.reserves[lane], exposure: 0, active: false }
        }))
    }
    fn encode(&self) -> Result<Vec<u8>> {
        self.states()?;
        let mut out = self.owner.clone();
        out.extend(self.id.as_bytes());
        out.extend(self.last_game.as_bytes());
        out.extend(self.generation.to_le_bytes());
        for r in self.reserves {
            out.extend(r.to_le_bytes());
        }
        out.push(self.side);
        Ok(out)
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != DESCRIPTOR_BYTES {
            return Err("invalid queue descriptor length".into());
        }
        let result = Self {
            owner: bytes[..32].to_vec(),
            id: Hash::from_bytes(bytes[32..64].try_into()?),
            last_game: Hash::from_bytes(bytes[64..96].try_into()?),
            generation: i64::from_le_bytes(bytes[96..104].try_into()?),
            reserves: std::array::from_fn(|i| u64::from_le_bytes(bytes[104 + i * 8..112 + i * 8].try_into().unwrap())),
            side: bytes[128],
        };
        result.states()?;
        Ok(result)
    }
    pub async fn accepted(&self, node: &Node, b: &TxBuilder<'_>) -> Result<Set> {
        let states = self.states()?;
        let scripts = states
            .iter()
            .map(|s| b.covenant_utxo(s.actor(), s.state(), 1, 0, false, Some(self.id)).map(|u| u.script_public_key))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let found = node.utxos(&scripts).await?;
        let lanes = states
            .iter()
            .enumerate()
            .map(|(i, state)| {
                let candidates = found
                    .iter()
                    .filter(|f| {
                        f.utxo.script_public_key == scripts[i]
                            && f.utxo.covenant_id == Some(self.id)
                            && f.utxo.block_daa_score != u64::MAX
                    })
                    .collect::<Vec<_>>();
                if candidates.len() != 1 {
                    return Err("host queues changed or are busy; request a fresh game code".into());
                }
                Ok(Cell { outpoint: candidates[0].outpoint, utxo: candidates[0].utxo.clone(), state: state.clone() })
            })
            .collect::<Result<Vec<_>>>()?
            .try_into()
            .map_err(|_| "wrong queue count")?;
        let set = Set { lanes };
        set.validate(b)?;
        Ok(set)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostCode {
    pub network: [u8; 8],
    pub tag: [u8; 8],
    pub queue: Descriptor,
}
impl HostCode {
    pub fn new(network: &str, tag: [u8; 8], set: &Set) -> Result<Self> {
        let code = Self { network: network_id(network), tag, queue: Descriptor::from_set(set)? };
        code.terms()?;
        Ok(code)
    }
    pub fn terms(&self) -> Result<GameTerms> {
        GameTerms::from_tag(&self.tag)
    }
    fn bytes(&self)->Result<Vec<u8>> {self.terms()?;secp256k1::XOnlyPublicKey::from_slice(&self.queue.owner)?;let mut bytes=self.queue.owner.clone();bytes.extend(self.tag);Ok(bytes)}
    fn from_bytes(bytes:&[u8])->Result<Self>{
        if bytes.len()!=HOST_BYTES{return Err("invalid compact invitation length".into());}
        let code=Self{network:[0;8],tag:bytes[32..].try_into()?,queue:Descriptor{owner:bytes[..32].to_vec(),id:Hash::from_bytes([0;32]),last_game:Hash::from_bytes([0;32]),generation:0,reserves:[0;3],side:0}};
        code.bytes()?;Ok(code)
    }
    pub fn text(&self)->Result<String>{super::invitations::rendezvous_code(&self.queue.owner,&self.tag)}
    pub fn parse(text:&str)->Result<Self>{let (owner,tag,_)=super::invitations::parse_rendezvous_code(text)?;let mut bytes=owner;bytes.extend(tag);Self::from_bytes(&bytes)}

}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reservation {
    pub host: HostCode,
    pub guest: Descriptor,
    pub capital: u64,
}
impl Reservation {
    pub fn offer(&self) -> Result<Offer> {
        let terms = self.host.terms()?;
        if self.guest.owner == self.host.queue.owner {
            return Err("a game needs different players and queue sets".into());
        }
        self.guest.states()?;
        let lobby = Lobby {
            left: self.guest.owner.clone(),
            right: self.host.queue.owner.clone(),
            refund: self.guest.owner.clone(),
            control_value: 0,
            ball_tick_daa: terms.speed.tick_daa(),
            stake_per_player: i64::from(terms.stake_kas) * 100_000_000,
            ball_count: i64::from(terms.ball_count),
            win_score: i64::from(terms.win_score),
        };
        Ok(Offer {
            lobby,
            pools: [self.guest.id, Hash::from_bytes(self.host.queue.owner.as_slice().try_into()?)],
            generations: [self.guest.generation, 0],
        })
    }
    pub fn payload(&self) -> Result<Vec<u8>> {
        self.offer()?;
        let mut out = MAGIC.to_vec();
        out.extend(self.host.bytes()?);
        out.extend(self.guest.encode()?);
        out.extend(self.capital.to_le_bytes());
        Ok(out)
    }
    pub fn parse(tx: &Transaction, _network: &str) -> Result<Self> {
        let bytes = tx.payload.strip_prefix(MAGIC).ok_or("not a current queue reservation")?;
        if bytes.len() != HOST_BYTES + DESCRIPTOR_BYTES + 8 {
            return Err("invalid queue reservation length".into());
        }
        let reservation = Self {
            host: HostCode::from_bytes(&bytes[..HOST_BYTES])?,
            guest: Descriptor::decode(&bytes[HOST_BYTES..HOST_BYTES + DESCRIPTOR_BYTES])?,
            capital: u64::from_le_bytes(bytes[HOST_BYTES + DESCRIPTOR_BYTES..].try_into()?),
        };

        reservation.offer()?;
        Ok(reservation)
    }
    pub fn accepted(&self, b: &TxBuilder<'_>, record: &AcceptedTx, daa: u64) -> Result<(Cell<Offer>, Set)> {
        let tx = &record.tx;
        if record.inputs.len() != tx.inputs.len() || tx.inputs.len() < 3 || tx.outputs.len() < 4 {
            return Err("reservation lacks complete inputs/outputs".into());
        }
        let states = self.guest.states()?;
        let before = Set {
            lanes: std::array::from_fn(|i| Cell {
                outpoint: tx.inputs[i].previous_outpoint,
                utxo: record.inputs[i].clone(),
                state: states[i].clone(),
            }),
        };
        before.validate(b)?;
        if self.capital < reusable::required_capital(&before, self.offer()?.lobby.stake_per_player as u64)?
            || self.capital < before.balance()
        {
            return Err("guest queues do not back the promised wager".into());
        }
        for i in 0..tx.inputs.len() {
            argent_runtime::execute_input_with_covenants(tx, record.inputs.clone(), i)?;
        }
        if storage_mass(tx, record.inputs.clone())? != tx.storage_mass() {
            return Err("reservation has invalid storage mass".into());
        }
        let offer = self.offer()?;
        let expected = b.genesis_output("Lobby", offer.state(), offer.lobby.stake_per_player as u64 / 2)?;
        if tx.outputs[0].script_public_key != expected.script_public_key
            || tx.outputs[0].value != expected.value
            || tx.outputs[0].covenant.is_none_or(|c| c.authorizing_input != 0 || c.covenant_id == self.guest.id)
        {
            return Err("reservation does not create the agreed Lobby".into());
        }
        let queue_value = tx.outputs[1..4].iter().try_fold(0u64, |total, output| {
            total.checked_add(output.value).ok_or("guest queue balance overflow")
        })?;
        if self.capital.checked_sub(expected.value) != Some(queue_value) {
            return Err("reservation capital does not match its queue outputs".into());
        }
        let (lobby, set) = reusable::reserved(tx, &before, offer, self.capital, daa);
        set.validate(b)?;
        Ok((lobby, set))
    }
}

// Only identity and invitation freshness belong to the exchanged code. Queue
// descriptors on a locally persisted host offer are never encoded into it.
impl PartialEq for HostCode {fn eq(&self,other:&Self)->bool{self.tag==other.tag && self.queue.owner==other.queue.owner}}
impl Eq for HostCode {}
pub fn started_host(b:&TxBuilder<'_>,record:&AcceptedTx,game:Hash,wager:u64)->Result<Set>{
    let bytes=record.tx.payload.strip_prefix(reusable::START_MAGIC).ok_or("start lacks host queue proof")?;
    let d=Descriptor::decode(bytes)?;let before=d.states()?;
    let states=std::array::from_fn::<_,3,_>(|i|before[i].reset(game,d.generation,1,reusable::part(wager-wager/2,i),true));
    // The covenant binds the disclosed metadata to its observed host pool.
    // Read canonical successors, avoiding any dependence on idle source fields
    // that the start legitimately resets (old paddle history or integer encodings).
    let lanes=states.iter().map(|state|{
        let script=b.covenant_utxo(state.actor(),state.state(),1,0,false,Some(d.id))?.script_public_key;
        let matches=record.tx.outputs.iter().enumerate().filter(|(_,o)|o.covenant.is_some_and(|c|c.covenant_id==d.id) && o.script_public_key==script).collect::<Vec<_>>();
        if matches.len()!=1{return Err("start does not prove a unique host queue successor".into());}
        let (i,_)=matches[0];Ok(accepted_cell(&record.tx,i,0,state.clone()))
    }).collect::<Result<Vec<_>>>()?.try_into().map_err(|_|"wrong host queue count")?;
    let set=Set{lanes};set.validate(b)?;Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use secp256k1::{Keypair, Secp256k1, SecretKey};
    #[test]
    fn codes_match_previous_format_and_guest_metadata_stays_in_reservation() {
        let key = Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[13; 32]).unwrap());
        let d = Descriptor {
            owner: pubkey(&key),
            id: Hash::from_bytes([1; 32]),
            last_game: Hash::from_bytes([2; 32]),
            generation: 3,
            reserves: [100_000_000; 3],
            side: 1,
        };
        let terms = GameTerms { speed: GameSpeed::Medium, ball_count: 3, win_score: 6, stake_kas: 201 };
        let tag = super::super::invitations::new_rendezvous_tag(terms).unwrap();
        let c = HostCode { network: network_id("testnet-10"), tag, queue: d.clone() };
        let text = c.text().unwrap();
        assert!(text.len() < 256);
        assert_eq!(HostCode::parse(&text).unwrap(), c);
        assert_eq!(text,super::super::invitations::rendezvous_code(&c.queue.owner,&tag).unwrap());
        assert!(text.len()<=61);
        let mut bad = text.into_bytes();
        let n = bad.len() - 1;
        bad[n] = if bad[n] == b'1' { b'2' } else { b'1' };
        assert!(HostCode::parse(std::str::from_utf8(&bad).unwrap()).is_err());
        let mut guest = d;
        guest.id = Hash::from_bytes([3; 32]);
        guest.owner = pubkey(&Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[14; 32]).unwrap()));
        let r = Reservation { host: c, guest, capital: 20_400_000_000 };
        assert!(r.payload().unwrap().len() < 320);
    }
}
