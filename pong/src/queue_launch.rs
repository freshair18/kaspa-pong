//! Interactive reusable-queue games. The wallet service owns chain replay and
//! durable submissions; this module owns only menu choices and live intent.
use super::{keys, menu, network_ui, node};
use kaspa_consensus_core::{Hash, tx::UtxoEntry};
use kaspa_pong::{
    engine::*,
    reusable::{self, Set},
};
use node::{
    Config,
    queue_book::{Effect, Session},
    queue_codes::{Descriptor, HostCode, Reservation},
    queue_service::{GameAction, Quote, Service, View},
    queue_tracker::QueueStage,
};
use std::{
    collections::VecDeque,
    io::IsTerminal,
    path::Path,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

mod consolidation;

fn interactive(json: bool) -> bool {
    !json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}
fn service(c: &Config, wallet: Option<&Path>, connection: &node::rpc::startup::Connection) -> Result<Service> {
    let (player, _) = keys(wallet)?;
    Service::open(c, player, connection.clone())
}
fn cells(set: &Set) -> Vec<(kaspa_consensus_core::tx::TransactionOutpoint, UtxoEntry)> {
    set.lanes.iter().map(|c| (c.outpoint, c.utxo.clone())).collect()
}
fn fee_details<T>(service: &Service, quote: &Quote<T>) -> Result<String> {
    let mass = transaction_mass(&quote.transaction)?;
    let target = (u128::from(mass.priority) * u128::from(node::fee_policy::selected_rate(service.config())) + 999) / 1000;
    Ok(format!(
        "Network fee: {} KAS{}",
        menu::format_kas(quote.fee),
        if target > u128::from(quote.fee) { "\nFee ceiling reached; the requested network priority is capped." } else { "" }
    ))
}
async fn review(title: &str, body: String, automatic: bool) -> Result<bool> {
    let title = title.to_string();
    let seconds = automatic.then(network_ui::auto_accept_seconds);
    Ok(tokio::task::spawn_blocking(move || menu::operation_review(&title, &body, seconds)).await??)
}
async fn ready(service: &Service, enabled: bool, label: &str) -> Result<Option<View>> {
    let ui = network_ui::Setup::queue_check(enabled)?;
    let started = Instant::now();
    let Some(result) = ui.read(async {
        loop {
            let view = service.snapshot();
            if view.ready && view.observed.elapsed() < network_ui::SNAPSHOT_LIFETIME { return Ok::<_, Box<dyn std::error::Error>>(view); }
            ui.set_status(&format!("{label}\n{}\n\nChecking connection and accepted queues ({}s).\nEsc/q: return to menu. Background recovery continues.", view.status, started.elapsed().as_secs()));
            if started.elapsed() >= Duration::from_secs(30) {
                return Err(format!("Queue synchronization has not completed after 30 seconds. {}. Background recovery continues; retry Host/Join after the connection recovers.", view.status).into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await else { return Ok(None) };
    let view = result?;
    if ui.finish() {
        return Ok(None);
    }
    Ok(Some(view))
}
async fn wait_tx(service: &Service, id: Hash, enabled: bool, label: &str) -> Result<bool> {
    let ui = network_ui::Setup::new(enabled)?;
    ui.set_status(&format!("{label}\nWaiting for chain acceptance.\nTransaction: {id}"));
    loop {
        if service.was_confirmed(id) {
            return Ok(!ui.finish());
        }
        let view = service.snapshot();
        if view.conflicted.contains(&id) {
            return Err("another accepted transaction replaced this operation; refresh the wallet".into());
        }
        if ui.read(tokio::time::sleep(Duration::from_millis(100))).await.is_none() {
            return Ok(false);
        }
    }
}
async fn create_set(service: &Service, wager: u64, enabled: bool) -> Result<Option<Set>> {
    let Some(view) = ready(service, enabled, "Checking existing reusable queues…").await? else { return Ok(None) };
    let automatic = view.auto_create;
    let floor = reusable::recommended_reserve(wager)?;
    let reserve = service.queue_reserve_target().max(floor);
    let capital = wager.checked_add(reserve).ok_or("queue capital overflow")?;
    let mut nonce = [0u8; 32];
    getrandom::getrandom(&mut nonce).map_err(|_| "secure randomness unavailable")?;
    let nonce = Hash::from_bytes(nonce);
    let owner = service.player();
    let ui = network_ui::Setup::new(enabled)?;
    ui.set_status("Preparing reusable queues and their full network fee…");
    let Some(result) = ui
        .read(service.quote(vec![], move |b, coins, fee| {
            let (tx, states) = reusable::create_with_nonce(b, &owner, capital, floor, coins, fee, nonce)?;
            let set = reusable::accepted_set(&tx, [0, 1, 2], states, u64::MAX);
            Ok((tx, set))
        }))
        .await
    else {
        return Ok(None);
    };
    let quote = result?;
    if ui.finish() {
        return Ok(None);
    }
    let body = format!(
        "Queue balance: {} KAS\nWager backing: {} KAS\nReserve: {} KAS\n\nMoves and contacts can draw from the reserve above the protected floor.\nClose an idle set to withdraw its balance.\n\n{}\n{}",
        menu::format_kas(capital),
        menu::format_kas(wager),
        menu::format_kas(reserve),
        fee_details(service, &quote)?,
        if automatic {
            "No existing sets · uses your auto-approval setting."
        } else {
            "Additional set · press Enter to approve."
        }
    );
    if !review("CREATE REUSABLE QUEUES", body, automatic).await? {
        return Ok(None);
    }
    let set_id = quote.data.id()?;
    let successors = vec![quote.data.clone()];
    let id = service.authorize(quote, "create queues", Effect::Wallet { closes: vec![] }, successors, true, None).await?;
    if !wait_tx(service, id, enabled, "Reusable queue creation submitted.").await? {
        return Ok(None);
    }
    Ok(service.snapshot().records.into_iter().find(|r| r.set.id().ok() == Some(set_id)).map(|r| r.set))
}
async fn choose_set(service: &Service, wager: u64, enabled: bool) -> Result<Option<Set>> {
    loop {
        let Some(view) = ready(service, enabled, "Checking reusable queue availability…").await? else { return Ok(None) };
        if let Some(set) = Service::available(&view).into_iter().max_by_key(|s| s.balance()) {
            return Ok(Some(set.clone()));
        }
        if view.auto_create {
            return create_set(service, wager, enabled).await;
        }
        let status = sets_frame(&view);
        match menu::queue_choice(
            "Queues are busy",
            &format!(
                "{status}\n\nA set becomes reusable only after settlement is accepted.\nPrevious-contract sets can be closed or consolidated from Wallet.\n\n  1  Wait for an existing set\n  2  Create an alternate set (manual approval)"
            ),
            "12",
        )? {
            Some('1') => {
                let ui = network_ui::Setup::new(enabled)?;
                ui.set_status("Waiting for queue settlement. Esc/q: return to queue choices.");
                loop {
                    let view = service.snapshot();
                    if view.ready && !Service::available(&view).is_empty() {
                        break;
                    }
                    if ui.read(tokio::time::sleep(Duration::from_millis(100))).await.is_none() {
                        break;
                    }
                }
                let _ = ui.finish();
            }
            Some('2') => return create_set(service, wager, enabled).await,
            _ => return Ok(None),
        }
    }
}
fn sets_frame(view: &View) -> String {
    let records = view.records.iter().filter(|r| r.phase != node::queue_sets::Phase::Closed).collect::<Vec<_>>();
    set_rows(view, &records)
}
fn set_rows(view: &View, records: &[&node::queue_sets::Record]) -> String {
    if records.is_empty() {
        return if !view.auto_create {
            "Initial creation is pending or needs recovery; its exact transaction will be resumed.".into()
        } else {
            "No reusable queue sets yet.".into()
        };
    }
    records.iter()
        .enumerate()
        .map(|(i, r)| {
            let offered = view.offers.iter().any(|o| o.queue.id == r.set.id().unwrap() && o.queue.generation == r.set.generation());
            format!(
                "  {}  {} KAS · {} · set {}",
                i + 1,
                menu::format_kas(r.set.balance()),
                if offered {
                    "Offered to a guest"
                } else {
                    match r.phase {
                        node::queue_sets::Phase::Available => "Available",
                        node::queue_sets::Phase::Reserved(_) => "Reserved",
                        node::queue_sets::Phase::Active(_) => "Playing",
                        node::queue_sets::Phase::Settling(_) => "Settling",
                        node::queue_sets::Phase::Unavailable => "Recovering",
                    node::queue_sets::Phase::Legacy => "Previous contract · close to withdraw",
                        node::queue_sets::Phase::Closed => "Closed",
                    }
                },
                &r.set.id().unwrap().to_string()[..12]
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
#[derive(Clone)]
struct Expected {
    host: Vec<u8>,
    guest: Vec<u8>,
    terms: node::invitations::GameTerms,
    previous_tag: [u8; 8],
}
impl Expected {
    fn from_session(s: &Session) -> Result<Self> {
        Ok(Self {
            host: s.reservation.host.queue.owner.clone(),
            guest: s.reservation.guest.owner.clone(),
            terms: s.reservation.host.terms()?,
            previous_tag: s.reservation.host.tag,
        })
    }
    fn validate(&self, code: &HostCode, guest: &[u8]) -> Result<()> {
        if code.queue.owner != self.host || guest != self.guest || code.terms()? != self.terms || code.tag == self.previous_tag {
            return Err("Rematch conditions changed. Join from the main menu to review the new terms.".into());
        }
        Ok(())
    }
}
enum Next {
    Host { settings: menu::GameSettings, offer: Option<HostCode>, expected: Option<Expected> },
    Join { code: String, expected: Option<Expected> },
}
pub async fn host(
    c: &Config,
    wallet: Option<&Path>,
    json: bool,
    connection: &node::rpc::startup::Connection,
    settings: menu::GameSettings,
) -> Result<()> {
    matches(service(c, wallet, connection)?, json, Next::Host { settings, offer: None, expected: None }).await
}
pub async fn join(
    c: &Config,
    wallet: Option<&Path>,
    code: &str,
    json: bool,
    connection: &node::rpc::startup::Connection,
) -> Result<()> {
    matches(service(c, wallet, connection)?, json, Next::Join { code: code.into(), expected: None }).await
}
async fn matches(service: Service, json: bool, mut next: Next) -> Result<()> {
    let enabled = interactive(json);
    loop {
        let id = match next {
            Next::Host { settings, offer, expected } => {
                let code = if let Some(code) = offer {
                    code
                } else {
                    let Some(set) = choose_set(&service, u64::from(settings.stake_kas) * 100_000_000, enabled).await? else {
                        return Ok(());
                    };
                    let tag = node::invitations::new_rendezvous_tag(settings.terms())?;
                    let code = HostCode::new(&service.config().network, tag, &set)?;
                    let saved = code.clone();
                    let peer = expected.as_ref().map(|e| e.guest.clone());
                    service.edit(move |b| b.reserve_offer(saved, peer, false)).await?;
                    code
                };
                let text = code.text()?;
                let terms = code.terms()?;
                let ui = network_ui::Setup::new(enabled)?;
                ui.set_status(&format!("HOST · You play RIGHT\n{}\n\nShare this complete game code:\nCODE: {text}\n\nYour queues are ready. Waiting for the guest to reserve their wager.\nEsc/q: return to menu.",terms.summary()));
                if !enabled {
                    eprintln!("{}\nCODE: {text}", terms.summary());
                }
                let found = loop {
                    let candidate = service.snapshot().sessions.iter().rev().find(|s| s.code == code).map(|s| s.id);
                    if let Some(id) = candidate {
                        break Some(id);
                    }
                    if ui.read(tokio::time::sleep(Duration::from_millis(100))).await.is_none() {
                        break None;
                    }
                };
                let cancelled = ui.finish();
                if cancelled || found.is_none() {
                    let tag = code.tag;
                    service
                        .edit(move |b| {
                            b.abandon_offer(tag);
                            Ok(())
                        })
                        .await?;
                    return Ok(());
                }
                found.unwrap()
            }
            Next::Join { code, expected } => {
                let host = HostCode::parse(&code)?;
                let owner = service.player();
                if host.queue.owner == pubkey(&owner) {
                    return Err("host and guest must use different wallets".into());
                }
                if let Some(e) = &expected {
                    e.validate(&host, &pubkey(&owner))?;
                }
                let wager = u64::from(host.terms()?.stake_kas) * 100_000_000;
                let Some(set) = choose_set(&service, wager, enabled).await? else { return Ok(()) };
                let ui = network_ui::Setup::new(enabled)?;
                ui.set_status("Preparing your wager reservation…");
                let reservation = Reservation {
                    host: host.clone(),
                    guest: Descriptor::from_set(&set)?,
                    capital: set.balance().max(reusable::required_capital(&set, wager)?),
                };
                let payload = reservation.payload()?;
                let offer = reservation.offer()?;
                let before = set.clone();
                let capital = reservation.capital;
                let Some(quote) = ui
                    .read(service.quote(cells(&set), move |b, coins, fee| {
                        Ok((reusable::reserve_with_payload(b, &before, &offer, &owner, capital, coins, fee, payload.clone())?, ()))
                    }))
                    .await
                else {
                    return Ok(());
                };
                let quote = quote?;
                if ui.finish() {
                    return Ok(());
                }
                let body = format!(
                    "{}\n\nWager: {} KAS\nQueue top-up: {} KAS\nYour queues can cover move, contact and cancellation fees. Unspent funds stay in your queues.\n{}",
                    host.terms()?.summary(),
                    menu::format_kas(wager),
                    menu::format_kas(capital - set.balance()),
                    fee_details(&service, &quote)?
                );
                if expected.is_none() && !review("Join game · You play LEFT", body, true).await? {
                    return Ok(());
                }
                let id = quote.transaction.id();
                let seed = Session {
                    id,
                    reservation,
                    creation: quote.transaction.clone(),
                    creation_inputs: quote.inputs.clone(),
                    host_set:None,
                    stage: None,
                    cancel_requested: false,
                    created_ms: node::client::now_ms(),
                    authorized_fees: 0,
                    paid_fees: 0,
                    recovered: false,
                    unavailable: false,
                    cursor: Hash::from_bytes([0; 32]),
                    anchor: Hash::from_bytes([0; 32]),
                    fee_budget: service.config().fee_budget_sompi,
                };
                service.authorize(quote, "reserve wager", Effect::Game { session: id }, vec![], false, Some(seed)).await?;
                id
            }
        };
        let Some((request, session, new_offer)) = play(&service, id, json).await? else { return Ok(()) };
        let expected = Expected::from_session(&session)?;
        next = match request {
            network_ui::RematchRequest::Host => Next::Host {
                settings: menu::GameSettings {
                    speed: expected.terms.speed,
                    ball_count: expected.terms.ball_count,
                    goal_limit: expected.terms.win_score,
                    stake_kas: expected.terms.stake_kas,
                },
                offer: new_offer,
                expected: Some(expected),
            },
            network_ui::RematchRequest::Join(code) => Next::Join { code, expected: Some(expected) },
        };
    }
}

struct Work {
    action: GameAction,
    epoch: u64,
    batch: Vec<network_ui::Movement>,
    started: Instant,
    ready_since: Instant,
    task: tokio::task::JoinHandle<std::result::Result<Quote<()>, String>>,
    completed: Option<std::result::Result<Quote<()>, String>>,
}
struct AuthorizationWork {
    transaction: Hash,
    batch: Vec<network_ui::Movement>,
    task: tokio::task::JoinHandle<std::result::Result<Hash, String>>,
    completed: Option<std::result::Result<Hash, String>>,
}

const DEFER_MOVEMENT_AFTER_DAA: u64 = 20;
const CONTACT_WARNING_AFTER_DAA: u64 = 6 * DAA_PER_SECOND as u64;

#[derive(Clone, Copy)]
struct ContactWait {
    receiver: usize,
    deadline: u64,
    expiry: u64,
}
impl ContactWait {
    fn warning_due(self, daa: u64) -> bool {
        daa >= self.deadline.saturating_add(CONTACT_WARNING_AFTER_DAA)
    }

    fn status(self, side: usize, daa: u64, ready: bool, stale: bool, timeout_acknowledged: bool) -> Option<String> {
        if stale || (!ready && self.warning_due(daa)) {
            return Some("Connection lag · Checking chain".into());
        }
        if !ready {
            return None;
        }
        if self.receiver != side {
            if timeout_acknowledged {
                return Some("Timeout sent · Waiting for chain acceptance".into());
            }
            if daa >= self.expiry {
                return Some("Claiming opponent timeout…".into());
            }
            if self.warning_due(daa) {
                let remaining = (self.expiry - daa).div_ceil(DAA_PER_SECOND as u64);
                return Some(format!("Waiting for opponent’s proof · Timeout in ~{remaining}s"));
            }
        } else if self.warning_due(daa) {
            return Some("Your contact proof is pending · Checking chain".into());
        }
        None
    }
}

async fn play(service: &Service, id: Hash, json: bool) -> Result<Option<(network_ui::RematchRequest, Session, Option<HostCode>)>> {
    let _pause_legacy_recovery = node::hosted::pause_background();
    service.select(id).await?;
    let initial = service.snapshot().current.ok_or("game journal not loaded")?;
    let player_key = pubkey(&service.player());
    let side = initial.side(&player_key);
    let ui = network_ui::Frontend::new(interactive(json), true, side as i64, false, &service.config().network)?;
    let mut queued = VecDeque::new();
    let mut work: Option<Work> = None;
    let mut authorization: Option<AuthorizationWork> = None;
    let mut rematch_offer: Option<HostCode> = None;
    let mut epoch = service.snapshot().epoch;
    let mut submitted: Vec<(Hash, Vec<network_ui::Movement>)> = vec![];
    let mut accepted_before=std::collections::HashSet::new();
    let mut next_action = Instant::now();
    let mut status = String::new();
    let mut ready_batch = None;
    let mut next_contact_cache = None;
    loop {
        let view = service.snapshot();
        let Some(session) = view.current.clone() else {
            tokio::time::sleep(Duration::from_millis(25)).await;
            continue;
        };
        if ui.quit.load(Ordering::Relaxed) {
            break;
        }
        if side==1 && session.cancel_requested && matches!(session.stage,Some(QueueStage::Lobby{..})) {
            if let Some(w)=work.take(){w.task.abort();}drop(ui);
            let _=menu::queue_choice("Host offer cancelled","Your matching wager was not newly authorized. Any earlier signed submission remains tracked. Start a fresh Host offer to play; the guest can cancel their reservation independently.","")?;
            return Ok(None);
        }
        if session.unavailable || session.recovered {
            if let Some(w) = work.take() {
                w.task.abort();
            }
            drop(ui);
            let message = if session.recovered {
                "Your idle queues were verified on chain and can be reused. This node no longer has enough history to show the old result. Start a fresh game from Host or Join."
            } else {
                "This game needs retained chain history to recover its current state. Recovery continues in the background without the opponent's signature. Other available sets can still be used; Wallet > Reusable queues also allows a manually approved alternate set."
            };
            let _ = menu::queue_choice("Queue recovery", message, "")?;
            return Ok(None);
        }
        if matches!(session.stage, Some(QueueStage::Cancelled(_))) {
            break;
        }
        if epoch != view.epoch {
            epoch = view.epoch;
            ready_batch = None;
            next_contact_cache = None;
            // Preserve intent across a rollback. Only actual conflicts are
            // rebuilt; an authorized uncertain transaction stays reserved.
            for tx in accepted_before.difference(&view.confirmed){
                for (_,batch) in submitted.iter().filter(|(id,_)|id==tx){for movement in batch{ui.restore_movement(movement,*tx);}}
            }
            if let Some(w) = work.take() {
                w.task.abort();
            }
            ui.offer_rematch(None);
            rematch_offer = None;
        }
        submitted.retain(|(tx, batch)| {
            if view.conflicted.contains(tx) {
                for m in batch {
                    ui.requeue_movement(m);
                    if !queued.iter().any(|n: &network_ui::Movement| n.pressed == m.pressed) {
                        queued.push_back(m.clone());
                    }
                }
                false
            } else {
                true
            }
        });
        accepted_before=view.confirmed.clone();
        queued.make_contiguous().sort_by_key(|m| m.pressed);
        queued.extend(ui.input.try_iter());
        if let Some(stage) = &session.stage {
            if let QueueStage::Live(m) = stage {
                if let Ok(plan) = project(&m.snapshot(), view.daa) {
                    ui.prune_unsent(&mut queued, &plan.world, side as i64, &view.confirmed.iter().copied().collect::<Vec<_>>());
                }
            } else if matches!(stage,QueueStage::Collecting{..}|QueueStage::Finished{..}|QueueStage::Cancelled(_)) {
                ui.clear_preview();
                queued.clear();
            }
            if matches!(stage, QueueStage::Finished { .. }) && rematch_offer.is_none() && side == 1 && view.ready {
                rematch_offer = view.sessions.iter().find(|s| s.id == id).and_then(|s| s.rematch.clone());
            }
            if matches!(stage, QueueStage::Finished { .. }) && rematch_offer.is_none() && side == 1 && view.ready {
                let original = stage
                    .sets()
                    .into_iter()
                    .find(|s| s.lanes[0].state.queue.owner == player_key)
                    .ok_or("missing settled host queues")?;
                if let Some(set) = Service::available(&view)
                    .into_iter()
                    .find(|s| s.id().ok() == original.id().ok() && s.generation() == original.generation())
                {
                    let code = HostCode::new(
                        &service.config().network,
                        node::invitations::new_rendezvous_tag(session.reservation.host.terms()?)?,
                        &set,
                    )?;
                    let saved = code.clone();
                    let peer = session.reservation.guest.owner.clone();
                    service.edit(move |b| b.reserve_offer(saved, Some(peer), true)).await?;
                    rematch_offer = Some(code);
                }
            }
            if matches!(stage, QueueStage::Finished { .. }) && stage.restoring_set(&player_key).is_none() {
                ui.offer_rematch(if side == 1 {
                    rematch_offer.as_ref().map(|c| network_ui::RematchOffer::Host {
                        code: c.text().unwrap_or_default(),
                        summary: session.reservation.host.terms().unwrap().summary(),
                    })
                } else {
                    Some(network_ui::RematchOffer::Guest)
                });
                if let Some(code) = &rematch_offer {
                    let matched =
                        view.sessions.iter().any(|s| s.code == *code && s.guest == session.reservation.guest.owner && s.id != id);
                    if matched {
                        ui.request_verified_host_rematch();
                    }
                }
                if let Some(request) = ui.take_rematch_request() {
                    return Ok(Some((request, session, rematch_offer)));
                }
            } else if matches!(stage, QueueStage::Finished { .. }) {
                ui.offer_rematch(None);
            }
        }
        let live = matches!(session.stage, Some(QueueStage::Live(_)));
        let observation = if live { view.chain_observed } else { view.ready.then_some(view.observed) };
        let stale = observation.is_none_or(|at| at.elapsed() >= network_ui::SNAPSHOT_LIFETIME);
        let fresh = !stale;
        let contact_wait = match session.stage.as_ref() {
            Some(QueueStage::Live(m)) if !m.court.state.world.ended() => {
                if let Some((_, contact)) = next_contact_cache.filter(|(court, _)| {
                    *court == (m.court.outpoint, m.court.utxo.block_daa_score)
                }) {
                    Some(contact)
                } else {
                    let snapshot = m.snapshot();
                    let (receiver, deadline) = next_contact(&snapshot)?;
                    let expiry = reusable::contact_expiry(&snapshot, deadline)?;
                    let contact = ContactWait { receiver, deadline: u64::try_from(deadline)?, expiry: u64::try_from(expiry)? };
                    next_contact_cache = Some(((m.court.outpoint, m.court.utxo.block_daa_score), contact));
                    Some(contact)
                }
            }
            _ => None,
        };
        let defer_moves = contact_wait.is_some_and(|contact| {
            contact.receiver != side && view.daa >= contact.deadline.saturating_add(DEFER_MOVEMENT_AFTER_DAA)
        });
        if authorization.as_ref().is_some_and(|w| w.completed.is_some() || w.task.is_finished()) {
            let mut w = authorization.take().unwrap();
            let result = match w.completed.take() {
                Some(result) => result,
                None => w.task.await.map_err(|e| e.to_string())?,
            };
            match result {
                Ok(tx) => {
                    for movement in &w.batch { queued.retain(|m| m.pressed != movement.pressed); }
                    if !w.batch.is_empty() { submitted.push((tx, w.batch)); }
                    status.clear();
                }
                Err(e) => { ui.release_prepared(w.transaction); status = e; }
            }
            next_action = Instant::now();
            // Authorization publishes reservations asynchronously. Refresh the
            // snapshot before choosing another lane or a second start.
            continue;
        }
        if work.as_ref().is_some_and(|w| w.completed.is_some() || w.task.is_finished()) {
            let mut w = work.take().unwrap();
            let quote = match w.completed.take() {
                Some(result) => result,
                None => w.task.await.map_err(|e| e.to_string())?,
            };
            if w.epoch == view.epoch && !ui.quit.load(Ordering::Relaxed) {
                match quote {
                    Ok(quote) => {
                        if matches!(w.action, GameAction::Move { .. } | GameAction::Resolve { .. }) {
                            node::queue_service::gameplay_timing("prepared", quote.transaction.id(), serde_json::json!({
                                "action": w.action.label(),
                                "batch_size": w.batch.len(),
                                "ready_to_prepare_ms": w.started.saturating_duration_since(w.ready_since).as_micros() as f64 / 1000.0,
                                "prepare_ms": w.started.elapsed().as_micros() as f64 / 1000.0,
                                "keypress_to_prepare_ms": w.batch.first().map(|m| w.started.saturating_duration_since(m.pressed).as_micros() as f64 / 1000.0),
                            }));
                        }
                        let mut valid = true;
                        if !w.batch.is_empty() {
                            queued.extend(ui.input.try_iter());
                            let current = ui.movement_batch(&queued);
                            valid = !defer_moves && w.batch.iter().all(|m| current.iter().any(|n| n.pressed == m.pressed));
                        }
                        if valid {
                            let transaction = quote.transaction.id();
                            for movement in &w.batch { ui.submitted_movement(movement, transaction); }
                            let svc = service.clone();
                            authorization = Some(AuthorizationWork {
                                transaction, batch: w.batch,
                                completed: None,
                                task: tokio::spawn(async move {
                                    svc.authorize(quote, w.action.label(), Effect::Game { session: id }, vec![], false, None)
                                        .await.map_err(|e| e.to_string())
                                }),
                            });
                        }
                    }
                    Err(e) => {
                        if e != status && let GameAction::Resolve { side, until } = w.action {
                            let row = serde_json::json!({"type": "queue_resolve_prepare_failed", "game": id,
                                "observed_unix_ms": node::client::now_ms(), "daa": view.daa, "side": side,
                                "until": until, "message": e});
                            tokio::task::spawn_blocking(move || node::rpc::startup::diagnostic(&row.to_string()));
                        }
                        status = e;
                    }
                }
            }
            next_action = Instant::now() + Duration::from_millis(100);
        }
        let movement_batch = ui.movement_batch(&queued);
        match movement_batch.first() {
            Some(m) if ready_batch.is_none_or(|(pressed, _)| pressed != m.pressed) => {
                ready_batch = Some((m.pressed, Instant::now()));
            }
            None => ready_batch = None,
            _ => {}
        }
        if work.is_none() && authorization.is_none() && fresh && Instant::now() >= next_action {
            let mut batch = vec![];
            let action = match session.stage.as_ref() {
                Some(QueueStage::Lobby { .. })
                    if side == 1 && !view.pending.iter().any(|p| p.game == Some(id)) && !session.cancel_requested =>
                {
                    Some(GameAction::Start)
                }
                Some(QueueStage::Live(m)) if !m.court.state.world.ended() => {
                    let contact = contact_wait.expect("live game has a next contact");
                    let receiver = contact.receiver;
                    let until = contact.deadline as i64;
                    let pending_court = view.pending.iter().any(|p| p.inputs.contains(&m.court.outpoint));
                    let lanes_free=!m.sets[side].lanes.iter().any(|c|view.pending.iter().any(|p|p.inputs.contains(&c.outpoint)));
                    let count:usize=m.sets[side].lanes.iter().map(|c|c.state.queue.count).sum();
                    let prefix=(view.daa.min(i64::MAX as u64) as i64).min(until);
                    if !pending_court
                        && receiver == side
                        && view.daa >= until as u64 + RESOLVE_DAA
                        && lanes_free
                    {
                        Some(GameAction::Resolve { side, until })
                    } else if defer_moves {
                        // Keep local input queued without paying for moves while the opponent's contact blocks play.
                        None
                    } else if !pending_court && lanes_free && count>=8 && prefix>m.sets[side].lanes[0].state.queue.through
                        && checkpoint_plan(&m.snapshot(),side,prefix).is_ok() {
                        // Match the previous client's early compaction policy;
                        // waiting for contact can otherwise fill all 12 slots.
                        Some(GameAction::Resolve{side,until:prefix})
                    } else {
                        // A contact resolve need not block an independent
                        // unreserved lane (especially the other player's).
                        let movement = view.movement.as_ref().unwrap_or(m);
                        batch = movement_batch;
                        let direction = batch.iter().map(|m| m.direction).sum::<i64>().clamp(-MAX_MOVE_STEPS, MAX_MOVE_STEPS);
                        if direction != 0 {
                            (0..3)
                                .filter(|i| {
                                    movement.sets[side].lanes[*i].state.queue.count < MAX_EVENTS
                                        && !view.pending.iter().any(|p| p.inputs.contains(&movement.sets[side].lanes[*i].outpoint))
                                })
                                .min_by_key(|i| movement.sets[side].lanes[*i].state.queue.count)
                                .map(|lane| GameAction::Move { lane, direction })
                        } else {
                            None
                        }
                    }
                }
                _ => None,
            };
            if let Some(action) = action {
                let svc = service.clone();
                let mut snapshot = session.clone();
                if matches!(action, GameAction::Move { .. }) {
                    if let Some(movement) = &view.movement { snapshot.stage = Some(QueueStage::Live(movement.clone())); }
                }
                let planned = action.clone();
                let started = Instant::now();
                let ready_since = batch.first().and_then(|m| ready_batch.filter(|(pressed, _)| *pressed == m.pressed))
                    .map_or(started, |(_, at)| at);
                work = Some(Work {
                    action,
                    epoch: view.epoch,
                    batch,
                    started,
                    ready_since,
                    completed: None,
                    task: tokio::spawn(async move { svc.prepare_action(snapshot, planned).await.map_err(|e| e.to_string()) }),
                });
            }
        }
        let stage = session.stage.as_ref().map(QueueStage::display).unwrap_or(node::tracker::Stage::AwaitingLobby);
        let hint = match session.stage {
            None => "Reservation submitted. Waiting for chain acceptance.",
            Some(QueueStage::Lobby { .. }|QueueStage::Invitation{..}) if side == 0 => "Your wager is reserved. Waiting for the host to start.",
            Some(QueueStage::Lobby { .. }) => "Guest reservation accepted. Preparing your matching wager and start.",
            Some(QueueStage::Collecting { .. }) => "Settling balances into the same reusable queues.",
            _ if session.stage.as_ref().and_then(|stage| stage.restoring_set(&player_key)).is_some() =>
                "Payout accepted. Restoring your queues for the next game.",
            Some(QueueStage::Finished { .. }) => "Settlement accepted. Winnings remain in your reusable queues.",
            _ => "Following accepted game state.",
        };
        let pending = view.pending.iter().filter(|p| p.game == Some(id)).collect::<Vec<_>>();
        let timeout_acknowledged = pending.iter().any(|p| p.purpose == "timeout" && p.acknowledged);
        let wait_status = contact_wait.and_then(|contact| contact.status(side, view.daa, observation.is_some(), stale, timeout_acknowledged));
        ui.observe_clock(network_ui::ClockSample { epoch: view.clock.epoch, daa: view.clock.daa, observed: view.clock.observed });
        ui.update(network_ui::Display {
            stage,
            daa: view.daa,
            ready: fresh,
            observed: observation.unwrap_or(view.observed),
            clock_epoch: view.epoch,
            pending: pending.len(),
            payout_submitted: pending.iter().any(|p| p.purpose == "collect queues"),
            accepted_previews: view.confirmed.iter().copied().collect(),
            queued: queued.len(),
            spending: Some(network_ui::Spending {
                paid: u128::from(session.paid_fees),
                pending: pending.iter().map(|p| p.fee).sum(),
                complete: !session.recovered,
            }),
            reorgs: view.epoch,
            status: if live {
                if status.is_empty() && fresh { hint.into() }
                else if status.is_empty() { view.status.clone() } else { status.clone() }
            } else if status.is_empty() { format!("{hint}\n{}", view.status) } else { format!("{hint}\n{status}") },
            last_reorg: None,
            contact_deadline: contact_wait.map(|contact| contact.deadline),
            contact_status: wait_status,
            recent_transactions: view.recent_transactions.clone(),
            settlement_hint: matches!(session.stage, Some(QueueStage::Collecting { .. } | QueueStage::Finished { .. }))
                .then(|| hint.into()),
            lobby_hint: Some(hint.into()),
        });
        if json {
            println!("{}", serde_json::json!({"game":id,"daa":view.daa,"ready":fresh,"status":hint,"stage":session.stage}));
        }
        tokio::select! {
            result = async { (&mut work.as_mut().unwrap().task).await }, if work.is_some() => {
                work.as_mut().unwrap().completed = Some(result.map_err(|e| e.to_string())?);
            }
            result = async { (&mut authorization.as_mut().unwrap().task).await }, if authorization.is_some() => {
                authorization.as_mut().unwrap().completed = Some(result.map_err(|e| e.to_string())?);
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    if let Some(w) = work {
        w.task.abort();
    }
    service.edit(move |b| b.request_cancel(id)).await?;
    if let Some(code) = rematch_offer {
        service
            .edit(move |b| {
                b.abandon_offer(code.tag);
                Ok(())
            })
            .await?;
    }
    Ok(None)
}

pub fn configure_reserve_target(
    c: &Config,
    config_path: &Path,
    keyfile: Option<&Path>,
    connection: &node::rpc::startup::Connection,
) -> Result<()> {
    let service = service(c, keyfile, connection)?;
    let mut notice = String::new();
    loop {
        let prompt = format!(
            "Queue reserve target · {} KAS\nNew sets and refills use this target. Changes are saved for future launches.\n\n{notice}Enter whole KAS (minimum 3, q to go back): ",
            menu::format_kas(service.queue_reserve_target()),
        );
        let menu::Prompt::Selected(input) = menu::wallet_text(&prompt)? else { return Ok(()) };
        let Ok(kas) = input.parse::<u64>() else {
            notice = "Enter a whole number of KAS, at least 3.\n".into();
            continue;
        };
        match service.set_queue_reserve_target(config_path, kas) {
            Ok(()) => return Ok(()),
            Err(error) => notice = format!("{error}\n"),
        }
    }
}

pub async fn wallet(
    c: &Config,
    keyfile: Option<&Path>,
    connection: &node::rpc::startup::Connection,
    settings: menu::GameSettings,
) -> Result<()> {
    let service = service(c, keyfile, connection)?;
    const PAGE_SIZE: usize = 9;
    let mut page = 0;
    loop {
        let view = service.snapshot();
        let records = view.records.iter().filter(|r| r.phase != node::queue_sets::Phase::Closed).collect::<Vec<_>>();
        let last_page = records.len().saturating_sub(1) / PAGE_SIZE;
        page = page.min(last_page);
        let first = page * PAGE_SIZE;
        let rows = &records[first..(first + PAGE_SIZE).min(records.len())];
        let mut text = format!(
            "{}\n\nC  Create a new set ({})\nH  Closed set history",
            set_rows(&view, rows),
            if view.auto_create { "uses auto-approval setting" } else { "manual approval" },
        );
        let mut keys = format!("ch{}", &"123456789"[..rows.len()]);
        if page > 0 {
            text.push_str("\nP  Previous page");
            keys.push('p');
        }
        if page < last_page {
            text.push_str("\nN  Next page");
            keys.push('n');
        }
        let Some(value) = menu::queue_choice("Reusable queues", &text, &keys)? else { return Ok(()) };
        if value == 'p' {
            page -= 1;
            continue;
        }
        if value == 'n' {
            page += 1;
            continue;
        }
        if value == 'c' {
            let _ = create_set(&service, u64::from(settings.stake_kas) * 100_000_000, interactive(false)).await?;
            continue;
        }
        if value == 'h' {
            let history = view.records.iter().filter(|r| r.phase == node::queue_sets::Phase::Closed)
                .map(|r| format!("Closed · set {}", r.set.id().unwrap())).collect::<Vec<_>>();
            let body = if history.is_empty() { "No closed queue sets.".into() } else {
                format!("Historical records only. These sets no longer hold funds.\n\n{}", history.join("\n"))
            };
            let _ = menu::queue_choice("Closed set history", &body, "")?;
            continue;
        }
        let Some(record) = value.to_digit(10).and_then(|i| i.checked_sub(1)).and_then(|i| rows.get(i as usize)) else {
            continue;
        };
        let set = record.set.clone();
        let tags = view
            .offers
            .iter()
            .filter(|o| o.queue.id == set.id().unwrap() && o.queue.generation == set.generation())
            .map(|o| o.tag)
            .collect::<Vec<_>>();
        if set.idle() && !tags.is_empty() {
            if menu::queue_choice(
                "Host offer",
                "This idle set has an open host code.\n\n  1  Cancel the code and release the set",
                "1",
            )? == Some('1')
            {
                service
                    .edit(move |b| {
                        for tag in tags {
                            b.abandon_offer(tag);
                        }
                        Ok(())
                    })
                    .await?;
            }
            continue;
        }
        let legacy=record.phase==node::queue_sets::Phase::Legacy;
        if !legacy && !Service::available(&view).iter().any(|s| s.id().ok() == set.id().ok()) {
            let _ = menu::queue_choice(
                "Queue set unavailable",
                "This set is offered, active or settling. Check Game status or wait for automatic settlement. Previous-contract active, offered or pending sets must first finish or be cancelled with the previous client.",
                "",
            )?;
            continue;
        }
        let Some(action) = menu::queue_choice(
            if legacy{"Previous queue contract"}else{"Manage queue set"},
            &if legacy {
                format!("Balance: {} KAS\nPrevious contract: close this idle set to return its funds.\n\n  3  Close idle set",menu::format_kas(set.balance()))
            }else{format!("Balance: {} KAS\nProtected buffer: {} KAS\n\n  1  Top up\n  2  Withdraw available balance (keep buffer)\n  3  Close idle set (return the whole balance)",menu::format_kas(set.balance()),menu::format_kas(set.reserve()))},
            if legacy{"3"}else{"123"},
        )?
        else {
            continue;
        };
        let owner = service.player();
        let root = set.id()?;
        let mut capital = set.reserve();
        if action == '1' {
            let menu::Prompt::Selected(value) = menu::wallet_text("Whole KAS to add (q: back): ")? else {
                continue;
            };
            let Some(amount) = value.parse::<u64>().ok().filter(|v| *v > 0).and_then(|v| v.checked_mul(100_000_000)) else {
                continue;
            };
            capital = set.balance().checked_add(amount).ok_or("top-up overflow")?;
        }
        if action == '2' && set.balance() == capital {
            continue;
        }
        let sources = cells(&set);
        let before = set.clone();
        let quote = service
            .quote(sources, move |b, coins, fee| {
                let old_artifact = if legacy { Some(node::queue_sets::artifact_for(&before)?) } else { None };
                let old_builder=old_artifact.as_ref().map(argent_runtime::TxBuilder::new).transpose()?;
                let b=old_builder.as_ref().unwrap_or(b);
                let tx = if action == '3' {
                    reusable::close(b, &before, &owner, coins, fee)?
                } else {
                    reusable::resize(b, &before, &owner, capital, coins, fee)?
                };
                let next = if action == '3' {
                    vec![]
                } else {
                    vec![reusable::accepted_set(&tx, [0, 1, 2], std::array::from_fn(|i| before.lanes[i].state.clone()), u64::MAX)]
                };
                Ok((tx, next))
            })
            .await?;
        let body = format!(
            "Set: {}\n{}\n{}",
            &root.to_string()[..16],
            if action == '3' {
                format!("Return {} KAS to the wallet and close this set.", menu::format_kas(set.balance().saturating_sub(quote.fee)))
            } else {
                format!("New queue balance: {} KAS. The identity is preserved.", menu::format_kas(capital))
            },
            fee_details(&service, &quote)?
        );
        if !review("Confirm queue change", body, false).await? {
            continue;
        }
        let successors = quote.data.clone();
        let id = service
            .authorize(
                quote,
                "manage queues",
                Effect::Wallet { closes: if action == '3' { vec![root] } else { vec![] } },
                successors,
                false,
                None,
            )
            .await?;
        let _ = wait_tx(&service, id, interactive(false), "Queue change submitted.").await?;
    }
}
pub async fn consolidate(c: &Config, wallet: Option<&Path>, connection: &node::rpc::startup::Connection) -> Result<()> {
    use kaspa_pong::terminal::{PromptKey, Terminal};

    const TITLE: &str = "CONSOLIDATE WALLET";
    let screen = interactive(false).then(Terminal::new).transpose()?;
    let draw = |body: &str| -> Result<()> {
        if let Some(screen) = &screen {
            screen.draw_with_panels(&format!("{TITLE}\n\n{body}"), None, None)?;
        }
        Ok(())
    };
    let cancelled = || -> Result<bool> {
        Ok(match &screen {
            Some(screen) => matches!(screen.prompt_key(Duration::ZERO)?, Some(PromptKey::Back | PromptKey::Exit | PromptKey::Character('q' | 'Q'))),
            None => false,
        })
    };
    if let Some(screen) = &screen { screen.begin_prompt()?; }
    draw("Checking available wallet coins and idle queues…\n\nEsc/q: back")?;
    let service = service(c, wallet, connection)?;
    let started = Instant::now();
    let view = loop {
        if cancelled()? { return Ok(()); }
        let view = service.snapshot();
        if view.ready && view.observed.elapsed() < network_ui::SNAPSHOT_LIFETIME { break view; }
        if started.elapsed() >= Duration::from_secs(30) {
            return Err(format!("Queue synchronization has not completed after 30 seconds. {}", view.status).into());
        }
        draw(&format!("Checking available wallet coins and idle queues…\n{}\n\nEsc/q: back", view.status))?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let owner = service.player();
    let sets = consolidation::eligible_sets(&view);
    let include_queues = if sets.is_empty() { false } else {
        let total=sets.iter().map(Set::balance).sum::<u64>();
        let body=format!("1  Consolidate ordinary wallet coins only\n2  Include {} idle queue sets ({} KAS)\n\nIncluding queues closes those sets and returns their balances to the wallet.\nActive, reserved, offered and pending sets are excluded.\nReview the total fee and approve once.",sets.len(),menu::format_kas(total));
        let choice = match &screen {
            Some(screen) => menu::queue_choice_on(screen, TITLE, &body, "12")?,
            None => menu::queue_choice(TITLE, &body, "12")?,
        };
        match choice { Some('1') => false, Some('2') => true, _ => return Ok(()) }
    };
    draw("Preparing consolidation and calculating the total fee…")?;
    let mut steps = Vec::new();
    let mut coins = consolidation::ordinary_coins(&service).await?;
    let ordinary_count = coins.len();
    let closed_count = if include_queues { sets.len() } else { 0 };
    if include_queues {
        let mut groups:Vec<(argent_runtime::Artifact,Vec<Set>)>=vec![];
        for set in sets {
            let artifact=node::queue_sets::artifact_for(&set)?;
            if let Some((_,sets))=groups.iter_mut().find(|(a,sets)|a.id==artifact.id && sets.len()<4) {sets.push(set);}else{groups.push((artifact,vec![set]));}
        }
        for (artifact,sets) in groups {
            let sources=sets.iter().flat_map(cells).collect::<Vec<_>>();let closes=sets.iter().map(Set::id).collect::<Result<Vec<_>>>()?;
            let quote=service.quote(sources,move|_b,_coins,fee|{let b=argent_runtime::TxBuilder::new(&artifact)?;Ok((consolidation::close_group(&b,&sets,&owner,fee)?,()))}).await?;
            coins.push(consolidation::closure_output(&quote.transaction));
            steps.push((quote, closes));
        }
    }
    if coins.len() >= 2 {
        steps.push((consolidation::quote_merge(&service, coins).await?, vec![]));
    }
    let Some((last, _)) = steps.last() else {
        let message = "Nothing to consolidate. Fewer than two mature, unreserved ordinary wallet outputs.";
        if let Some(screen) = &screen { menu::queue_choice_on(screen, TITLE, message, "")?; }
        else { menu::queue_choice(TITLE, message, "")?; }
        return Ok(());
    };
    let fee = steps.iter().try_fold(0u64, |total, (quote, _)| total.checked_add(quote.fee)).ok_or("consolidation fee overflow")?;
    let inputs = if include_queues {
        format!("Close {closed_count} idle queue set(s) and combine the proceeds with {ordinary_count} ordinary wallet output(s).")
    } else {
        format!("Combine {ordinary_count} ordinary wallet outputs into one.")
    };
    let body = format!(
        "{inputs}\n\nTransactions: {}\nTotal network fee: {} KAS\nReturn to wallet: {} KAS",
        steps.len(),
        menu::format_kas(fee),
        menu::format_kas(last.transaction.outputs[0].value),
    );
    let approved = match &screen {
        Some(screen) => menu::operation_review_on(screen, TITLE, &body, None)?,
        None => review(TITLE, body, false).await?,
    };
    if !approved {
        return Ok(());
    }
    let count = steps.len();
    for (index, (quote, closes)) in steps.into_iter().enumerate() {
        if cancelled()? { return Ok(()); }
        let (purpose, action) = if closes.is_empty() {
            ("manual consolidation", "Combining wallet outputs")
        } else {
            ("close queues for manual consolidation", "Closing idle queue sets")
        };
        let step = index + 1;
        draw(&format!("{step} of {count} · {action}\n{} confirmed\n\nSubmitting transaction…\nEsc/q: return to wallet. The current submission continues.", index))?;
        let id = service.authorize(quote, purpose, Effect::Wallet { closes }, vec![], false, None).await?;
        draw(&format!("{step} of {count} · {action}\n{} confirmed\n\nWaiting for chain acceptance.\nTransaction: {id}\n\nEsc/q: return to wallet. Any remaining steps stop.", index))?;
        loop {
            if cancelled()? { return Ok(()); }
            if service.was_confirmed(id) { break; }
            if service.snapshot().conflicted.contains(&id) {
                return Err("another accepted transaction replaced this operation; refresh the wallet".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    if let Some(screen) = &screen {
        menu::queue_choice_on(screen, TITLE, &format!("Consolidation complete. {count} of {count} transactions confirmed."), "")?;
    }
    Ok(())
}

pub async fn resume(c: &Config, wallet: Option<&Path>, connection: &node::rpc::startup::Connection, _json: bool) -> Result<()> {
    let service = service(c, wallet, connection)?;
    let view = service.snapshot();
    let sessions = view.sessions.iter().rev().filter(|s|!s.finished).collect::<Vec<_>>();
    if sessions.is_empty() {
        let _ = menu::queue_choice("GAME STATUS", "No used queues need recovery. Idle sets are available from Host or Join.", "")?;
        return Ok(());
    }
    let text = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}  {} · {}", i + 1, &s.id.to_string()[..12], if s.label=="Playing"{"Awaiting result or timeout"}else{&s.label}))
        .collect::<Vec<_>>()
        .join("\n");
    let menu::Prompt::Selected(value) = menu::wallet_text(&format!("GAME STATUS\n{text}\n\nGame number (q: back): "))? else {
        return Ok(());
    };
    if let Some(index) = value.parse::<usize>().ok().and_then(|i| i.checked_sub(1)).filter(|i| *i < sessions.len()) {
        let id = sessions[index].id;
        service.select(id).await?;
        let session = service.snapshot().current.ok_or("game not loaded")?;
        let text = format!(
            "{}\n\nNetwork fees paid: {} KAS\nSigned fee allowances: {} / {} KAS\n\nTimeout and settlement run automatically.\n\n  1  Increase fee allowance",
            sessions[index].label,
            menu::format_kas(session.paid_fees),
            menu::format_kas(session.authorized_fees),
            menu::format_kas(session.fee_budget)
        );
        match menu::queue_choice("GAME STATUS", &text, "1")? {
            Some('1') => {
                let menu::Prompt::Selected(value) = menu::wallet_text("Whole KAS to add to this game's fee budget (q: back): ")?
                else {
                    return Ok(());
                };
                let amount = value.parse::<u64>()?.checked_mul(100_000_000).filter(|v| *v > 0).ok_or("invalid budget amount")?;
                let budget = session.fee_budget.checked_add(amount).ok_or("fee budget overflow")?;
                if review(
                    "Increase game fee budget",
                    format!(
                        "Allow this game up to {} KAS in signed network fees. This changes the allowance; it does not send funds now.",
                        menu::format_kas(budget)
                    ),
                    false,
                )
                .await?
                {
                    service
                        .edit(move |b| {
                            let s = b.sessions.iter_mut().find(|s| s.id == id).ok_or("unknown game")?;
                            s.fee_budget = budget;
                            Ok(())
                        })
                        .await?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use node::invitations::{GameTerms, new_rendezvous_tag};
    #[test]
    fn contact_wait_signals_only_due_proofs_and_uses_observed_chain_time() {
        let contact = ContactWait { receiver: 1, deadline: 100, expiry: 210 };
        assert_eq!(contact.status(0, 159, true, false, false), None);
        assert_eq!(contact.status(0, 160, true, false, false).as_deref(), Some("Waiting for opponent’s proof · Timeout in ~5s"));
        assert_eq!(contact.status(1, 160, true, false, false).as_deref(), Some("Your contact proof is pending · Checking chain"));
        assert_eq!(contact.status(0, 210, false, false, false).as_deref(), Some("Connection lag · Checking chain"));
        assert_eq!(contact.status(0, 210, true, false, false).as_deref(), Some("Claiming opponent timeout…"));
        assert_eq!(contact.status(0, 210, true, false, true).as_deref(), Some("Timeout sent · Waiting for chain acceptance"));
    }
    fn key(n: u8) -> Vec<u8> {
        pubkey(&secp256k1::Keypair::from_secret_key(
            &secp256k1::Secp256k1::new(),
            &secp256k1::SecretKey::from_slice(&[n; 32]).unwrap(),
        ))
    }
    #[test]
    fn rematch_rechecks_both_players_all_settings_and_freshness() {
        let terms = GameTerms { speed: GameSpeed::Medium, ball_count: 2, win_score: 3, stake_kas: 201 };
        let old = new_rendezvous_tag(terms).unwrap();
        let expected = Expected { host: key(41), guest: key(42), terms, previous_tag: old };
        let mut fresh = new_rendezvous_tag(terms).unwrap();
        while fresh == old {
            fresh = new_rendezvous_tag(terms).unwrap();
        }
        let code = HostCode {
            network: [0; 8],
            tag: fresh,
            queue: Descriptor {
                owner: expected.host.clone(),
                id: Hash::from_bytes([1; 32]),
                last_game: Hash::from_bytes([2; 32]),
                generation: 1,
                reserves: [100_000_000; 3],
                side: 1,
            },
        };
        expected.validate(&code, &expected.guest).unwrap();
        let mut stale = code.clone();
        stale.tag = old;
        assert!(expected.validate(&stale, &expected.guest).is_err());
        assert!(expected.validate(&code, &key(43)).is_err());
        let mut stranger = code.clone();
        stranger.queue.owner = key(43);
        assert!(expected.validate(&stranger, &expected.guest).is_err());
        for changed in [
            GameTerms { stake_kas: 202, ..terms },
            GameTerms { ball_count: 1, ..terms },
            GameTerms { win_score: 4, ..terms },
            GameTerms { speed: GameSpeed::Fast, ..terms },
        ] {
            let mut code = code.clone();
            code.tag = new_rendezvous_tag(changed).unwrap();
            assert!(expected.validate(&code, &expected.guest).is_err());
        }
    }
}
