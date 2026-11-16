//! Reusable queue protocol. Kept separate from v26 so saved games remain valid.
//! Queue identities survive games; only their lease, generation and outpoints change.
use crate::engine::*;
use argent_runtime::{ArgValue, Artifact, EntryCall, InputSigScript, TxBuilder, TxContext, args, state, state_with};
use kaspa_consensus_core::{
    Hash,
    tx::{CovenantBinding, Transaction},
};
use kaspa_txscript::script_builder::ScriptBuilder;
use secp256k1::Keypair;
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 36;
pub const RESPONSE_TIMEOUT_DAA: u64 = 110;
pub const MIN_RESERVE: u64 = 200_000_000;
pub const DEFAULT_QUEUE_RESERVE: u64 = 500_000_000;
const MAX_CANCEL_LANE_FEE: u64 = 20_000_000;
/// Historical floor for whole-KAS wagers, matching fee_reserve_floor in the
/// covenant. These thresholds bound a worst-loss reset at 150k storage mass
/// without input credit.
pub fn recommended_reserve(wager: u64) -> Result<u64> {
    match wager {
        500_000_000..=899_999_999 => Ok(300_000_000),
        900_000_000..=2_899_999_999 => Ok(275_000_000),
        2_900_000_000..=25_400_000_000 => Ok(250_000_000),
        _ => Err("wager requires 5–254 KAS".into()),
    }
}
pub fn required_capital(set: &Set, wager: u64) -> Result<u64> {
    let minimum = recommended_reserve(wager)?;
    set.lanes.iter().try_fold(wager, |total, cell| {
        total.checked_add(protected_lane_reserve(cell, minimum)).ok_or_else(|| "queue capital overflow".into())
    })
}
pub const ARTIFACT: &str = include_str!("../artifacts/reusable/artifact.json");
pub fn previous_artifact()->Result<Artifact>{Ok(serde_json::from_str(include_str!("../artifacts/reusable-v27/artifact.json"))?)}
pub fn artifact() -> Result<Artifact> {
    Ok(serde_json::from_str(ARTIFACT)?)
}
pub fn v28_artifact() -> Result<Artifact> {
    Ok(serde_json::from_str(include_str!("../artifacts/reusable-v28/artifact.json"))?)
}
pub fn v29_artifact() -> Result<Artifact> {
    Ok(serde_json::from_str(include_str!("../artifacts/reusable-v29/artifact.json"))?)
}
pub fn v30_artifact() -> Result<Artifact> {
    Ok(serde_json::from_str(include_str!("../artifacts/reusable-v30/artifact.json"))?)
}
pub fn v31_artifact() -> Result<Artifact> {
    Ok(serde_json::from_str(include_str!("../artifacts/reusable-v31/artifact.json"))?)
}
pub fn v32_artifact() -> Result<Artifact> {
    Ok(serde_json::from_str(include_str!("../artifacts/reusable-v32/artifact.json"))?)
}
pub fn v33_artifact() -> Result<Artifact> {
    Ok(serde_json::from_str(include_str!("../artifacts/reusable-v33/artifact.json"))?)
}
pub fn v34_artifact() -> Result<Artifact> {
    Ok(serde_json::from_str(include_str!("../artifacts/reusable-v34/artifact.json"))?)
}
pub fn v35_artifact() -> Result<Artifact> {
    Ok(serde_json::from_str(include_str!("../artifacts/reusable-v35/artifact.json"))?)
}

pub fn response_timeout(snapshot: &Snapshot) -> Result<Finish> {
    timeout_result_after(snapshot, RESPONSE_TIMEOUT_DAA)
}

pub fn contact_expiry(snapshot: &Snapshot, deadline: i64) -> Result<i64> {
    response_expiry(snapshot, deadline, RESPONSE_TIMEOUT_DAA)
}
fn packed(value: i64) -> [u8; 8] {
    let mut bytes = value.unsigned_abs().to_le_bytes();
    if value < 0 {
        bytes[7] |= 128;
    }
    bytes
}
pub fn part(total: u64, lane: usize) -> u64 {
    total / 3 + u64::from((lane as u64) < total % 3)
}
fn id_state(result: &mut State, ids: [Hash; 2], generations: [i64; 2], court: bool) {
    result.insert("left_pool".into(), argent_runtime::ArtifactValue::Bytes(ids[0].as_bytes().to_vec()));
    result.insert("right_pool".into(), argent_runtime::ArtifactValue::Bytes(ids[1].as_bytes().to_vec()));
    let _ = court;
    result.insert("generations".into(), argent_runtime::ArtifactValue::Bytes(generations.into_iter().flat_map(packed).collect()));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lane {
    pub queue: Queue,
    pub game: Hash,
    pub generation: i64,
    pub reserve: u64,
    pub exposure: u64,
    pub active: bool,
}
impl Lane {
    pub fn actor(&self) -> &'static str {
        if self.queue.lane == 0 && !self.active { "Bank" } else { "Lane" }
    }
    pub fn state(&self) -> State {
        let q = &self.queue;
        state! { game: self.game.as_bytes().to_vec(), data: self.state_data(), owner: q.owner.clone(), role: q.side*3+q.lane as i64 }
    }
    fn state_data(&self) -> Vec<u8> {
        let q = &self.queue;
        [self.generation, self.reserve as i64, self.exposure as i64, i64::from(self.active), q.count as i64, q.y, q.through]
            .into_iter()
            .chain(q.times)
            .chain(q.commands)
            .flat_map(packed)
            .collect()
    }
    pub(crate) fn reset(&self, game: Hash, generation: i64, side: usize, exposure: u64, active: bool) -> Self {
        let mut queue = Queue::empty(self.queue.owner.clone(), side as i64);
        queue.lane = self.queue.lane;
        Self { queue, game, generation, reserve: self.reserve, exposure, active }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Set {
    pub lanes: [Cell<Lane>; 3],
}
impl Set {
    pub fn id(&self) -> Result<Hash> {
        self.lanes[0].utxo.covenant_id.ok_or_else(|| "queue set has no identity".into())
    }
    pub fn generation(&self) -> i64 {
        self.lanes[0].state.generation
    }
    pub fn balance(&self) -> u64 {
        self.lanes.iter().map(|c| c.utxo.amount).sum()
    }
    pub fn reserve(&self) -> u64 {
        self.lanes.iter().map(|c| c.state.reserve).sum()
    }
    pub fn idle(&self) -> bool {
        self.lanes.iter().all(|c| !c.state.active)
    }
    pub fn validate(&self, b: &TxBuilder<'_>) -> Result<()> {
        let id = self.id()?;
        let first = &self.lanes[0].state;
        let reserve = self.lanes.iter().try_fold(0u64, |sum, c| sum.checked_add(c.state.reserve).ok_or("queue reserve overflow"))?;
        let _balance = self.lanes.iter().try_fold(0u64, |sum, c| sum.checked_add(c.utxo.amount).ok_or("queue balance overflow"))?;
        if first.generation < 0 || first.generation == i64::MAX || reserve < MIN_RESERVE {
            return Err("invalid queue generation or reserve".into());
        }
        for (i, c) in self.lanes.iter().enumerate() {
            let s = &c.state;
            if c.utxo.amount > i64::MAX as u64
                || s.reserve > i64::MAX as u64
                || s.exposure > i64::MAX as u64
                || c.utxo.covenant_id != Some(id)
                || s.queue.lane != i
                || s.queue.owner != first.queue.owner
                || s.generation != first.generation
                || s.game != first.game
                || s.active != first.active
                || c.utxo.amount < s.reserve.checked_add(s.exposure).ok_or("queue exposure overflow")?
                || self.lanes[..i].iter().any(|previous| previous.outpoint == c.outpoint)
            {
                return Err("inconsistent or underfunded queue set".into());
            }
            // v27/v28 used Player for the active lane zero. Older idle sets
            // still use Bank and Lane and can be closed with their own artifact.
            let actor = if s.active && i == 0 && b.actor_type_handle("Player", "PlayerState").is_ok() {
                "Player"
            } else {
                s.actor()
            };
            let expected = b.covenant_utxo(actor, s.state(), c.utxo.amount, c.utxo.block_daa_score, false, Some(id))?;
            if expected.script_public_key != c.utxo.script_public_key {
                return Err("queue state does not match its script".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Offer {
    pub lobby: Lobby,
    pub pools: [Hash; 2],
    pub generations: [i64; 2],
}
impl Offer {
    pub fn state(&self) -> State {
        let mut s = self.lobby.state();
        for field in ["refund", "control_value", "ball_tick_daa", "stake_per_player", "ball_count", "win_score"] {
            s.remove(field);
        }
        s.insert(
            "terms".into(),
            argent_runtime::ArtifactValue::Bytes(
                [self.lobby.ball_tick_daa, self.lobby.stake_per_player, self.lobby.ball_count, self.lobby.win_score]
                    .into_iter()
                    .flat_map(packed)
                    .collect(),
            ),
        );
        id_state(&mut s, self.pools, self.generations, false);
        s
    }
    pub fn opening(&self) -> Game {
        Game {
            world: self.lobby.opening().world,
            pools: self.pools,
            generations: self.generations,
            flights: [None; MAX_BALLS],
            pending_contact: None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Flight {
    deadline: i64,
    origin: i64,
    ball: Ball,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Game {
    pub world: World,
    pub pools: [Hash; 2],
    pub generations: [i64; 2],
    // Old journals may still be read to import idle sets. The new template is
    // used only for new games, whose untouched opening slots start all zero.
    #[serde(default)]
    flights: [Option<Flight>; MAX_BALLS],
    #[serde(default)]
    pending_contact: Option<(usize, usize, i64)>,
}
impl Game {
    pub(crate) fn result_state(&self, finish: Finish) -> ResultState {
        ResultState {
            collection: Collection { left: self.world.left.clone(), right: self.world.right.clone(), finish },
            pools: self.pools,
            generations: self.generations,
        }
    }

    pub fn state(&self) -> State {
        let mut s = self.world.state();
        for field in
            ["stake_per_player", "started", "clock", "ball_tick_daa", "ball_count", "win_score", "launch_epoch", "response_at", "balls"]
        {
            s.remove(field);
        }
        let flights = self.flights.iter().flat_map(|flight| {
            flight.map_or([0; 7], |f| [f.deadline, f.origin, f.ball.x, f.ball.y, f.ball.vx, f.ball.vy, f.ball.serve_until])
        }).flat_map(packed).collect();
        s.insert("flights".into(), argent_runtime::ArtifactValue::Bytes(flights));
        let contact = self.pending_contact.map_or([0; 3], |(index, side, deadline)| [index as i64, side as i64, deadline]);
        let w = &self.world;
        s.insert(
            "header".into(),
            argent_runtime::ArtifactValue::Bytes(
                [
                    w.stake_per_player,
                    i64::from(w.started),
                    w.clock,
                    w.ball_tick_daa,
                    w.ball_count,
                    w.win_score,
                    w.launch_epoch,
                    w.response_at,
                ]
                .into_iter()
                .chain(contact)
                .flat_map(packed)
                .collect(),
            ),
        );
        id_state(&mut s, self.pools, self.generations, true);
        s
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResultState {
    pub collection: Collection,
    pub pools: [Hash; 2],
    pub generations: [i64; 2],
}
impl ResultState {
    pub fn state(&self) -> State {
        let mut s = self.collection.state();
        id_state(&mut s, self.pools, self.generations, false);
        s
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Match {
    pub court: Cell<Game>,
    pub sets: [Set; 2],
}
impl Match {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            court: Cell { outpoint: self.court.outpoint, utxo: self.court.utxo.clone(), state: self.court.state.world.clone() },
            players: std::array::from_fn(|i| {
                let c = &self.sets[i % 2].lanes[i / 2];
                Cell { outpoint: c.outpoint, utxo: c.utxo.clone(), state: c.state.queue.clone() }
            }),
        }
    }

    pub(crate) fn checkpoint_state(&self, side: usize, until: i64) -> Result<(Game, [Lane; 3])> {
        let snapshot = self.snapshot();
        let plan = checkpoint_plan(&snapshot, side, until)?;
        let mut game = self.court.state.clone();
        if plan.world.resolves != game.world.resolves {
            let (index, _, _) = next_ball_contact(&snapshot)?;
            let ball = plan.world.ball(index);
            game.flights[index] = Some(Flight {
                deadline: ball_contact(&plan.world, ball)?,
                origin: plan.world.clock,
                ball,
            });
        }
        game.pending_contact = Some(crate::engine::next_world_contact(&plan.world)?);
        game.world = plan.world;
        let states = std::array::from_fn(|i| {
            let mut lane = self.sets[side].lanes[i].state.clone();
            lane.queue = plan.queues[side + i * 2].clone();
            lane
        });
        Ok((game, states))
    }
}

/// Direct multi-input funding: no preliminary consolidation transaction.
/// The caller must select mature, accepted, unreserved ordinary coins.
pub fn fund<'a>(mut ctx: TxContext<'a>, coins: &[Funding], key: &Keypair, spend: u64) -> Result<TxContext<'a>> {
    let key = *key;
    let script = p2pk(&pubkey(&key));
    let mut total = 0u64;
    for (i, coin) in coins.iter().enumerate() {
        if coin.utxo.covenant_id.is_some()
            || coin.utxo.script_public_key != script
            || coins[..i].iter().any(|c| c.outpoint == coin.outpoint)
            || ctx.inputs.iter().any(|c| match c {
                argent_runtime::ContextInput::Actor(c) => c.outpoint == coin.outpoint,
                argent_runtime::ContextInput::Ordinary(c) => c.outpoint == coin.outpoint,
            })
        {
            return Err("funding must use distinct ordinary coins owned by this wallet".into());
        }
        total = total.checked_add(coin.utxo.amount).ok_or("funding overflow")?;
        ctx = ctx.input(
            coin.outpoint,
            coin.utxo.clone(),
            InputSigScript::with_transaction(move |tx, i| ScriptBuilder::new().add_data(&sign(tx, i, &key)).unwrap().drain()),
            0,
        );
    }
    let change = total.checked_sub(spend).ok_or("not enough ordinary funding for this operation")?;
    if change > 0 {
        ctx = ctx.output(script, None, change);
    }
    Ok(ctx)
}
fn checked_build(b: &TxBuilder<'_>, ctx: &TxContext<'_>) -> Result<Transaction> {
    let tx = build(b, ctx)?;
    transaction_mass(&tx)?;
    Ok(tx)
}
fn wallet_funds(coins: &[Funding]) -> u64 {
    coins.iter().fold(0u64, |total, coin| total.saturating_add(coin.utxo.amount))
}
fn protected_lane_reserve(cell: &Cell<Lane>, minimum: u64) -> u64 {
    cell.state.reserve.max(part(minimum, cell.state.queue.lane))
}
fn lane_fee_capacity(cell: &Cell<Lane>, minimum: u64) -> u64 {
    cell.utxo.amount.saturating_sub(protected_lane_reserve(cell, minimum).saturating_add(cell.state.exposure))
}
fn fund_lanes<const N: usize>(
    lanes: [&Cell<Lane>; N], coins: &[Funding], fee: u64, minimum: u64, target: u64, first_fee_lane: usize,
) -> Result<([u64; N], u64)> {
    let wallet = wallet_funds(coins);
    let mut unpaid = fee.saturating_sub(wallet);
    let mut payments = [0; N];
    for offset in 0..N {
        if unpaid == 0 { break; }
        let index = (first_fee_lane + offset) % N;
        let cell = lanes[index];
        let payment = unpaid.min(lane_fee_capacity(cell, minimum));
        payments[index] = payment;
        unpaid -= payment;
    }
    if unpaid != 0 {
        return Err("reserve and wallet funding cannot cover this network fee".into());
    }
    let mut available = wallet.saturating_sub(fee);
    let amounts = std::array::from_fn(|index| {
        let cell = lanes[index];
        let target_balance = part(target, cell.state.queue.lane).saturating_add(cell.state.exposure);
        let refill = available.min(target_balance.saturating_sub(cell.utxo.amount));
        available -= refill;
        cell.utxo.amount - payments[index] + refill
    });
    Ok((amounts, wallet - available))
}
pub fn create(
    b: &TxBuilder<'_>,
    owner: &Keypair,
    capital: u64,
    reserve: u64,
    coins: &[Funding],
    fee: u64,
) -> Result<(Transaction, [Lane; 3])> {
    create_with_nonce(b, owner, capital, reserve, coins, fee, Hash::from_bytes([0; 32]))
}
pub fn create_with_nonce(
    b: &TxBuilder<'_>,
    owner: &Keypair,
    capital: u64,
    reserve: u64,
    coins: &[Funding],
    fee: u64,
    nonce: Hash,
) -> Result<(Transaction, [Lane; 3])> {
    if reserve < MIN_RESERVE || capital < reserve || capital > i64::MAX as u64 {
        return Err("invalid queue capital or reserve".into());
    }
    let lanes = std::array::from_fn(|i| {
        let mut queue = Queue::empty(pubkey(owner), 0);
        queue.lane = i;
        Lane { queue, game: nonce, generation: 0, reserve: part(reserve, i), exposure: 0, active: false }
    });
    let mut ctx = TxContext::new();
    for (i, lane) in lanes.iter().enumerate() {
        ctx = ctx.actor_genesis_output(0, "launch::queues", lane.actor(), lane.state(), part(capital, i));
    }
    let ctx = fund(ctx, coins, owner, capital.checked_add(fee).ok_or("creation fee overflow")?)?;
    Ok((checked_build(b, &ctx)?, lanes))
}
pub fn accepted_set(tx: &Transaction, indexes: [usize; 3], states: [Lane; 3], daa: u64) -> Set {
    Set { lanes: std::array::from_fn(|i| accepted_cell(tx, indexes[i], daa, states[i].clone())) }
}
fn require_idle(b: &TxBuilder<'_>, set: &Set, owner: &Keypair) -> Result<()> {
    set.validate(b)?;
    if !set.idle() || set.lanes[0].state.queue.owner != pubkey(owner) {
        return Err("queue set is busy or belongs to another wallet".into());
    }
    Ok(())
}
fn idle_inputs<'a>(mut ctx: TxContext<'a>, set: &Set, owner: &Keypair, primary: EntryCall<'a>) -> TxContext<'a> {
    let key = *owner;
    for (i, c) in set.lanes.iter().enumerate() {
        let call = if i == 0 { continue } else { EntryCall::new("supply").args_with(move |tx, i| args![sign(tx, i, &key)]) };
        ctx = ctx.actor_input(c.state.actor(), c.state.state(), call, c.outpoint, c.utxo.clone(), 0);
    }
    // Put the primary first: observed groups and covenant bindings use lane order.
    let c = &set.lanes[0];
    let mut result = TxContext::new().actor_input("Bank", c.state.state(), primary, c.outpoint, c.utxo.clone(), 0);
    result.inputs.extend(ctx.inputs);
    result.outputs.extend(ctx.outputs);
    result
}
fn active_inputs<'a>(mut ctx: TxContext<'a>, set: &Set, next: &[Lane; 3]) -> Result<(TxContext<'a>, u16)> {
    let first = u16::try_from(ctx.inputs.len())?;
    for (cell, successor) in set.lanes.iter().zip(next) {
        let entry = if successor.actor() == "Bank" { "reset_primary" } else { "coordinate" };
        let call = EntryCall::new(entry).args(args![successor.state_data()]);
        ctx = ctx.actor_input("Lane", cell.state.state(), call, cell.outpoint, cell.utxo.clone(), 0);
    }
    Ok((ctx, first))
}

// Idle transitions bind the group to Bank. Active transitions bind each
// replacement to its matching lane, with the game checking the whole group.
fn set_outputs<'a>(mut ctx: TxContext<'a>, set: &Set, states: &[Lane; 3], amounts: [u64; 3], first: u16) -> Result<TxContext<'a>> {
    for i in 0..3 {
        let authorizer = if set.idle() { first } else { first.checked_add(i as u16).ok_or("queue input index overflow")? };
        ctx = ctx.actor_output(states[i].actor(), states[i].state(), CovenantBinding::new(authorizer, set.id()?), amounts[i]);
    }
    Ok(ctx)
}

/// Reserve the guest's set and spawn an invitation atomically. `capital` may
/// increase using ordinary inputs; wager principal is never spent on fees.
fn lane_amounts(set: &Set, capital: u64, wager: u64) -> Result<[u64; 3]> {
    let minimum = recommended_reserve(wager)?;
    let extra = capital.checked_sub(required_capital(set, wager)?).ok_or("queue set needs a wager top-up")?;
    Ok(std::array::from_fn(|lane| {
        protected_lane_reserve(&set.lanes[lane], minimum) + part(wager - wager / 2, lane) + part(extra, lane)
    }))
}
pub fn reserve(
    b: &TxBuilder<'_>,
    set: &Set,
    offer: &Offer,
    owner: &Keypair,
    capital: u64,
    coins: &[Funding],
    fee: u64,
) -> Result<Transaction> {
    reserve_with_payload(b, set, offer, owner, capital, coins, fee, vec![])
}
pub fn reserve_with_payload(
    b: &TxBuilder<'_>,
    set: &Set,
    offer: &Offer,
    owner: &Keypair,
    capital: u64,
    coins: &[Funding],
    fee: u64,
    payload: Vec<u8>,
) -> Result<Transaction> {
    require_idle(b, set, owner)?;
    if offer.pools[0] != set.id()? || offer.generations[0] != set.generation() || offer.lobby.left != pubkey(owner) {
        return Err("offer does not identify this guest queue set".into());
    }
    let wager = u64::try_from(offer.lobby.stake_per_player)?;
    if capital < required_capital(set, wager)? || capital < set.balance() {
        return Err("queue set needs a wager top-up".into());
    }
    let key = *owner;
    let offer_state = offer.state();
    let primary = EntryCall::new("reserve_game").args_with(move |tx, i| {
        vec![sign(tx, i, &key).into(), ArgValue::Value(argent_runtime::ArtifactValue::Object(offer_state.clone()))]
    });
    let mut ctx = idle_inputs(TxContext::new(), set, owner, primary).actor_genesis_output(
        0,
        "spawn::match_game",
        "Lobby",
        offer.state(),
        wager / 2,
    );
    for (i, amount) in lane_amounts(set, capital, wager)?.into_iter().enumerate() {
        let lane = set.lanes[i].state.clone();
        let generation = set.generation();
        ctx = ctx.actor_output(
            "Lane",
            state_with(move |context| {
                let game = context.genesis_covenant_id(0, "spawn::match_game").expect("declared spawn group");
                lane.reset(game, generation, 0, part(wager - wager / 2, i), true).state()
            }),
            CovenantBinding::new(0, set.id()?),
            amount,
        );
    }
    checked_build(b, &fund(ctx.payload(payload), coins, owner, capital - set.balance() + fee)?)
}
pub fn reserved(tx: &Transaction, set: &Set, offer: Offer, capital: u64, daa: u64) -> (Cell<Offer>, Set) {
    let game = tx.outputs[0].covenant.unwrap().covenant_id;
    let wager = offer.lobby.stake_per_player as u64;
    let states = std::array::from_fn(|i| set.lanes[i].state.reset(game, set.generation(), 0, part(wager - wager / 2, i), true));
    debug_assert_eq!(tx.outputs[1..4].iter().map(|o| o.value).sum::<u64>(), capital - wager / 2);
    (accepted_cell(tx, 0, daa, offer), accepted_set(tx, [1, 2, 3], states, daa))
}
fn start_context<'a>(
    b: &TxBuilder<'_>,
    lobby: &Cell<Offer>,
    guest: &Set,
    host: &Set,
    owner: &Keypair,
    capital: u64,
    coins: &[Funding],
    fee: u64,
) -> Result<(TxContext<'a>, Game, [[Lane; 3]; 2])> {
    require_idle(b, host, owner)?;
    guest.validate(b)?;
    let offer = &lobby.state;
    let game = lobby.utxo.covenant_id.ok_or("missing match identity")?;
    if offer.pools[0]!=guest.id()? || offer.generations[0]!=guest.generation() || guest.id()?==host.id()?
        || (offer.pools[1].as_bytes().as_slice()!=offer.lobby.right.as_slice() && (offer.pools[1]!=host.id()? || offer.generations[1]!=host.generation()))
        || offer.lobby.right != pubkey(owner)
    {
        return Err("host queue offer changed".into());
    }
    let wager = u64::try_from(offer.lobby.stake_per_player)?;
    if capital < required_capital(host, wager)? || capital < host.balance() {
        return Err("host queue set needs a wager top-up".into());
    }
    if guest.lanes.iter().enumerate().any(|(i, c)| {
        !c.state.active || c.state.game != game || c.state.queue.side != 0 || c.state.exposure != part(wager - wager / 2, i)
    }) {
        return Err("guest queues are not reserved for this exact invitation".into());
    }
    let states = [
        std::array::from_fn(|i| guest.lanes[i].state.clone()),
        std::array::from_fn(|i| host.lanes[i].state.reset(game, host.generation(), 1, part(wager - wager / 2, i), true)),
    ];
    let key = *owner;
    let host_pool=host.id()?;let host_generation=host.generation();
    let ctx = TxContext::new().actor_input(
        "Lobby",
        offer.state(),
        EntryCall::new("start").args_with(move |tx, i| args![sign(tx, i, &key),host_pool.as_bytes().to_vec(),host_generation]),
        lobby.outpoint,
        lobby.utxo.clone(),
        0,
    );
    let mut ctx = ctx;
    let h_primary = u16::try_from(ctx.inputs.len())?;
    for (i, c) in host.lanes.iter().enumerate() {
        let call = if i == 0 {
            EntryCall::new("accept")
                .args_with(move |tx, i| args![sign(tx, i, &key), game.as_bytes().to_vec(), 1i64, (wager - wager / 2) as i64])
        } else {
            EntryCall::new("supply").args_with(move |tx, i| args![sign(tx, i, &key)])
        };
        ctx = ctx.actor_input(c.state.actor(), c.state.state(), call, c.outpoint, c.utxo.clone(), 0);
    }
    let mut world = offer.opening();world.pools[1]=host_pool;world.generations[1]=host_generation;
    ctx=ctx.payload(start_payload(host)?);
    ctx = ctx.actor_output("Court", world.state(), CovenantBinding::new(0, game), 2 * (wager / 2));
    ctx = set_outputs(ctx, host, &states[1], lane_amounts(host, capital, wager)?, h_primary)?;
    let spend = capital - host.balance() + fee;
    Ok((fund(ctx, coins, owner, spend)?, world, states))
}
pub const START_MAGIC:&[u8]=b"QSTARTB";
fn start_payload(host:&Set)->Result<Vec<u8>> {
    let first=&host.lanes[0].state;let mut bytes=START_MAGIC.to_vec();bytes.extend(&first.queue.owner);
    bytes.extend(host.id()?.as_bytes());bytes.extend(first.game.as_bytes());bytes.extend(first.generation.to_le_bytes());
    for lane in &host.lanes{bytes.extend(lane.state.reserve.to_le_bytes());}bytes.push(u8::try_from(first.queue.side)?);Ok(bytes)
}
pub fn start(
    b: &TxBuilder<'_>,
    lobby: &Cell<Offer>,
    guest: &Set,
    host: &Set,
    owner: &Keypair,
    capital: u64,
    coins: &[Funding],
    fee: u64,
) -> Result<(Transaction, Game, [[Lane; 3]; 2])> {
    let (ctx, game, states) = start_context(b, lobby, guest, host, owner, capital, coins, fee)?;
    Ok((checked_build(b, &ctx)?, game, states))
}
pub fn accepted_match(tx: &Transaction, game: Game, states: [[Lane; 3]; 2], guest: &Set, daa: u64) -> Match {
    Match { court: accepted_cell(tx, 0, daa, game), sets: [guest.clone(), accepted_set(tx, [1, 2, 3], states[1].clone(), daa)] }
}
pub fn command(
    b: &TxBuilder<'_>,
    cell: &Cell<Lane>,
    direction: i64,
    owner: &Keypair,
    coins: &[Funding],
    fee: u64,
) -> Result<(Transaction, Lane)> {
    command_with_reserve_target(b, cell, direction, owner, coins, fee, 0)
}
pub fn command_with_reserve_target(
    b: &TxBuilder<'_>,
    cell: &Cell<Lane>,
    direction: i64,
    owner: &Keypair,
    coins: &[Funding],
    fee: u64,
    reserve_target: u64,
) -> Result<(Transaction, Lane)> {
    if !cell.state.active {
        return Err("queue is idle".into());
    }
    let mut next = cell.state.clone();
    next.queue = next.queue.push(direction, cell.utxo.block_daa_score)?;
    // The exposure is a rounded third of half a whole-KAS wager.
    let wager = (cell.state.exposure * 6 + 50_000_000) / 100_000_000 * 100_000_000;
    let ([amount], spend) = fund_lanes([cell], coins, fee, recommended_reserve(wager)?, reserve_target, 0)?;
    let key = *owner;
    let ctx = TxContext::new()
        .actor_input(
            cell.state.actor(),
            cell.state.state(),
            EntryCall::new("command").args_with(move |tx, i| args![direction, sign(tx, i, &key)]),
            cell.outpoint,
            cell.utxo.clone(),
            0,
        )
        .actor_output(
            next.actor(),
            next.state(),
            CovenantBinding::new(0, cell.utxo.covenant_id.ok_or("missing pool")?),
            amount,
        );
    Ok((checked_build(b, &fund(ctx, coins, owner, spend)?)?, next))
}
pub fn checkpoint(
    b: &TxBuilder<'_>,
    current: &Match,
    side: usize,
    until: i64,
    owner: &Keypair,
    coins: &[Funding],
    fee: u64,
) -> Result<(Transaction, Game, [Lane; 3])> {
    checkpoint_with_reserve_target(b, current, side, until, owner, coins, fee, 0)
}
pub fn checkpoint_with_reserve_target(
    b: &TxBuilder<'_>,
    current: &Match,
    side: usize,
    until: i64,
    owner: &Keypair,
    coins: &[Funding],
    fee: u64,
    reserve_target: u64,
) -> Result<(Transaction, Game, [Lane; 3])> {
    let (game, states) = current.checkpoint_state(side, until)?;
    let minimum = recommended_reserve(u64::try_from(game.world.stake_per_player)?)?;
    // An accepted contact advances the rotation. Repricing the same Court
    // checkpoint uses the same order and preserves the covenant output order.
    let first_fee_lane = current.court.state.world.resolves.rem_euclid(QUEUES_PER_PLAYER as i64) as usize;
    let (amounts, spend) = fund_lanes(current.sets[side].lanes.each_ref(), coins, fee, minimum, reserve_target, first_fee_lane)?;
    let key = *owner;
    let pool = current.sets[side].id()?;
    let age = until.saturating_sub(i64::try_from(current.court.utxo.block_daa_score)?).max(0) as u64;
    let ctx = TxContext::new().actor_input(
        "Court",
        current.court.state.state(),
        EntryCall::new("resolve").args_with(move |tx, i| args![side as i64, until, sign(tx, i, &key), pool.as_bytes().to_vec()]),
        current.court.outpoint,
        current.court.utxo.clone(),
        age,
    );
    let (ctx, primary) = active_inputs(ctx, &current.sets[side], &states)?;
    let (ctx, actor, state) = if game.world.ended() {
        static COLLECTOR_PAYLOAD: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        let result = game.result_state(game.world.result());
        (ctx.payload(template_payload("Collector", &COLLECTOR_PAYLOAD)?), "Collector", result.state())
    } else {
        (ctx, "Court", game.state())
    };
    let ctx = ctx.actor_output(
        actor,
        state,
        CovenantBinding::new(0, current.court.utxo.covenant_id.ok_or("missing match")?),
        current.court.utxo.amount,
    );
    let ctx = set_outputs(ctx, &current.sets[side], &states, amounts, primary)?;
    Ok((checked_build(b, &fund(ctx, coins, owner, spend)?)?, game, states))
}
pub fn timeout(
    b: &TxBuilder<'_>,
    current: &Match,
    owner: &Keypair,
    coins: &[Funding],
    fee: u64,
) -> Result<(Transaction, ResultState)> {
    let finish = response_timeout(&current.snapshot())?;
    let age = finish.end_daa.saturating_sub(i64::try_from(current.court.utxo.block_daa_score)?).max(0) as u64;
    let game = &current.court.state;
    let result = game.result_state(finish);
    let ctx = TxContext::new()
        .actor_input("Court", game.state(), "timeout", current.court.outpoint, current.court.utxo.clone(), age)
        .actor_output(
            "Collector",
            result.state(),
            CovenantBinding::new(0, current.court.utxo.covenant_id.ok_or("missing match")?),
            current.court.utxo.amount,
        );
    Ok((checked_build(b, &fund(ctx, coins, owner, fee)?)?, result))
}
fn template_payload(actor: &str, cache: &std::sync::OnceLock<Vec<u8>>) -> Result<Vec<u8>> {
    if let Some(payload) = cache.get() {
        return Ok(payload.clone());
    }
    let a = artifact()?;
    let contract = a.sil_abi.contract(actor).ok_or("covenant template missing")?;
    let (prefix, _, suffix) = contract.compiled.script_parts(&contract.compiled.bytecode).ok_or("invalid covenant template spans")?;
    let mut payload = Vec::with_capacity(8 + prefix.len() + suffix.len());
    payload.extend_from_slice(&i64::try_from(prefix.len())?.to_le_bytes());
    payload.extend_from_slice(prefix);
    payload.extend_from_slice(suffix);
    Ok(cache.get_or_init(|| payload).clone())
}
fn bank_template_payload() -> Result<Vec<u8>> {
    static BANK_PAYLOAD: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    template_payload("Bank", &BANK_PAYLOAD)
}

pub fn collect(
    b: &TxBuilder<'_>,
    result: &Cell<ResultState>,
    sets: &[Set; 2],
    payer: &Keypair,
    coins: &[Funding],
    fee: u64,
) -> Result<(Transaction, [[Lane; 3]; 2])> {
    let r = &result.state;
    let game = result.utxo.covenant_id.ok_or("missing match")?;
    let wager = u64::try_from(r.collection.finish.stake_per_player)?;
    let winner = usize::try_from(r.collection.finish.winner)?;
    if winner > 1 {
        return Err("invalid winner".into());
    }
    let mut balances = [sets[0].balance(), sets[1].balance()];
    balances[winner] = balances[winner]
        .checked_add(result.utxo.amount)
        .and_then(|v| v.checked_add(wager - wager / 2))
        .ok_or("settlement overflow")?;
    balances[1 - winner] = balances[1 - winner].checked_sub(wager - wager / 2).ok_or("underfunded wager exposure")?;
    let states = std::array::from_fn(|side| {
        std::array::from_fn(|i| sets[side].lanes[i].state.reset(game, sets[side].generation() + 1, side, 0, false))
    });
    let mut ctx = TxContext::new().payload(bank_template_payload()?)
        .actor_input("Collector", r.state(), "collect", result.outpoint, result.utxo.clone(), 0);
    for side in 0..2 {
        let (next, primary) = active_inputs(ctx, &sets[side], &states[side])?;
        ctx = set_outputs(next, &sets[side], &states[side], std::array::from_fn(|i| part(balances[side], i)), primary)?;
    }
    Ok((checked_build(b, &fund(ctx, coins, payer, fee)?)?, states))
}
pub fn cancel(
    b: &TxBuilder<'_>,
    lobby: &Cell<Offer>,
    set: &Set,
    owner: &Keypair,
    coins: &[Funding],
    fee: u64,
) -> Result<(Transaction, [Lane; 3])> {
    let game = lobby.utxo.covenant_id.ok_or("missing match")?;
    let key = *owner;
    let states = std::array::from_fn(|i| set.lanes[i].state.reset(game, set.generation() + 1, 0, 0, false));
    let ctx = TxContext::new().payload(bank_template_payload()?).actor_input(
        "Lobby",
        lobby.state.state(),
        EntryCall::new("cancel").args_with(move |tx, i| args![sign(tx, i, &key)]),
        lobby.outpoint,
        lobby.utxo.clone(),
        0,
    );
    let (ctx, primary) = active_inputs(ctx, set, &states)?;
    let lane_fee = fee.saturating_sub(wallet_funds(coins));
    let minimum = recommended_reserve(u64::try_from(lobby.state.lobby.stake_per_player)?)?;
    let capacity = set.lanes.iter().map(|cell| lane_fee_capacity(cell, minimum)).sum::<u64>().min(MAX_CANCEL_LANE_FEE);
    if lane_fee > capacity {
        return Err("reserve and wallet funding cannot cover cancellation".into());
    }
    let balance = set.balance().checked_add(lobby.utxo.amount).ok_or("refund overflow")? - lane_fee;
    let ctx = set_outputs(ctx, set, &states, std::array::from_fn(|i| part(balance, i)), primary)?;
    Ok((checked_build(b, &fund(ctx, coins, owner, fee - lane_fee)?)?, states))
}
/// Owner-approved change to idle capital, including top-up or withdrawal.
pub fn resize(b: &TxBuilder<'_>, set: &Set, owner: &Keypair, capital: u64, coins: &[Funding], fee: u64) -> Result<Transaction> {
    require_idle(b, set, owner)?;
    if capital < set.reserve() {
        return Err("close the set to withdraw its protected reserve".into());
    }
    let key = *owner;
    let mut ctx = idle_inputs(TxContext::new(), set, owner, EntryCall::new("resize").args_with(move |tx, i| args![sign(tx, i, &key)]));
    let states = std::array::from_fn(|i| set.lanes[i].state.clone());
    ctx = set_outputs(ctx, set, &states, std::array::from_fn(|i| part(capital, i)), 0)?;
    if capital < set.balance() {
        let withdrawal = set.balance() - capital;
        if coins.is_empty() {
            let returned = withdrawal.checked_sub(fee).filter(|v| *v > 0).ok_or("withdrawal cannot cover its network fee")?;
            ctx = ctx.output(p2pk(&pubkey(owner)), None, returned);
            return checked_build(b, &ctx);
        }
        ctx = ctx.output(p2pk(&pubkey(owner)), None, withdrawal);
    }
    checked_build(b, &fund(ctx, coins, owner, capital.saturating_sub(set.balance()) + fee)?)
}
/// Close only an idle set. Optional ordinary inputs combine withdrawal and consolidation.
pub fn close(b: &TxBuilder<'_>, set: &Set, owner: &Keypair, coins: &[Funding], fee: u64) -> Result<Transaction> {
    require_idle(b, set, owner)?;
    let key = *owner;
    let ctx = idle_inputs(
        TxContext::new(),
        set,
        owner,
        EntryCall::new("close").args_with(move |tx, i| args![sign(tx, i, &key), fee as i64]),
    );
    let total = coins.iter().try_fold(set.balance(), |v, c| v.checked_add(c.utxo.amount).ok_or("withdrawal overflow"))?;
    // Fund first, then combine its change with the queue withdrawal into output zero.
    let mut ctx = fund(ctx, coins, owner, 0)?;
    ctx.outputs.clear();
    ctx = ctx.output(p2pk(&pubkey(owner)), None, total.checked_sub(fee).ok_or("fee exceeds withdrawal")?);
    checked_build(b, &ctx)
}

#[cfg(test)]
#[path = "reusable/tests.rs"]
mod tests;
