//! Headless transaction construction and deterministic interpretation of Pong.
//! The .ag covenant, not this client, decides whether a successor is valid.
use argent_runtime::{ArtifactValue, ContextInput, TxBuilder, TxContext, state};
use kaspa_consensus_core::{
    config::params::MAINNET_PARAMS,
    hashing::sighash::{SigHashReusedValuesUnsync, calc_schnorr_signature_hash},
    hashing::sighash_type::{SIG_HASH_ALL, SigHashType},
    mass::{ContextualMasses, Mass, MassCalculator},
    tx::{MutableTransaction, PopulatedTransaction, ScriptPublicKey, Transaction, TransactionOutpoint, UtxoEntry},
};
use kaspa_txscript::{opcodes::codes::OpCheckSig, script_builder::ScriptBuilder};
use secp256k1::{Keypair, Message};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub type State = BTreeMap<String, ArtifactValue>;
/// Fixed-width Script numbers use signed magnitude, matching OpNum2Bin/OpBin2Num.
fn packed_int(value: i64) -> [u8; 8] {
    let mut bytes = value.unsigned_abs().to_le_bytes();
    if value < 0 {
        bytes[7] |= 0x80;
    }
    bytes
}
pub const MAX_EVENTS: usize = 4;
pub const QUEUES_PER_PLAYER: usize = 3;
pub const QUEUE_COUNT: usize = 2 * QUEUES_PER_PLAYER;
/// Default client wager; each game commits its own stake in the covenant.
pub const DEFAULT_STAKE: i64 = 500_000_000;
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
    SuperFast,
    Fast,
    #[default]
    Medium,
    Slow,
    Manual(u8),
}
impl GameSpeed {
    pub const fn tick_daa(self) -> i64 {
        match self {
            Self::SuperFast => 5,
            Self::Fast => 6,
            Self::Medium => 7,
            Self::Slow => 8,
            Self::Manual(ticks) => ticks as i64,
        }
    }
    pub const fn code(self) -> u8 {
        self.tick_daa() as u8
    }
    pub fn from_code(code: u8) -> Result<Self> {
        Self::from_tick_daa(i64::from(code)).ok_or_else(|| "invalid game speed in code".into())
    }
    pub const fn from_tick_daa(ticks: i64) -> Option<Self> {
        match ticks {
            5 => Some(Self::SuperFast),
            6 => Some(Self::Fast),
            7 => Some(Self::Medium),
            8 => Some(Self::Slow),
            1..=15 => Some(Self::Manual(ticks as u8)),
            _ => None,
        }
    }
    pub fn label(self) -> String {
        match self {
            Self::SuperFast => "Super Fast".into(),
            Self::Fast => "Fast".into(),
            Self::Medium => "Medium".into(),
            Self::Slow => "Slow".into(),
            Self::Manual(ticks) => format!("Manual ({ticks} DAA/tick)"),
        }
    }
}
pub const DEFAULT_BALL_TICK_DAA: i64 = GameSpeed::Medium.tick_daa();
pub const MAX_BALL_TICK_DAA: i64 = 15;
pub fn valid_ball_tick_daa(ticks: i64) -> bool {
    (1..=MAX_BALL_TICK_DAA).contains(&ticks)
}
pub const SERVE_DELAY_DAA: i64 = 20;
pub const OPENING_DELAY_DAA: i64 = 25;
pub const DAA_PER_SECOND: i64 = 10;
pub const RESOLVE_DAA: u64 = 2;
/// Fifteen nominal seconds at ten DAA/second; consensus does not promise wall time.

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lobby {
    pub left: Vec<u8>,
    pub right: Vec<u8>,
    pub refund: Vec<u8>,
    pub control_value: i64,
    pub ball_tick_daa: i64,
    pub stake_per_player: i64,
    pub ball_count: i64,
    pub win_score: i64,
}
impl Lobby {
    pub fn state(&self) -> State {
        state! { left: self.left.clone(), right: self.right.clone(), refund: self.refund.clone(), control_value: self.control_value, ball_tick_daa: self.ball_tick_daa, stake_per_player: self.stake_per_player, ball_count: self.ball_count, win_score: self.win_score }
    }
    pub fn opening(&self) -> Plan {
        let mut world = World::opening(self.left.clone(), self.right.clone());
        world.ball_tick_daa = self.ball_tick_daa;
        world.stake_per_player = self.stake_per_player;
        world.ball_count = self.ball_count;
        world.win_score = self.win_score;
        Plan { world, queues: empty_queues(&self.left, &self.right) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ball {
    pub x: i64,
    pub y: i64,
    pub vx: i64,
    pub vy: i64,
    pub serve_until: i64,
}
impl Default for Ball {
    fn default() -> Self {
        Self { x: CENTER_X, y: 16, vx: 1, vy: 0, serve_until: 0 }
    }
}
pub const MAX_BALLS: usize = 8;
pub fn valid_ball_count(count: i64) -> bool {
    (1..=MAX_BALLS as i64).contains(&count)
}
pub fn valid_win_score(score: i64) -> bool {
    (1..=15).contains(&score)
}
pub fn default_win_score(count: i64) -> i64 {
    match count {
        2 => 5,
        3 => 6,
        4 => 7,
        5 => 8,
        6 => 9,
        7 => 10,
        8 => 11,
        _ => 3,
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
    pub extra_balls: [Ball; MAX_BALLS - 1],
    pub ball_count: i64,
    pub win_score: i64,
    pub launch_epoch: i64,
    pub left_score: i64,
    pub right_score: i64,
    pub resolver: i64,
    pub resolves: i64,
    pub takeovers: i64,
    /// -1 binds a new flight/progress anchor to this Court's actual acceptance.
    pub response_at: i64,
}

impl World {
    pub fn ball_count(&self) -> usize {
        assert!(valid_ball_count(self.ball_count), "invalid ball count");
        self.ball_count as usize
    }
    pub fn ball(&self, index: usize) -> Ball {
        match index {
            0 => Ball { x: self.ball_x, y: self.ball_y, vx: self.ball_vx, vy: self.ball_vy, serve_until: self.serve_until },
            1..MAX_BALLS => self.extra_balls[index - 1],
            _ => panic!("invalid ball index"),
        }
    }
    fn set_ball(&mut self, index: usize, ball: Ball) {
        if index == 0 {
            self.ball_x = ball.x;
            self.ball_y = ball.y;
            self.ball_vx = ball.vx;
            self.ball_vy = ball.vy;
            self.serve_until = ball.serve_until;
        } else {
            self.extra_balls[index - 1] = ball;
        }
    }
    pub fn with_ball(&self, index: usize) -> Self {
        let mut world = self.clone();
        world.set_ball(0, self.ball(index));
        world
    }
    pub fn court_period(&self) -> i64 {
        27 * self.ball_tick_daa
    }
    pub fn launch_phase(&self, index: usize) -> i64 {
        self.launch_epoch + (index as i64 * self.court_period() + self.ball_count - 1) / self.ball_count
    }
    fn choose_serve(&self, index: usize, serve: i64) -> Ball {
        let choice = serve_choice(serve, index as i64, &self.left, &self.right);
        Ball {
            x: CENTER_X,
            y: 2 + choice / 10,
            vx: if choice % 2 == 0 { -1 } else { 1 },
            vy: (choice / 2) % 5 - 2,
            serve_until: serve,
        }
    }
    pub(crate) fn start(&mut self, born: u64) -> Result<()> {
        if !valid_ball_tick_daa(self.ball_tick_daa) || !valid_ball_count(self.ball_count) || !valid_win_score(self.win_score) {
            return Err("invalid ball speed, count or winning score".into());
        }
        if !self.started {
            self.clock = accepted_score(born)?;
            self.launch_epoch = self.clock.checked_add(OPENING_DELAY_DAA).ok_or("opening deadline overflow")?;
            for index in 0..self.ball_count() {
                self.set_ball(index, self.choose_serve(index, self.launch_phase(index)));
            }
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
            extra_balls: [Ball::default(); MAX_BALLS - 1],
            ball_count: 1,
            win_score: WIN_SCORE,
            launch_epoch: 0,
            left_score: 0,
            right_score: 0,
            resolver: 0,
            resolves: 0,
            takeovers: 0,
            response_at: -1,
        }
    }
    pub fn state(&self) -> State {
        let balls = (0..MAX_BALLS)
            .map(|i| self.ball(i))
            .flat_map(|b| [b.x, b.y, b.vx, b.vy, b.serve_until])
            .flat_map(packed_int)
            .collect::<Vec<_>>();
        let progress = [self.left_y, self.right_y, self.left_score, self.right_score, self.resolver, self.resolves, self.takeovers]
            .into_iter()
            .flat_map(packed_int)
            .collect::<Vec<_>>();
        state! { left: self.left.clone(), right: self.right.clone(), stake_per_player: self.stake_per_player,
        started: self.started, clock: self.clock, ball_tick_daa: self.ball_tick_daa,
        ball_count: self.ball_count, win_score: self.win_score, launch_epoch: self.launch_epoch,
        balls: balls,
        progress: progress, response_at: self.response_at }
    }
    pub fn ended(&self) -> bool {
        self.left_score >= self.win_score || self.right_score >= self.win_score
    }
    pub fn result(&self) -> Finish {
        Finish {
            stake_per_player: self.stake_per_player,
            win_score: self.win_score,
            left_score: self.left_score,
            right_score: self.right_score,
            winner: if self.left_score >= self.win_score { 0 } else { 1 },
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
    pub fn state(&self) -> State {
        let checkpoint = [self.count as i64, self.y, self.through].into_iter().flat_map(packed_int).collect::<Vec<_>>();
        state! { owner: self.owner.clone(), side: self.side, lane: self.lane as i64, checkpoint: checkpoint,
        t0: self.times[0], t1: self.times[1], t2: self.times[2], t3: self.times[3],
        d0: self.commands[0], d1: self.commands[1], d2: self.commands[2], d3: self.commands[3] }
    }
    /// Explicit histories need no acceptance metadata; only the newest -1 does.
    pub fn stamp(&mut self, accepted: u64) -> Result<()> {
        if self.count > 4 {
            return Err("invalid queue length".into());
        }
        if self.count > 0 && self.times[self.count - 1] == -1 {
            self.times[self.count - 1] = accepted_score(accepted)?;
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
    pub win_score: i64,
    pub left_score: i64,
    pub right_score: i64,
    pub winner: i64,
    pub end_daa: i64,
}
impl Finish {
    pub fn state(&self) -> State {
        state! { stake_per_player: self.stake_per_player, win_score: self.win_score, left_score: self.left_score, right_score: self.right_score,
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
    let (_, side, deadline) = next_ball_contact(snapshot)?;
    Ok((side, deadline))
}
pub fn next_ball_contact(snapshot: &Snapshot) -> Result<(usize, usize, i64)> {
    let mut w = snapshot.court.state.clone();
    w.start(snapshot.court.utxo.block_daa_score)?;
    if w.ended() {
        return Err("match already ended".into());
    }
    next_world_contact(&w)
}
pub(crate) fn ball_contact(w: &World, ball: Ball) -> Result<i64> {
    let distance = if ball.vx == 1 { RIGHT_CONTACT_X - ball.x } else { ball.x - LEFT_CONTACT_X };
    let ticks = (distance + BALL_SPEED - 1) / BALL_SPEED;
    let phase = if w.clock >= ball.serve_until { (w.clock - ball.serve_until) % w.ball_tick_daa } else { 0 };
    w.clock
        .max(ball.serve_until)
        .checked_add(ticks.checked_mul(w.ball_tick_daa).ok_or("contact overflow")?)
        .and_then(|n| n.checked_sub(phase))
        .ok_or_else(|| "contact overflow".into())
}
pub(crate) fn next_world_contact(w: &World) -> Result<(usize, usize, i64)> {
    let mut next = (0, if w.ball_vx == 1 { 1 } else { 0 }, ball_contact(w, w.ball(0))?);
    for index in 1..w.ball_count() {
        let ball = w.ball(index);
        let deadline = ball_contact(w, ball)?;
        if deadline < next.2 {
            next = (index, if ball.vx == 1 { 1 } else { 0 }, deadline);
        }
    }
    Ok(next)
}

/// The opponent wins for an unanswered contact, without inventing a score.
pub(crate) fn timeout_result_after(snapshot: &Snapshot, response_timeout_daa: u64) -> Result<Finish> {
    let (receiver, deadline) = next_contact(snapshot)?;
    let w = &snapshot.court.state;
    Ok(Finish {
        stake_per_player: w.stake_per_player,
        win_score: w.win_score,
        left_score: w.left_score,
        right_score: w.right_score,
        winner: 1 - receiver as i64,
        end_daa: response_expiry(snapshot, deadline, response_timeout_daa)?,
    })
}

pub(crate) fn response_expiry(snapshot: &Snapshot, deadline: i64, response_timeout_daa: u64) -> Result<i64> {
    deadline.max(response_anchor(snapshot)?).checked_add(response_timeout_daa as i64).ok_or_else(|| "timeout deadline overflow".into())
}

fn response_anchor(snapshot: &Snapshot) -> Result<i64> {
    let born = accepted_score(snapshot.court.utxo.block_daa_score)?;
    let fresh = snapshot.court.state.response_at;
    if fresh < -1 || fresh > born {
        return Err("invalid accepted response anchor".into());
    }
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
        // Court.resolve still requires accepted lane inputs, even without placeholders.
        let accepted = snapshot.players[i].utxo.block_daa_score;
        accepted_score(accepted)?;
        queues[i].stamp(accepted)?;
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
    if side == receiver && remaining < previous {
        world.response_at = -1;
    }
    // Argent's leader validates the movement prefix and this contact atomically.
    if contact {
        if side == 0 {
            world.left_y = y;
        } else {
            world.right_y = y;
        }
        let mut none: [Queue; 0] = [];
        // One canonical contact per coordinated spend, including a tied event.
        step(&mut world, &mut none, deadline)?;
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
    for (queue, player) in queues.iter_mut().zip(&snapshot.players) {
        let accepted = player.utxo.block_daa_score;
        accepted_score(accepted)?;
        queue.stamp(accepted)?;
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
        .update(b"PongServe/v3")
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
    let (index, receiver, deadline) = next_world_contact(w)?;
    target = target.min(deadline);
    // All slots share one clock. Advance their free flights, but authenticate
    // only the earliest contact; commands at that DAA still follow contact.
    for i in 0..w.ball_count() {
        let mut ball = w.ball(i);
        let ticks = (target - ball.serve_until).max(0) / w.ball_tick_daa - (w.clock - ball.serve_until).max(0) / w.ball_tick_daa;
        ball = advance_ball(ball, ticks);
        w.set_ball(i, ball);
    }
    w.clock = target;
    if target == deadline {
        let mut ball = w.ball(index);
        let offset = ball.y - if receiver == 1 { w.right_y } else { w.left_y };
        if offset.abs() > PADDLE_RADIUS + BALL_RADIUS {
            if receiver == 1 {
                w.left_score += 1;
            } else {
                w.right_score += 1;
            }
            let minimum = deadline.checked_add(SERVE_DELAY_DAA).ok_or("serve deadline overflow")?;
            let serve = if w.ball_count == 1 {
                minimum
            } else {
                let phase = w.launch_phase(index);
                let periods = (minimum - phase + w.court_period() - 1).max(0) / w.court_period();
                phase.checked_add(periods * w.court_period()).ok_or("serve deadline overflow")?
            };
            ball = w.choose_serve(index, serve);
        } else {
            ball.vx = -ball.vx;
            ball.vy = offset.clamp(-2, 2);
        }
        w.set_ball(index, ball);
    }
    for queue in q {
        queue.apply(w.clock, if queue.side == 0 { &mut w.left_y } else { &mut w.right_y });
    }
    Ok(())
}

fn advance_ball(mut ball: Ball, ticks: i64) -> Ball {
    ball.x = (ball.x + ball.vx * BALL_SPEED * ticks).clamp(LEFT_CONTACT_X, RIGHT_CONTACT_X);
    if ticks > 0 && ball.vy != 0 {
        let speed = ball.vy.abs();
        let phase = (ball.y + ball.vy * ticks).rem_euclid(62);
        ball.y = if phase <= 31 { phase } else { 62 - phase };
        if phase > 31 {
            ball.vy = -ball.vy;
        }
        if phase == 0 {
            ball.vy = -speed;
        }
        if phase == 31 {
            ball.vy = speed;
        }
    }
    ball
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

#[derive(Clone, Copy, Debug, Serialize)]
pub struct TransactionMass {
    pub compute: u64,
    pub transient: u64,
    pub storage: u64,
    pub relay: u64,
    pub priority: u64,
}
/// Call after the contextual storage commitment and execution budgets are set.
pub fn transaction_mass(tx: &Transaction) -> Result<TransactionMass> {
    let params = &MAINNET_PARAMS;
    let limits = params.block_mass_limits;
    let non_contextual = MassCalculator::new_with_consensus_params(params).calc_non_contextual_masses(tx);
    let storage = tx.storage_mass();
    if non_contextual.compute_mass > limits.compute
        || non_contextual.transient_mass > limits.transient
        || storage > limits.storage
        || tx.gas > params.block_lane_limits.gas_per_lane
    {
        return Err(format!(
            "transaction exceeds a block resource limit: compute {}, transient {}, storage {}",
            non_contextual.compute_mass, non_contextual.transient_mass, storage
        )
        .into());
    }
    let cofactors = params.block_mass_cofactors();
    Ok(TransactionMass {
        compute: non_contextual.compute_mass,
        transient: non_contextual.transient_mass,
        storage,
        relay: non_contextual.normalized_max(&cofactors),
        priority: Mass::new(non_contextual, ContextualMasses::new(storage)).normalized_max(&cofactors),
    })
}

pub fn commit_storage_mass(tx: &Transaction, entries: Vec<UtxoEntry>) -> Result<()> {
    let mass = storage_mass(tx, entries)?;
    let limit = MAINNET_PARAMS.block_mass_limits.storage;
    if mass > limit {
        return Err(
            format!("storage mass {mass} exceeds limit {limit}; choose different funding to avoid a small change output").into()
        );
    }
    tx.set_storage_mass(mass);
    Ok(())
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
