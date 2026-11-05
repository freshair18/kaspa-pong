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
    let ui = network_ui::Setup::new(enabled)?;
    ui.set_status(label);
    let Some(result) = ui.read(service.wait_ready()).await else { return Ok(None) };
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
async fn create_set(service: &Service, wager: u64, manual: bool, enabled: bool) -> Result<Option<Set>> {
    let Some(view) = ready(service, enabled, "Checking existing reusable queues…").await? else { return Ok(None) };
    let automatic = !manual && view.created_count == 0 && view.records.is_empty();
    let reserve = reusable::recommended_reserve(wager)?;
    let capital = wager.checked_add(reserve).ok_or("queue capital overflow")?;
    let mut nonce = [0u8; 32];
    getrandom::getrandom(&mut nonce).map_err(|_| "secure randomness unavailable")?;
    let nonce = Hash::from_bytes(nonce);
    let owner = service.player();
    let ui = network_ui::Setup::new(enabled)?;
    ui.set_status("Preparing reusable queues and their full network fee…");
    let Some(result) = ui
        .read(service.quote(vec![], move |b, coins, fee| {
            let (tx, states) = reusable::create_with_nonce(b, &owner, capital, reserve, coins, fee, nonce)?;
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
        "Queue balance: {} KAS\nWager backing: {} KAS\nRecoverable buffer: {} KAS\n\nThese queues are reused across games.\nClose an idle set to withdraw its balance.\n\n{}\n{}",
        menu::format_kas(capital),
        menu::format_kas(wager),
        menu::format_kas(reserve),
        fee_details(service, &quote)?,
        if automatic {
            "First set · uses your auto-approval setting."
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
        if view.created_count == 0 && view.records.is_empty() {
            return create_set(service, wager, false, enabled).await;
        }
        if !view.records.is_empty()
            && view.records.iter().all(|r| r.phase == node::queue_sets::Phase::Closed)
            && view.pending.is_empty()
        {
            return create_set(service, wager, true, enabled).await;
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
            Some('2') => return create_set(service, wager, true, enabled).await,
            _ => return Ok(None),
        }
    }
}
fn sets_frame(view: &View) -> String {
    if view.records.is_empty() {
        return if view.created_count > 0 {
            "Initial creation is pending or needs recovery; its exact transaction will be resumed.".into()
        } else {
            "No reusable queue sets yet.".into()
        };
    }
    view.records
        .iter()
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
                    "{}\n\nWager: {} KAS\nQueue top-up: {} KAS\nThe buffer remains in your reusable queues.\n{}",
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
    task: tokio::task::JoinHandle<std::result::Result<Quote<()>, String>>,
}
async fn play(service: &Service, id: Hash, json: bool) -> Result<Option<(network_ui::RematchRequest, Session, Option<HostCode>)>> {
    service.select(id).await?;
    let initial = service.snapshot().current.ok_or("game journal not loaded")?;
    let side = initial.side(&pubkey(&service.player()));
    let ui = network_ui::Frontend::new(interactive(json), true, side as i64, false, &service.config().network)?;
    let mut queued = VecDeque::new();
    let mut work: Option<Work> = None;
    let mut rematch_offer: Option<HostCode> = None;
    let mut epoch = service.snapshot().epoch;
    let mut submitted: Vec<(Hash, Vec<network_ui::Movement>)> = vec![];
    let mut accepted_before=std::collections::HashSet::new();
    let mut next_action = Instant::now();
    let mut status = String::new();
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
                    .find(|s| s.lanes[0].state.queue.owner == pubkey(&service.player()))
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
            if matches!(stage, QueueStage::Finished { .. }) {
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
            }
        }
        if work.as_ref().is_some_and(|w| w.task.is_finished()) {
            let w = work.take().unwrap();
            let quote = w.task.await.map_err(|e| e.to_string())?;
            if w.epoch == view.epoch && !ui.quit.load(Ordering::Relaxed) {
                match quote {
                    Ok(quote) => {
                        let mut valid = true;
                        if !w.batch.is_empty() {
                            queued.extend(ui.input.try_iter());
                            let current = ui.movement_batch(&queued);
                            valid = w.batch.iter().all(|m| current.iter().any(|n| n.pressed == m.pressed));
                        }
                        if valid {
                            match service.authorize(quote, w.action.label(), Effect::Game { session: id }, vec![], false, None).await {
                                Ok(tx) => {
                                    for movement in &w.batch {
                                        ui.submitted_movement(movement, tx);
                                        queued.retain(|m| m.pressed != movement.pressed);
                                    }
                                    if !w.batch.is_empty() {
                                        submitted.push((tx, w.batch));
                                    }
                                    status.clear();
                                }
                                Err(e) => status = e.to_string(),
                            }
                        }
                    }
                    Err(e) => status = e,
                }
            }
            next_action = Instant::now() + Duration::from_millis(100);
        }
        let fresh = view.ready && view.observed.elapsed() < network_ui::SNAPSHOT_LIFETIME;
        if work.is_none() && fresh && Instant::now() >= next_action {
            let mut batch = vec![];
            let action = match session.stage.as_ref() {
                Some(QueueStage::Lobby { .. })
                    if side == 1 && !view.pending.iter().any(|p| p.game == Some(id)) && !session.cancel_requested =>
                {
                    Some(GameAction::Start)
                }
                Some(QueueStage::Live(m)) if !m.court.state.world.ended() => {
                    let (receiver, until) = next_contact(&m.snapshot())?;
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
                    } else if !pending_court && lanes_free && count>=8 && prefix>m.sets[side].lanes[0].state.queue.through
                        && checkpoint_plan(&m.snapshot(),side,prefix).is_ok() {
                        // Match the previous client's early compaction policy;
                        // waiting for contact can otherwise fill all 12 slots.
                        Some(GameAction::Resolve{side,until:prefix})
                    } else {
                        // A contact resolve need not block an independent
                        // unreserved lane (especially the other player's).
                        batch = ui.movement_batch(&queued);
                        let direction = batch.iter().map(|m| m.direction).sum::<i64>().clamp(-MAX_MOVE_STEPS, MAX_MOVE_STEPS);
                        if direction != 0 {
                            (0..3)
                                .filter(|i| {
                                    m.sets[side].lanes[*i].state.queue.count < MAX_EVENTS
                                        && !view.pending.iter().any(|p| p.inputs.contains(&m.sets[side].lanes[*i].outpoint))
                                })
                                .min_by_key(|i| m.sets[side].lanes[*i].state.queue.count)
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
                let snapshot = session.clone();
                let planned = action.clone();
                work = Some(Work {
                    action,
                    epoch: view.epoch,
                    batch,
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
            Some(QueueStage::Finished { .. }) => "Settlement accepted. Winnings remain in your reusable queues.",
            _ => "Following accepted game state.",
        };
        let pending = view.pending.iter().filter(|p| p.game == Some(id)).collect::<Vec<_>>();
        ui.observe_clock(network_ui::ClockSample { epoch: view.clock.epoch, daa: view.clock.daa, observed: view.clock.observed });
        ui.update(network_ui::Display {
            stage,
            daa: view.daa,
            ready: fresh,
            observed: view.observed,
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
            status: if status.is_empty() { format!("{hint}\n{}", view.status) } else { format!("{hint}\n{status}") },
            last_reorg: None,
            recent_transactions: view.recent_transactions.clone(),
            settlement_hint: matches!(session.stage, Some(QueueStage::Collecting { .. } | QueueStage::Finished { .. }))
                .then(|| hint.into()),
            lobby_hint: Some(hint.into()),
        });
        if json {
            println!("{}", serde_json::json!({"game":id,"daa":view.daa,"ready":fresh,"status":hint,"stage":session.stage}));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
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

pub async fn wallet(
    c: &Config,
    keyfile: Option<&Path>,
    connection: &node::rpc::startup::Connection,
    settings: menu::GameSettings,
) -> Result<()> {
    let service = service(c, keyfile, connection)?;
    loop {
        let view = service.snapshot();
        let text =
            format!("{}\n\nEnter a set number to manage it.\nc  Create a new set (manual approval)\nq  Back", sets_frame(&view));
        let menu::Prompt::Selected(value) = menu::wallet_text(&format!("Reusable queues\n{text}\n\nChoose: "))? else { return Ok(()) };
        if value.eq_ignore_ascii_case("c") {
            let _ = create_set(&service, u64::from(settings.stake_kas) * 100_000_000, true, interactive(false)).await?;
            continue;
        }
        let Some(index) = value.parse::<usize>().ok().and_then(|i| i.checked_sub(1)).filter(|i| *i < view.records.len()) else {
            continue;
        };
        let set = view.records[index].set.clone();
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
        let legacy=view.records[index].phase==node::queue_sets::Phase::Legacy;
        if !legacy && !Service::available(&view).iter().any(|s| s.id().ok() == set.id().ok()) {
            let _ = menu::queue_choice(
                "Queue set unavailable",
                "This set is offered, active, settling or closed. Check Game status or wait for automatic settlement. Previous-contract active, offered or pending sets must first finish or be cancelled with the previous client.",
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
                let old_artifact=if legacy{Some(reusable::previous_artifact()?)}else{None};
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
    let service = service(c, wallet, connection)?;
    let Some(view) = ready(&service, interactive(false), "Checking available wallet coins and idle queues…").await? else { return Ok(()) };
    let owner = service.player();
    let sets = consolidation::eligible_sets(&view);
    let include_queues = if sets.is_empty() { false } else {
        let total=sets.iter().map(Set::balance).sum::<u64>();
        let body=format!("1  Consolidate ordinary wallet coins only\n2  Include {} idle queue sets ({} KAS)\n\nIncluding queues closes those sets and returns their balances to the wallet.\nActive, reserved, offered and pending sets are excluded.\nEvery transaction shows its fee before manual approval.",sets.len(),menu::format_kas(total));
        match menu::queue_choice("CONSOLIDATE WALLET",&body,"12")? {Some('1')=>false,Some('2')=>true,_=>return Ok(())}
    };
    let mut closed_any=false;
    if include_queues {
        let mut groups:Vec<(argent_runtime::Artifact,Vec<Set>)>=vec![];
        for set in sets {
            let artifact=node::queue_sets::artifact_for(&set)?;
            if let Some((_,sets))=groups.iter_mut().find(|(a,sets)|a.id==artifact.id && sets.len()<4) {sets.push(set);}else{groups.push((artifact,vec![set]));}
        }
        for (artifact,sets) in groups {
            let total=sets.iter().map(Set::balance).sum::<u64>();let count=sets.len();
            let sources=sets.iter().flat_map(cells).collect::<Vec<_>>();let closes=sets.iter().map(Set::id).collect::<Result<Vec<_>>>()?;
            let quote=service.quote(sources,move|_b,_coins,fee|{let b=argent_runtime::TxBuilder::new(&artifact)?;Ok((consolidation::close_group(&b,&sets,&owner,fee)?,()))}).await?;
            let body=format!("Close {count} idle queue set(s) holding {} KAS.\nThese sets will no longer be reusable.\n\nReturn to ordinary wallet: {} KAS\n{}\n\nAny remaining wallet merge is reviewed separately after acceptance.",menu::format_kas(total),menu::format_kas(quote.transaction.outputs[0].value),fee_details(&service,&quote)?);
            if !review("CLOSE IDLE QUEUES",body,false).await?{return Ok(());}
            let id=service.authorize(quote,"close queues for manual consolidation",Effect::Wallet{closes},vec![],false,None).await?;
            if !wait_tx(&service,id,interactive(false),"Idle queue closure submitted.").await?{return Ok(());}
            closed_any=true;
        }
    }
    if consolidation::ordinary_count(&service).await?<2 {
        let message=if closed_any {"Idle queue balances returned to the wallet. There is already at most one eligible wallet output, so no further consolidation is needed."} else {"Nothing to consolidate. There are fewer than two mature, unreserved ordinary wallet outputs. No transaction was submitted."};
        let _=menu::queue_choice("CONSOLIDATION",message,"")?;return Ok(());
    }
    let quote = match service.quote_all(vec![], move |b, coins, fee| {
        if coins.len()<2{return Err(consolidation::NOT_NEEDED.into());}
        Ok((build(b,&reusable::fund(argent_runtime::TxContext::new(),coins,&owner,fee)?)?,coins.len()))
    }).await {
        Ok(quote)=>quote,
        Err(error) if error.to_string().contains(consolidation::NOT_NEEDED)=>{let _=menu::queue_choice("CONSOLIDATION","Nothing to consolidate. Fewer than two eligible wallet outputs remain. No merge was submitted.","")?;return Ok(());},
        Err(error)=>return Err(error),
    };
    let body=format!("Combine {} available ordinary wallet outputs into one.\nPending inputs are excluded.\n\nReturn to wallet: {} KAS\n{}",quote.data,menu::format_kas(quote.transaction.outputs[0].value),fee_details(&service,&quote)?);
    if !review("Consolidate wallet coins",body,false).await?{return Ok(());}
    let id=service.authorize(quote,"manual consolidation",Effect::Wallet{closes:vec![]},vec![],false,None).await?;
    let _=wait_tx(&service,id,interactive(false),"Manual consolidation submitted.").await?;Ok(())
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
