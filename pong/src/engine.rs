//! Headless transaction construction and deterministic interpretation of Pong.
//! The .ag covenant, not this client, decides whether a successor is valid.
use argent_runtime::{ArtifactValue, ContextInput, EntryCall, InputSigScript, TxBuilder, TxContext, args, state};
use kaspa_consensus_core::{
    config::params::MAINNET_PARAMS,
    hashing::sighash::{SigHashReusedValuesUnsync, calc_schnorr_signature_hash},
    hashing::sighash_type::{SigHashType, SIG_HASH_ALL},
    mass::MassCalculator,
    tx::{CovenantBinding, MutableTransaction, PopulatedTransaction, ScriptPublicKey, Transaction, TransactionOutpoint, UtxoEntry},
};
use kaspa_txscript::{opcodes::codes::OpCheckSig, script_builder::ScriptBuilder};
use secp256k1::{Keypair, Message};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub type State = BTreeMap<String, ArtifactValue>;
pub const MAX_EVENTS: usize = 4;
pub const QUEUES_PER_PLAYER: usize = 3;
pub const QUEUE_COUNT: usize = 2 * QUEUES_PER_PLAYER;
// Actor values are allocations FROM the pot, not additional collateral.
// Six queues hold one KAS each; Court holds the rest. Fees use ordinary funding.
pub const QUEUE_DEPOSIT: u64 = 100_000_000;
pub const SIDE_DEPOSIT: u64 = QUEUE_DEPOSIT * QUEUES_PER_PLAYER as u64;
/// Fixed client policy; the signed covenant parameter permits future amounts.
pub const DEFAULT_STAKE: i64 = 500_000_000;
pub const GAME_POT: u64 = 2 * DEFAULT_STAKE as u64;
pub const COURT_VALUE: u64 = GAME_POT - 2 * SIDE_DEPOSIT;
pub const MAX_STAKE: i64 = i64::MAX / 2;
pub fn stake_amount(stake: i64) -> Result<u64> {
    if !(SIDE_DEPOSIT as i64 + 1..=MAX_STAKE).contains(&stake) {
        return Err("stake per player is outside the supported contract range".into());
    }
    Ok(stake as u64)
}
pub const WIN_SCORE: i64 = 3;
pub const COURT_WIDTH: i64 = 58;
pub const COURT_HEIGHT: i64 = 32;
pub const LEFT_CONTACT_X: i64 = 2;
pub const RIGHT_CONTACT_X: i64 = COURT_WIDTH - 3;
pub const CENTER_X: i64 = COURT_WIDTH / 2;
pub const BALL_SPEED: i64 = 2;
pub const PADDLE_RADIUS: i64 = 4;
/// The four-dot-tall sprite overlaps a paddle one cell beyond either end.
pub const BALL_RADIUS: i64 = 1;
pub const PADDLE_MIN_Y: i64 = PADDLE_RADIUS;
pub const PADDLE_MAX_Y: i64 = 31 - PADDLE_RADIUS;
pub const MAX_MOVE_STEPS: i64 = 27;
/// The interval between ball movement steps in chain DAA ticks.
/// This setting is selected by the host for each newly created game.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GameSpeed {
    Fast,
    #[default]
    Medium,
    Slow,
}
impl GameSpeed {
    pub const fn tick_daa(self) -> i64 {
        match self {
            Self::Fast => 6,
            Self::Medium => 7,
            Self::Slow => 8,
        }
    }
    pub const fn code(self) -> u8 {
        match self {
            Self::Fast => 0,
            Self::Medium => 1,
            Self::Slow => 2,
        }
    }
    pub fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Fast),
            1 => Ok(Self::Medium),
            2 => Ok(Self::Slow),
            _ => Err("invalid game speed in code".into()),
        }
    }
    pub const fn from_tick_daa(ticks: i64) -> Option<Self> {
        match ticks {
            6 => Some(Self::Fast),
            7 => Some(Self::Medium),
            8 => Some(Self::Slow),
            _ => None,
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Medium => "Medium",
            Self::Slow => "Slow",
        }
    }
}
pub const DEFAULT_BALL_TICK_DAA: i64 = GameSpeed::Medium.tick_daa();
pub const MAX_BALL_TICK_DAA: i64 = 64;
pub fn valid_ball_tick_daa(ticks: i64) -> bool {
    (1..=MAX_BALL_TICK_DAA).contains(&ticks)
}
pub const SERVE_DELAY_DAA: i64 = 20;
pub const OPENING_DELAY_DAA: i64 = 25;
pub const DAA_PER_SECOND: i64 = 10;
pub const RESOLVE_DAA: u64 = 2;
/// Five nominal seconds at ten DAA/second; consensus does not promise wall time.
pub const RESPONSE_TIMEOUT_DAA: u64 = 50;
pub const GAME_VERSION: u32 = 23;
pub const GAME_ARTIFACT_ID: &str = "7a8d8452c58c14f6d444e9023649bcb830877b5c8603fa1ff084cd58ac9ca9ec";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lobby {
    pub left: Vec<u8>,
    pub right: Vec<u8>,
    pub refund: Vec<u8>,
    pub control_value: i64,
    pub ball_tick_daa: i64,
    pub stake_per_player: i64,
}
impl Lobby {
    pub fn state(&self) -> State {
        state! { left: self.left.clone(), right: self.right.clone(), refund: self.refund.clone(), control_value: self.control_value, ball_tick_daa: self.ball_tick_daa, stake_per_player: self.stake_per_player }
    }
    pub fn opening(&self) -> Plan {
        let mut world = World::opening(self.left.clone(), self.right.clone());
        world.ball_tick_daa = self.ball_tick_daa;
        world.stake_per_player = self.stake_per_player;
        Plan { world, queues: empty_queues(&self.left, &self.right) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct World {
    pub left: Vec<u8>,
    pub right: Vec<u8>,
    pub stake_per_player: i64,
    pub started: bool,
    pub clock: i64,
    pub ball_tick_daa: i64,
    pub left_y: i64,
    pub right_y: i64,
    pub ball_x: i64,
    pub ball_y: i64,
    pub ball_vx: i64,
    pub ball_vy: i64,
    pub serve_until: i64,
    pub left_score: i64,
    pub right_score: i64,
    pub resolver: i64,
    pub resolves: i64,
    pub takeovers: i64,
    /// -1 binds a new flight/progress anchor to this Court's actual acceptance.
    pub response_at: i64,
}

impl World {
    fn choose_serve(&mut self, daa: i64, total: i64) {
        let choice = serve_choice(daa, total, &self.left, &self.right);
        self.ball_y = 2 + choice / 10;
        self.ball_vx = if choice % 2 == 0 { -1 } else { 1 };
        self.ball_vy = (choice / 2) % 5 - 2;
    }
    fn start(&mut self, born: u64) -> Result<()> {
        if !valid_ball_tick_daa(self.ball_tick_daa) {
            return Err("invalid ball speed".into());
        }
        if !self.started {
            self.clock = accepted_score(born)?;
            self.serve_until = self.clock.checked_add(OPENING_DELAY_DAA).ok_or("opening deadline overflow")?;
            self.choose_serve(self.clock, 0);
            self.started = true;
        }
        Ok(())
    }
    pub fn opening(left: Vec<u8>, right: Vec<u8>) -> Self {
        Self {
            left,
            right,
            stake_per_player: DEFAULT_STAKE,
            started: false,
            clock: 0,
            ball_tick_daa: DEFAULT_BALL_TICK_DAA,
            left_y: 16,
            right_y: 16,
            ball_x: CENTER_X,
            ball_y: 16,
            ball_vx: 1,
            ball_vy: 0,
            serve_until: 0,
            left_score: 0,
            right_score: 0,
            resolver: 0,
            resolves: 0,
            takeovers: 0,
            response_at: -1,
        }
    }
    pub fn state(&self) -> State {
        state! { left: self.left.clone(), right: self.right.clone(), stake_per_player: self.stake_per_player,
        started: self.started, clock: self.clock, ball_tick_daa: self.ball_tick_daa, left_y: self.left_y, right_y: self.right_y,
        ball_x: self.ball_x,
        ball_y: self.ball_y, ball_vx: self.ball_vx, ball_vy: self.ball_vy, serve_until: self.serve_until,
        left_score: self.left_score, right_score: self.right_score,
        resolver: self.resolver, resolves: self.resolves, takeovers: self.takeovers, response_at: self.response_at }
    }
    pub fn ended(&self) -> bool {
        self.left_score == WIN_SCORE || self.right_score == WIN_SCORE
    }
    pub fn result(&self) -> Finish {
        Finish {

            stake_per_player: self.stake_per_player,
            left_score: self.left_score,
            right_score: self.right_score,
            winner: if self.left_score == WIN_SCORE { 0 } else { 1 },
            end_daa: self.clock,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queue {
    pub owner: Vec<u8>,
    pub side: i64,
    pub lane: usize,
    pub count: usize,
    pub times: [i64; 4],
    pub commands: [i64; 4],
    pub y: i64,
    /// Every movement strictly before this DAA has been applied (lane zero).
    pub through: i64,
}
impl Queue {
    pub fn empty(owner: Vec<u8>, side: i64) -> Self {
        Self { owner, side, lane: 0, count: 0, times: [0; 4], commands: [0; 4], y: 16, through: 0 }
    }
    pub fn actor(&self) -> &'static str {
        "Player"
    }
    pub fn state(&self) -> State {
        state! { owner: self.owner.clone(), side: self.side, lane: self.lane as i64, count: self.count as i64,
        t0: self.times[0], t1: self.times[1], t2: self.times[2], t3: self.times[3],
        d0: self.commands[0], d1: self.commands[1], d2: self.commands[2], d3: self.commands[3], y: self.y, through: self.through }
    }
    pub fn stamp(&mut self, accepted: u64) -> Result<()> {
        let daa = accepted_score(accepted)?;
        if self.count > 4 {
            return Err("invalid queue length".into());
        }
        if self.count > 0 && self.times[self.count - 1] == -1 {
            self.times[self.count - 1] = daa;
        }
        Ok(())
    }
    pub fn push(&self, direction: i64, accepted: u64) -> Result<Self> {
        if direction == 0 || !(-MAX_MOVE_STEPS..=MAX_MOVE_STEPS).contains(&direction) {
            return Err("movement must be -27..-1 (up) or 1..27 (down) steps".into());
        }
        if self.count >= 4 {
            return Err("queue full: resolve accepted inputs before submitting another".into());
        }
        let mut result = self.clone();
        result.stamp(accepted)?;
        result.times[result.count] = -1;
        result.commands[result.count] = direction;
        result.count += 1;
        Ok(result)
    }
    fn apply(&mut self, at: i64, paddle_y: &mut i64) {
        if self.count > 0 && self.times[0] == at {
            *paddle_y = (*paddle_y + self.commands[0]).clamp(PADDLE_MIN_Y, PADDLE_MAX_Y);
            self.count -= 1;
            self.times.rotate_left(1);
            self.times[3] = 0;
            self.commands.rotate_left(1);
            self.commands[3] = 0;
        }
    }
}

/// Stable covenant order: L0,R0,L1,R1,L2,R2.
pub fn empty_queues(left: &[u8], right: &[u8]) -> [Queue; QUEUE_COUNT] {
    std::array::from_fn(|i| {
        let mut q = Queue::empty(if i % 2 == 0 { left } else { right }.to_vec(), (i % 2) as i64);
        q.lane = i / 2;
        q
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finish {
    pub stake_per_player: i64,
    pub left_score: i64,
    pub right_score: i64,
    pub winner: i64,
    pub end_daa: i64,
}
impl Finish {
    pub fn state(&self) -> State {
        state! { stake_per_player: self.stake_per_player, left_score: self.left_score, right_score: self.right_score,
        winner: self.winner, end_daa: self.end_daa }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cell<T> {
    pub outpoint: TransactionOutpoint,
    pub utxo: UtxoEntry,
    pub state: T,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub court: Cell<World>,
    pub players: [Cell<Queue>; QUEUE_COUNT],
}
/// Final result authorization, before the separate collector refunds all lanes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Collection {
    pub left: Vec<u8>,
    pub right: Vec<u8>,
    pub finish: Finish,
}
impl Collection {
    pub fn state(&self) -> State {
        let mut result = self.finish.state();
        result.insert("left".into(), ArtifactValue::Bytes(self.left.clone()));
        result.insert("right".into(), ArtifactValue::Bytes(self.right.clone()));
        result
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Collecting {
    pub collector: Cell<Collection>,
    pub players: [Cell<Queue>; QUEUE_COUNT],
}
pub fn accepted_collection(snapshot: &Snapshot, tx: &Transaction, finish: Finish, daa: u64) -> Collecting {
    Collecting {
        collector: accepted_cell(tx, 0, daa, Collection {
            left: snapshot.court.state.left.clone(), right: snapshot.court.state.right.clone(), finish,
        }),
        players: snapshot.players.clone(),
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Funding {
    pub outpoint: TransactionOutpoint,
    pub utxo: UtxoEntry,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub world: World,
    pub queues: [Queue; QUEUE_COUNT],
}

fn accepted_score(score: u64) -> Result<i64> {
    i64::try_from(score).map_err(|_| "an accepted UTXO with a real DAA score is required".into())
}

/// The next paddle-plane crossing is fixed by the last accepted ball contact.
pub fn next_contact(snapshot: &Snapshot) -> Result<(usize, i64)> {
    let mut w = snapshot.court.state.clone();
    w.start(snapshot.court.utxo.block_daa_score)?;
    if w.ended() {
        return Err("match already ended".into());
    }
    let distance = if w.ball_vx == 1 { RIGHT_CONTACT_X - w.ball_x } else { w.ball_x - LEFT_CONTACT_X };
    let ticks = (distance + BALL_SPEED - 1) / BALL_SPEED;
    let phase = if w.clock >= w.serve_until { (w.clock - w.serve_until) % w.ball_tick_daa } else { 0 };
    let deadline = w
        .clock
        .max(w.serve_until)
        .checked_add(ticks.checked_mul(w.ball_tick_daa).ok_or("contact overflow")?)
        .and_then(|n| n.checked_sub(phase))
        .ok_or("contact overflow")?;
    Ok((if w.ball_vx == 1 { 1 } else { 0 }, deadline))
}

/// The opponent wins for an unanswered contact, without inventing a score.
/// Only accepted flight confirmation or consumed receiver history buys freshness.
pub fn timeout_result(snapshot: &Snapshot) -> Result<Finish> {
    let (receiver, deadline) = next_contact(snapshot)?;
    let w = &snapshot.court.state;
    Ok(Finish {
        stake_per_player: w.stake_per_player, left_score: w.left_score, right_score: w.right_score,
        winner: 1 - receiver as i64,
        end_daa: deadline.max(response_anchor(snapshot)?).checked_add(RESPONSE_TIMEOUT_DAA as i64).ok_or("timeout deadline overflow")?,
    })
}

fn response_anchor(snapshot: &Snapshot) -> Result<i64> {
    let born = accepted_score(snapshot.court.utxo.block_daa_score)?;
    let fresh = snapshot.court.state.response_at;
    if fresh < -1 || fresh > born { return Err("invalid accepted response anchor".into()); }
    Ok(if fresh == -1 { born } else { fresh })
}

/// A small checkpoint reads only one side's lanes. It cannot pass the next
/// ball contact, so compacted movements can never erase required history.
pub fn checkpoint_plan(snapshot: &Snapshot, side: usize, until: i64) -> Result<Plan> {
    if side > 1 {
        return Err("invalid paddle side".into());
    }
    let (receiver, deadline) = next_contact(snapshot)?;
    let mut world = snapshot.court.state.clone();
    world.start(snapshot.court.utxo.block_daa_score)?;
    world.response_at = response_anchor(snapshot)?;
    let mut queues = std::array::from_fn(|i| snapshot.players[i].state.clone());
    if until < queues[side].through || until > deadline {
        return Err("checkpoint outside safe contact interval".into());
    }
    let mut y = queues[side].y;
    for i in (side..QUEUE_COUNT).step_by(2) {
        queues[i].stamp(snapshot.players[i].utxo.block_daa_score)?;
        if queues[i].side != side as i64
            || queues[i].lane != i / 2
            || queues[i].owner != if side == 0 { world.left.clone() } else { world.right.clone() }
        {
            return Err("incorrect paddle lane identity".into());
        }
        if queues[i].count > 0 && queues[i].times[0] < queues[side].through {
            return Err("movement precedes certified paddle checkpoint".into());
        }
    }
    for _ in 0..MAX_EVENTS {
        let head = (side..QUEUE_COUNT).step_by(2).filter(|&i| queues[i].count > 0).map(|i| queues[i].times[0]).min().unwrap_or(until);
        if head >= until {
            break;
        }
        for i in (side..QUEUE_COUNT).step_by(2) {
            queues[i].apply(head, &mut y);
        }
    }
    let through =
        (side..QUEUE_COUNT).step_by(2).filter(|&i| queues[i].count > 0).map(|i| queues[i].times[0]).min().unwrap_or(until).min(until);
    queues[side].y = y;
    queues[side].through = through;
    let contact = side == receiver && until == deadline && through == deadline;
    let remaining: usize = (side..QUEUE_COUNT).step_by(2).map(|i| queues[i].count).sum();
    let previous: usize = (side..QUEUE_COUNT).step_by(2).map(|i| snapshot.players[i].state.count).sum();
    if !contact && through <= snapshot.players[side].state.through && remaining >= previous {
        return Err("checkpoint must advance contact, watermark or consume a batch".into());
    }
    // Only strictly pre-contact batches are consumed. Once due, at most twelve
    // remain, and post-contact commands cannot replenish this finite history.
    if side == receiver && remaining < previous { world.response_at = -1; }
    // Argent's leader validates the movement prefix and this contact atomically.
    if contact {
        if side == 0 {
            world.left_y = y;
        } else {
            world.right_y = y;
        }
        let mut none: [Queue; 0] = [];
        while world.clock < deadline {
            step(&mut world, &mut none, deadline)?;
        }
        world.resolves += 1;
        world.resolver = 1 - side as i64;
        // A delayed outgoing proof must not instantly forfeit the next receiver.
        world.response_at = -1;
    }
    Ok(Plan { world, queues })
}

/// Resolve the receiving side's prefix and contact in one coordinated spend.
pub fn resolve(snapshot: &Snapshot) -> Result<Plan> {
    let (side, deadline) = next_contact(snapshot)?;
    checkpoint_plan(snapshot, side, deadline)
}
pub fn resolve_sequence(snapshot: &Snapshot, _takeover: bool) -> Result<u64> {
    let (_, deadline) = next_contact(snapshot)?;
    Ok(deadline.saturating_sub(accepted_score(snapshot.court.utxo.block_daa_score)?).max(0) as u64)
}
pub fn resolve_as(snapshot: &Snapshot, side: i64) -> Result<Plan> {
    if side != next_contact(snapshot)?.0 as i64 {
        return Err("only the receiving player can prove contact".into());
    }
    resolve(snapshot)
}

/// Interpret the current game at a node's DAA score without publishing frames.
/// The result is reproducible from accepted UTXOs; later covenant resolutions
/// must agree with it. The node score is for viewing, never a transaction witness.
pub fn project(snapshot: &Snapshot, node_daa: u64) -> Result<Plan> {
    let horizon = accepted_score(node_daa)?;
    let mut world = snapshot.court.state.clone();
    let mut queues = std::array::from_fn(|i| snapshot.players[i].state.clone());
    world.start(snapshot.court.utxo.block_daa_score)?;
    if world.clock > horizon || queues[0].through > horizon || queues[1].through > horizon {
        return Err("view precedes a saved ball or paddle checkpoint".into());
    }
    world.left_y = queues[0].y;
    world.right_y = queues[1].y;
    for i in 0..QUEUE_COUNT {
        queues[i].stamp(snapshot.players[i].utxo.block_daa_score)?;
    }
    // The inactive paddle may still have commands older than the last ball
    // contact. They cannot change that authenticated contact; apply them to its
    // independent position before projecting subsequent flight.
    loop {
        let head = queues.iter().filter(|q| q.count > 0).map(|q| q.times[0]).min();
        let Some(head) = head.filter(|&t| t <= world.clock) else {
            break;
        };
        for q in &mut queues {
            q.apply(head, if q.side == 0 { &mut world.left_y } else { &mut world.right_y });
        }
    }
    loop {
        step_until(&mut world, &mut queues, horizon, true)?;
        if world.ended() || (world.clock == horizon && queues.iter().all(|q| q.count == 0 || q.times[0] > horizon)) {
            break;
        }
    }
    Ok(Plan { world, queues })
}

/// 28 center-line heights times ten shot directions. Deterministic variation,
/// with the same signed-magnitude decoding used by the covenant's OpBin2Num.
pub fn serve_choice(goal_daa: i64, total: i64, left: &[u8], right: &[u8]) -> i64 {
    let hash = blake2b_simd::Params::new()
        .hash_length(32)
        .to_state()
        .update(b"PongServe/v2")
        .update(left)
        .update(right)
        .update(&goal_daa.to_le_bytes())
        .update(&total.to_le_bytes())
        .finalize();
    let bytes = hash.as_bytes();
    let value = i64::from(bytes[0]) + (i64::from(bytes[1] & 0x7f) << 8);
    (if bytes[1] & 0x80 != 0 { -value } else { value }).rem_euclid(280)
}

/// Jump over free flight, preserving exactly the discrete tick rules.
pub fn step(w: &mut World, q: &mut [Queue], horizon: i64) -> Result<()> {
    step_until(w, q, horizon, false)
}

/// Only historical presentation may retain commands beyond its horizon.
/// Covenant construction keeps the stricter checkpoint interval validation.
fn step_until(w: &mut World, q: &mut [Queue], horizon: i64, historical: bool) -> Result<()> {
    if w.ended() {
        return Ok(());
    }
    if horizon < w.clock {
        return Err("chain time precedes checkpoint".into());
    }
    let mut target = horizon;
    for queue in q.iter() {
        if queue.count > 0 {
            if queue.times[0] < w.clock || (!historical && queue.times[0] > horizon) {
                return Err("input timestamp outside checkpoint interval".into());
            }
            target = target.min(queue.times[0]);
        }
    }
    let flying = w.clock >= w.serve_until;
    if !flying {
        target = target.min(w.serve_until);
    }
    let mut dt = target - w.clock;
    let travel_ticks =
        (if w.ball_vx == 1 { RIGHT_CONTACT_X - w.ball_x } else { w.ball_x - LEFT_CONTACT_X } + BALL_SPEED - 1) / BALL_SPEED;
    // Keep the fractional ball tick across commands and checkpoints. The serve
    // deadline is the phase anchor; only ball motion uses this slower clock.
    let flight_phase = if flying { (w.clock - w.serve_until) % w.ball_tick_daa } else { 0 };
    let distance = (travel_ticks * w.ball_tick_daa - flight_phase).max(0);
    if flying {
        dt = dt.min(distance);
    }
    if flying {
        let movement_ticks = (flight_phase + dt) / w.ball_tick_daa;
        w.ball_x = (w.ball_x + w.ball_vx * BALL_SPEED * movement_ticks).clamp(LEFT_CONTACT_X, RIGHT_CONTACT_X);
        if movement_ticks > 0 && w.ball_vy != 0 {
            let vertical_speed = w.ball_vy.abs();
            let phase = (w.ball_y + w.ball_vy * movement_ticks).rem_euclid(62);
            w.ball_y = if phase <= 31 { phase } else { 62 - phase };
            if phase > 31 {
                w.ball_vy = -w.ball_vy;
            }
            if phase == 0 {
                w.ball_vy = -vertical_speed;
            }
            if phase == 31 {
                w.ball_vy = vertical_speed;
            }
        }
        if dt == distance {
            let offset = w.ball_y - if w.ball_vx == 1 { w.right_y } else { w.left_y };
            if offset.abs() > PADDLE_RADIUS + BALL_RADIUS {
                if w.ball_vx == 1 {
                    w.left_score += 1;
                } else {
                    w.right_score += 1;
                }
                w.ball_x = CENTER_X;
                w.serve_until = w.clock + dt + SERVE_DELAY_DAA;
                w.choose_serve(w.clock + dt, w.left_score + w.right_score);
            } else {
                w.ball_vx = -w.ball_vx;
                w.ball_vy = offset.clamp(-2, 2);
            }
        }
    }
    w.clock += dt;
    for queue in q {
        queue.apply(w.clock, if queue.side == 0 { &mut w.left_y } else { &mut w.right_y });
    }
    Ok(())
}

pub fn pubkey(key: &Keypair) -> Vec<u8> {
    key.x_only_public_key().0.serialize().to_vec()
}
pub fn sign(tx: &MutableTransaction<Transaction>, input: usize, key: &Keypair) -> Vec<u8> {
    sign_with_type(tx, input, key, SIG_HASH_ALL)
}
pub fn sign_with_type(tx: &MutableTransaction<Transaction>, input: usize, key: &Keypair, hash_type: SigHashType) -> Vec<u8> {
    let hash = calc_schnorr_signature_hash(&tx.as_verifiable(), input, hash_type, &SigHashReusedValuesUnsync::new());
    let mut bytes = key.sign_schnorr(Message::from_digest_slice(hash.as_bytes().as_slice()).unwrap()).as_ref().to_vec();
    bytes.push(hash_type.to_u8());
    bytes
}
pub fn p2pk(key: &[u8]) -> ScriptPublicKey {
    ScriptPublicKey::from_vec(0, ScriptBuilder::new().add_data(key).unwrap().add_op(OpCheckSig).unwrap().drain())
}
pub fn funded<'a>(mut ctx: TxContext<'a>, funding: &'a Funding, key: &'a Keypair, fee: u64) -> Result<TxContext<'a>> {
    if funding.utxo.script_public_key != p2pk(&pubkey(key)) {
        return Err("fee key does not own the supplied P2PK UTXO".into());
    }
    let change = funding.utxo.amount.checked_sub(fee).ok_or("fee exceeds funding UTXO")?;
    ctx = ctx.input(
        funding.outpoint,
        funding.utxo.clone(),
        InputSigScript::with_transaction(move |tx, input| ScriptBuilder::new().add_data(&sign(tx, input, key)).unwrap().drain()),
        0,
    );
    if change > 0 {
        ctx = ctx.output(funding.utxo.script_public_key.clone(), None, change);
    }
    Ok(ctx)
}

/// Argent commits script execution budgets; Kaspa additionally requires the
/// contextual storage-mass commitment. Mass is not part of the signed tx ID.
pub fn build(builder: &TxBuilder<'_>, ctx: &TxContext<'_>) -> Result<Transaction> {
    let tx = builder.build(ctx)?;
    let entries = ctx
        .inputs
        .iter()
        .map(|input| match input {
            ContextInput::Actor(input) => input.utxo.clone(),
            ContextInput::Ordinary(input) => input.utxo.clone(),
        })
        .collect();
    commit_storage_mass(&tx, entries)?;
    Ok(tx)
}

/// Storage depends on output values/pluralities, not signatures. Use the same
/// consensus calculation for ordinary builders and the portable joint start.
pub fn storage_mass(tx: &Transaction, entries: Vec<UtxoEntry>) -> Result<u64> {
    if tx.inputs.len() != entries.len() {
        return Err("storage mass calculation lacks source entries".into());
    }
    MassCalculator::new_with_consensus_params(&MAINNET_PARAMS)
        .calc_contextual_masses(&PopulatedTransaction::new(tx, entries))
        .map(|mass| mass.storage_mass)
        .ok_or_else(|| "storage mass overflow".into())
}

pub fn commit_storage_mass(tx: &Transaction, entries: Vec<UtxoEntry>) -> Result<()> {
    let mass = storage_mass(tx, entries)?;
    let limit = MAINNET_PARAMS.block_mass_limits().after().storage;
    if mass > limit {
        return Err(
            format!("storage mass {mass} exceeds limit {limit}; choose different funding to avoid a small change output").into()
        );
    }
    tx.set_storage_mass(mass);
    Ok(())
}

pub fn command_tx(
    builder: &TxBuilder<'_>,
    player: &Cell<Queue>,
    direction: i64,
    owner: &Keypair,
    funding: &Funding,
    fee_key: &Keypair,
    fee: u64,
) -> Result<(Transaction, Queue)> {
    let next = player.state.push(direction, player.utxo.block_daa_score)?;
    let id = player.utxo.covenant_id.ok_or("player has no covenant ID")?;
    let ctx = TxContext::new()
        .actor_input(
            player.state.actor(),
            player.state.state(),
            EntryCall::new("command").args_with(|tx, input| args![direction, sign(tx, input, owner)]),
            player.outpoint,
            player.utxo.clone(),
            0,
        )
        .actor_output(player.state.actor(), next.state(), CovenantBinding::new(0, id), player.utxo.amount);
    Ok((build(builder, &funded(ctx, funding, fee_key, fee)?)?, next))
}

pub fn checkpoint_context<'a>(snapshot: &Snapshot, side: usize, until: i64, plan: &Plan, owner: &Keypair) -> Result<TxContext<'a>> {
    if side > 1 {
        return Err("invalid paddle side".into());
    }
    let owner = *owner;
    let id = snapshot.court.utxo.covenant_id.ok_or("court has no covenant ID")?;
    let age = until.saturating_sub(accepted_score(snapshot.court.utxo.block_daa_score)?).max(0) as u64;
    if age > u32::MAX as u64 {
        return Err("resolve relative lock exceeds consensus range".into());
    }
    let mut ctx = TxContext::new().actor_input(
        "Court",
        snapshot.court.state.state(),
        EntryCall::new("resolve").args_with(move |tx, input| args![side as i64, until, sign(tx, input, &owner)]),
        snapshot.court.outpoint,
        snapshot.court.utxo.clone(),
        age,
    );
    for i in [side, side + 2, side + 4] {
        let q = &snapshot.players[i];
        ctx = ctx.actor_input("Player", q.state.state(), "resolve", q.outpoint, q.utxo.clone(), 0);
    }
    ctx = ctx.actor_output("Court", plan.world.state(), CovenantBinding::new(0, id), snapshot.court.utxo.amount);
    for i in [side, side + 2, side + 4] {
        ctx = ctx.actor_output("Player", plan.queues[i].state(), CovenantBinding::new(0, id), snapshot.players[i].utxo.amount);
    }
    Ok(ctx)
}
pub fn checkpoint_tx(
    builder: &TxBuilder<'_>,
    snapshot: &Snapshot,
    side: usize,
    until: i64,
    owner: &Keypair,
    funding: &Funding,
    key: &Keypair,
    fee: u64,
) -> Result<(Transaction, Plan)> {
    let plan = checkpoint_plan(snapshot, side, until)?;
    let ctx = checkpoint_context(snapshot, side, until, &plan, owner)?;
    Ok((build(builder, &funded(ctx, funding, key, fee)?)?, plan))
}
pub fn resolve_context<'a>(snapshot: &Snapshot, plan: &Plan, owner: &Keypair) -> Result<TxContext<'a>> {
    let (side, deadline) = next_contact(snapshot)?;
    checkpoint_context(snapshot, side, deadline, plan, owner)
}
pub fn resolve_tx(
    builder: &TxBuilder<'_>,
    snapshot: &Snapshot,
    resolver_key: &Keypair,
    funding: &Funding,
    key: &Keypair,
    fee: u64,
    _takeover: bool,
) -> Result<(Transaction, Plan)> {
    if _takeover {
        return Err("opponent contact takeover is disabled; claim the match timeout instead".into());
    }
    if player_side(&snapshot.court.state, resolver_key)? != next_contact(snapshot)?.0 as i64 {
        return Err("only the receiving player can prove contact".into());
    }
    let plan = resolve(snapshot)?;
    let ctx = resolve_context(snapshot, &plan, resolver_key)?;
    Ok((build(builder, &funded(ctx, funding, key, fee)?)?, plan))
}
/// Authorize a timeout through the same Collector gate as normal settlement.
/// A valid late response races this spend; sequence locks have no upper bound.
pub fn timeout_context<'a>(snapshot: &Snapshot, claimant: &Keypair) -> Result<TxContext<'a>> {
    let finish = timeout_result(snapshot)?;
    let claimant = *claimant;
    let age = finish.end_daa.saturating_sub(accepted_score(snapshot.court.utxo.block_daa_score)?).max(0) as u64;
    if age > u32::MAX as u64 {
        return Err("timeout relative lock exceeds consensus range".into());
    }
    let mut ctx = collection_context(
        snapshot,
        EntryCall::new("timeout").args_with(move |tx, i| args![sign(tx, i, &claimant)]),
        finish,
    )?;
    if let ContextInput::Actor(input) = &mut ctx.inputs[0] {
        input.sequence = age;
    }
    Ok(ctx)
}
pub fn timeout_tx(
    builder: &TxBuilder<'_>, snapshot: &Snapshot, claimant: &Keypair,
    funding: &Funding, fee_key: &Keypair, fee: u64,
) -> Result<(Transaction, Finish)> {
    let finish = timeout_result(snapshot)?;
    let ctx = timeout_context(snapshot, claimant)?;
    Ok((build(builder, &funded(ctx, funding, fee_key, fee)?)?, finish))
}
/// Authorize final collection without carrying its six-lane verification code.
pub fn settle_tx(builder: &TxBuilder<'_>, snapshot: &Snapshot, funding: &Funding, key: &Keypair, fee: u64) -> Result<Transaction> {
    if !snapshot.court.state.ended() {
        return Err("match has not ended".into());
    }
    let ctx = collection_context(snapshot, EntryCall::new("settle"), snapshot.court.state.result())?;
    build(builder, &funded(ctx, funding, key, fee)?)
}
fn collection_context<'a>(snapshot: &Snapshot, entry: EntryCall<'a>, finish: Finish) -> Result<TxContext<'a>> {
    let id = snapshot.court.utxo.covenant_id.ok_or("missing covenant")?;
    let ctx = TxContext::new().actor_input(
        "Court",
        snapshot.court.state.state(),
        entry,
        snapshot.court.outpoint,
        snapshot.court.utxo.clone(),
        0,
    );
    Ok(ctx.actor_output(
        "Collector",
        Collection { left: snapshot.court.state.left.clone(), right: snapshot.court.state.right.clone(), finish }.state(),
        CovenantBinding::new(0, id),
        snapshot.court.utxo.amount,
    ))
}
/// Replace only the outputs actually spent by a checkpoint/contact.
pub fn accepted_transition(b: &TxBuilder<'_>, snapshot: &Snapshot, tx: &Transaction, plan: Plan, daa: u64) -> Result<Snapshot> {
    let mut next = snapshot.clone();
    next.court = accepted_cell(tx, 0, daa, plan.world);
    for i in 0..QUEUE_COUNT {
        if !tx.inputs.iter().any(|input| input.previous_outpoint == snapshot.players[i].outpoint) {
            continue;
        }
        let q = &plan.queues[i];
        let script = b
            .covenant_utxo(q.actor(), q.state(), snapshot.players[i].utxo.amount, daa, false, snapshot.players[i].utxo.covenant_id)?
            .script_public_key;
        let index = tx.outputs.iter().position(|o| o.script_public_key == script).ok_or("missing paddle successor")?;
        next.players[i] = accepted_cell(tx, index, daa, q.clone());
    }
    Ok(next)
}

pub fn player_side(world: &World, key: &Keypair) -> Result<i64> {
    let pk = pubkey(key);
    if pk == world.left {
        Ok(0)
    } else if pk == world.right {
        Ok(1)
    } else {
        Err("resolver key is not a player".into())
    }
}

pub fn resolve_call(world: &World, key: &Keypair, _takeover: bool) -> Result<EntryCall<'static>> {
    player_side(world, key)?;
    Ok(EntryCall::new("resolve"))
}

/// Use ONLY after the node has accepted this output; never stamp with a desired
/// locktime or a local wall clock. Tests use explicit simulated acceptance.
pub fn accepted_cell<T>(tx: &Transaction, index: usize, daa: u64, state: T) -> Cell<T> {
    let output = &tx.outputs[index];
    Cell {
        outpoint: TransactionOutpoint::new(tx.id(), index as u32),
        utxo: UtxoEntry::new(output.value, output.script_public_key.clone(), daa, false, output.covenant.map(|c| c.covenant_id)),
        state,
    }
}
pub fn accepted_plan(tx: &Transaction, plan: Plan, daa: u64) -> Result<Snapshot> {
    if plan.world.ended() {
        return Err("opening plan cannot already be terminal".into());
    }
    Ok(Snapshot {
        court: accepted_cell(tx, 0, daa, plan.world),
        players: std::array::from_fn(|i| accepted_cell(tx, i + 1, daa, plan.queues[i].clone())),
    })
}

pub fn genesis_tx(
    builder: &TxBuilder<'_>,
    lobby: &Lobby,
    funding: &Funding,
    key: &Keypair,
    fee: u64,
) -> Result<Transaction> {
    if lobby.control_value != SIDE_DEPOSIT as i64 || !valid_ball_tick_daa(lobby.ball_tick_daa) {
        return Err("invalid queue allocation or ball pace".into());
    }
    let stake = stake_amount(lobby.stake_per_player)?;
    let deposit = stake;
    let ctx = TxContext::new().actor_genesis_output(0, "launch::pong", "Lobby", lobby.state(), deposit);
    Ok(build(builder, &funded(ctx, funding, key, deposit.checked_add(fee).ok_or("fee overflow")?)?)?)
}

pub fn start_tx(
    builder: &TxBuilder<'_>,
    lobby: &Cell<Lobby>,
    right_key: &Keypair,
    funding: &Funding,
    fee_key: &Keypair,
    fee: u64,
) -> Result<(Transaction, Plan)> {
    let plan = lobby.state.opening();
    let id = lobby.utxo.covenant_id.ok_or("lobby has no covenant ID")?;
    let control = u64::try_from(lobby.state.control_value)?;
    if control == 0 || control % QUEUES_PER_PLAYER as u64 != 0 {
        return Err("player deposit must split evenly across three queues".into());
    }
    let stake = stake_amount(lobby.state.stake_per_player)?;
    if lobby.utxo.amount != stake { return Err("Lobby must contain only the joining player's stake".into()); }
    let court = stake.checked_mul(2).and_then(|v| v.checked_sub(2 * control)).filter(|v| *v > 0).ok_or("insufficient pot for queue allocation")?;
    let mut ctx = TxContext::new()
        .actor_input(
            "Lobby",
            lobby.state.state(),
            EntryCall::new("start").args_with(|tx, i| args![sign(tx, i, right_key)]),
            lobby.outpoint,
            lobby.utxo.clone(),
            0,
        )
        .actor_output("Court", plan.world.state(), CovenantBinding::new(0, id), court);
    for q in &plan.queues {
        ctx = ctx.actor_output(q.actor(), q.state(), CovenantBinding::new(0, id), control / QUEUES_PER_PLAYER as u64);
    }
    Ok((build(builder, &funded(ctx, funding, fee_key, stake.checked_add(fee).ok_or("stake and fee overflow")?)?)?, plan))
}

pub fn reclaim_tx(
    builder: &TxBuilder<'_>,
    collecting: &Collecting,
    funding: &Funding,
    fee_key: &Keypair,
    fee: u64,
) -> Result<Transaction> {
    let c = &collecting.collector;
    let mut ctx = TxContext::new().actor_input("Collector", c.state.state(), "collect", c.outpoint, c.utxo.clone(), 0);
    let mut total = c.utxo.amount;
    for q in &collecting.players {
        total = total.checked_add(q.utxo.amount).ok_or("deposit overflow")?;
        ctx = ctx.actor_input("Player", q.state.state(), "collect", q.outpoint, q.utxo.clone(), 0);
    }
    let stake = stake_amount(c.state.finish.stake_per_player)?;
    let pot = stake.checked_mul(2).ok_or("pot overflow")?;
    if total != pot { return Err("game outputs must hold exactly the two-player pot".into()); }
    match c.state.finish.winner {
        0 | 1 => {
            let destination = if c.state.finish.winner == 0 { &c.state.left } else { &c.state.right };
            ctx = ctx.output(p2pk(destination), None, pot);
        }
        _ => return Err("invalid winner".into()),
    }
    Ok(build(builder, &funded(ctx, funding, fee_key, fee)?)?)
}
