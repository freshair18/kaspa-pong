use super::*;
use kaspa_consensus_core::tx::{TransactionOutpoint, UtxoEntry};
use secp256k1::{Secp256k1, SecretKey};
fn key(n: u8) -> Keypair {
    Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
}
fn coin(n: u8, owner: &Keypair, value: u64) -> Funding {
    Funding {
        outpoint: TransactionOutpoint::new(Hash::from_bytes([n; 32]), 0),
        utxo: UtxoEntry::new(value, p2pk(&pubkey(owner)), 1, false, None),
    }
}
fn created(b: &TxBuilder<'_>, n: u8, capital: u64) -> Set {
    let owner = key(n);
    let coins = [coin(n + 20, &owner, 200_000_000), coin(n + 30, &owner, 10_000_000_000)];
    let (tx, states) = create(b, &owner, capital, MIN_RESERVE, &coins, 100_000).unwrap();
    accepted_set(&tx, [0, 1, 2], states, 100)
}
fn make_offer(guest: &Set, host: &Set, swapped: bool) -> Offer {
    let mut lobby = Lobby {
        left: guest.lanes[0].state.queue.owner.clone(),
        right: host.lanes[0].state.queue.owner.clone(),
        refund: guest.lanes[0].state.queue.owner.clone(),
        control_value: 0,
        ball_tick_daa: 7,
        stake_per_player: DEFAULT_STAKE,
        ball_count: 1,
        win_score: 3,
    };
    if swapped {
        lobby.ball_count = 2;
    }
    Offer { lobby, pools: [guest.id().unwrap(), host.id().unwrap()], generations: [guest.generation(), host.generation()] }
}
#[test]
fn creation_accepts_fragmented_funds_without_a_merge_transaction() {
    let a = artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let set = created(&b, 1, 800_000_000);
    set.validate(&b).unwrap();
    assert!(set.idle());
    assert_eq!(set.balance(), 800_000_000);
    assert!(set.lanes.iter().all(|c| c.utxo.covenant_id == Some(set.id().unwrap())));
}
#[test]
fn reserves_scale_with_the_actual_wager_and_worst_loss_layout() {
    let small = recommended_reserve(500_000_000).unwrap();
    let large = recommended_reserve(25_400_000_000).unwrap();
    assert_eq!(small, 300_000_000);
    assert_eq!(recommended_reserve(900_000_000).unwrap(), 275_000_000);
    assert_eq!(recommended_reserve(2_900_000_000).unwrap(), 250_000_000);
    assert_eq!(large, 250_000_000);
    for wager in 5..=254 {
        assert!(recommended_reserve(wager * 100_000_000).is_ok());
    }
    eprintln!("reserve: 5 KAS wager={} sompi; 254 KAS wager={} sompi", small, large);
}
#[test]
fn idle_owner_operations_preserve_identity_and_require_the_owner() {
    let a = artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let set = created(&b, 1, 800_000_000);
    assert!(close(&b, &set, &key(2), &[], 100_000).is_err());
    let tx = resize(&b, &set, &key(1), 1_000_000_000, &[coin(70, &key(1), 500_000_000)], 100_000).unwrap();
    let next = accepted_set(&tx, [0, 1, 2], std::array::from_fn(|i| set.lanes[i].state.clone()), 110);
    assert_eq!(next.balance(), 1_000_000_000);
    assert_eq!(next.id().unwrap(), set.id().unwrap());
    let tx = resize(&b, &next, &key(1), MIN_RESERVE, &[coin(71, &key(1), 500_000_000)], 100_000).unwrap();
    assert_eq!(tx.outputs[3].value, 800_000_000);
    let tx = close(&b, &next, &key(1), &[coin(72, &key(1), 500_000_000)], 100_000).unwrap();
    assert_eq!(tx.outputs.len(), 1);
    assert_eq!(tx.outputs[0].value, 1_499_900_000);
    let tx = resize(&b, &next, &key(1), MIN_RESERVE, &[], 100_000).unwrap();
    assert_eq!(tx.outputs[3].value, 799_900_000, "withdrawal can pay its own fee without another coin");
}
#[test]
fn new_creation_nonces_produce_distinct_native_identities_from_the_same_funding() {
    let a = artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let owner = key(1);
    let funding = coin(78, &owner, 10_000_000_000);
    let (first, states) =
        create_with_nonce(&b, &owner, 800_000_000, MIN_RESERVE, &[funding.clone()], 100_000, Hash::from_bytes([1; 32])).unwrap();
    let (second, next) =
        create_with_nonce(&b, &owner, 800_000_000, MIN_RESERVE, &[funding], 100_000, Hash::from_bytes([2; 32])).unwrap();
    assert_ne!(accepted_set(&first, [0, 1, 2], states, 100).id().unwrap(), accepted_set(&second, [0, 1, 2], next, 100).id().unwrap());
}

#[test]
#[cfg(feature = "node")]
fn full_mass_pricing_covers_payloads_small_and_large_wagers_and_every_lifecycle_tx() {
    use crate::node::{
        Config,
        invitations::{GameTerms, new_rendezvous_tag},
        queue_codes::{Descriptor, HostCode, Reservation},
        queue_sets::{funded_by_mass, funded_transaction},
    };
    let a = artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let guest = key(1);
    let host = key(2);
    let config = Config { game_fee_cap: true, fee_sompi: 50_000_000, ..Default::default() };
    let price = |name: &str, owner: &Keypair, make: &mut dyn FnMut(&[Funding], u64) -> Result<Transaction>| {
        let funding = coin(95, owner, 1_000_000_000_000);
        let (tx, (), _, fee) = funded_transaction(&config, &[funding], |coins, fee| Ok((make(coins, fee)?, ()))).unwrap();
        let mass = transaction_mass(&tx).unwrap();
        assert!(mass.priority >= mass.storage);
        assert!(fee >= crate::node::client::relay_fee_at_rate(&tx, &config, crate::node::fee_policy::selected_rate(&config)).unwrap());
        eprintln!(
            "{name}: compute={} transient={} storage={} full={} fee={}",
            mass.compute, mass.transient, mass.storage, mass.priority, fee
        );
        tx
    };
    for wager in [5u8, 254] {
        let stake = u64::from(wager) * 100_000_000;
        let reserve = DEFAULT_QUEUE_RESERVE;
        let capital = stake + reserve;
        let mut states = None;
        let tx = price("create", &guest, &mut |coins, fee| {
            let (tx, s) = create_with_nonce(&b, &guest, capital, reserve, coins, fee, Hash::from_bytes([1; 32]))?;
            states = Some(s);
            Ok(tx)
        });
        let g = accepted_set(&tx, [0, 1, 2], states.unwrap(), 100);
        let mut states = None;
        let tx = price("create host", &host, &mut |coins, fee| {
            let (tx, s) = create_with_nonce(&b, &host, capital, reserve, coins, fee, Hash::from_bytes([2; 32]))?;
            states = Some(s);
            Ok(tx)
        });
        let h = accepted_set(&tx, [0, 1, 2], states.unwrap(), 100);
        let code = HostCode::new(
            "testnet-10",
            new_rendezvous_tag(GameTerms { speed: GameSpeed::Medium, ball_count: 1, win_score: 3, stake_kas: wager }).unwrap(),
            &h,
        )
        .unwrap();
        let r = Reservation { host: code, guest: Descriptor::from_set(&g).unwrap(), capital };
        let offer = r.offer().unwrap();
        let payload = r.payload().unwrap();
        let tx = price("reserve", &guest, &mut |coins, fee| {
            reserve_with_payload(&b, &g, &offer, &guest, capital, coins, fee, payload.clone())
        });
        let (lobby, g) = reserved(&tx, &g, offer, capital, 110);
        price("cancel", &guest, &mut |coins, fee| Ok(cancel(&b, &lobby, &g, &guest, coins, fee)?.0));
        let mut result = None;
        let tx = price("start", &host, &mut |coins, fee| {
            let (tx, game, states) = start(&b, &lobby, &g, &h, &host, capital, coins, fee)?;
            result = Some((game, states));
            Ok(tx)
        });
        let (game, states) = result.unwrap();
        let mut m = accepted_match(&tx, game, states, &g, 120);
        let mut result = None;
        let tx = price("command", &guest, &mut |coins, fee| {
            let (tx, s) = command(&b, &m.sets[0].lanes[0], 1, &guest, coins, fee)?;
            result = Some(s);
            Ok(tx)
        });
        let state = result.unwrap();
        m.sets[0].lanes[0] = accepted_cell(&tx, 0, 130, state);
        let (side, until) = next_contact(&m.snapshot()).unwrap();
        let payer = if side == 0 { guest } else { host };
        let mut result = None;
        let tx = price("resolve", &payer, &mut |coins, fee| {
            let (tx, game, states) = checkpoint(&b, &m, side, until, &payer, coins, fee)?;
            result = Some((game, states));
            Ok(tx)
        });
        let (game, states) = result.unwrap();
        assert!(!game.world.ended());
        assert!(tx.payload.is_empty());
        m.court = accepted_cell(&tx, 0, 200, game);
        m.sets[side] = accepted_set(&tx, [1, 2, 3], states, 200);
        let mut result = None;
        let tx = price("timeout", &guest, &mut |coins, fee| {
            let (tx, r) = timeout(&b, &m, &guest, coins, fee)?;
            result = Some(r);
            Ok(tx)
        });
        let r = accepted_cell(&tx, 0, 500, result.unwrap());
        let mut result = None;
        let tx = price("collect", &guest, &mut |coins, fee| {
            let (tx, states) = collect(&b, &r, &m.sets, &guest, coins, fee)?;
            result = Some(states);
            Ok(tx)
        });
        if wager == 5 {
            let large = coin(95, &guest, 1_000_000_000_000);
            let small = coin(96, &guest, 200_000_000);
            let sources = std::iter::once((r.outpoint, r.utxo.clone()))
                .chain(m.sets.iter().flat_map(|set| set.lanes.iter().map(|lane| (lane.outpoint, lane.utxo.clone()))))
                .collect::<Vec<_>>();
            let (smaller, (), chosen, fee) = funded_by_mass(&config, &[large, small.clone()], &sources, 0, |coins, fee| {
                Ok((collect(&b, &r, &m.sets, &guest, coins, fee)?.0, ()))
            }).unwrap();
            assert_eq!(chosen.len(), 1);
            assert_eq!(chosen[0].outpoint, small.outpoint);
            assert!(transaction_mass(&smaller).unwrap().priority < transaction_mass(&tx).unwrap().priority);
            eprintln!("collection with 2 KAS fee coin: {:?}, fee={fee}", transaction_mass(&smaller).unwrap());
        }
        assert_eq!(tx.outputs.len(), 7);
        let winner = r.state.collection.finish.winner as usize;
        let owner = if winner == 0 { guest } else { host };
        let idle = accepted_set(&tx, if winner == 0 { [0, 1, 2] } else { [3, 4, 5] }, result.unwrap()[winner].clone(), 600);
        if idle.balance() > idle.reserve() {
            price("withdraw", &owner, &mut |coins, fee| resize(&b, &idle, &owner, idle.reserve(), coins, fee));
        }
        price("top up", &owner, &mut |coins, fee| resize(&b, &idle, &owner, idle.balance() + 100_000_000, coins, fee));
        price("close", &owner, &mut |coins, fee| close(&b, &idle, &owner, coins, fee));
    }
}
#[test]
#[cfg(feature = "node")]
fn accepted_observer_replays_start_and_loser_initiated_timeout() {
    use crate::node::{queue_tracker::QueueStage, tracker::AcceptedTx};
    let a = artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let guest = created(&b, 1, 800_000_000);
    let host = created(&b, 2, 800_000_000);
    let terms = make_offer(&guest, &host, false);
    let funding = coin(80, &key(1), 10_000_000_000);
    let tx = reserve(&b, &guest, &terms, &key(1), guest.balance(), &[funding], 100_000).unwrap();
    let (lobby, guest) = reserved(&tx, &guest, terms, guest.balance(), 110);
    let mut observed = QueueStage::Lobby { lobby: lobby.clone(), sets: [guest.clone(), host.clone()] };
    let funding = coin(81, &key(2), 10_000_000_000);
    let (tx, game, states) = start(&b, &lobby, &guest, &host, &key(2), host.balance(), &[funding.clone()], 100_000).unwrap();
    let current = accepted_match(&tx, game, states, &guest, 120);
    fn replay(stage: &mut QueueStage, a: &Artifact, b: &TxBuilder<'_>, tx: Transaction, funding: Funding, daa: u64) {
        let sources = stage.cells();
        let entries = tx
            .inputs
            .iter()
            .map(|i| {
                sources.iter().find(|(o, _)| *o == i.previous_outpoint).map(|(_, u)| u.clone()).unwrap_or_else(|| {
                    assert_eq!(i.previous_outpoint, funding.outpoint);
                    funding.utxo.clone()
                })
            })
            .collect();
        assert!(stage.apply(a, b, &AcceptedTx { tx, inputs: entries }, daa).unwrap());
    }
    replay(&mut observed, &a, &b, tx, funding, 120);
    let side = next_contact(&current.snapshot()).unwrap().0;
    let payer = key((side + 1) as u8);
    let funding = coin(82, &payer, 10_000_000_000);
    let (tx, r) = timeout(&b, &current, &payer, &[funding.clone()], 100_000).unwrap();
    assert_eq!(r.collection.finish.winner, (1 - side) as i64);
    let result = accepted_cell(&tx, 0, 400, r);
    replay(&mut observed, &a, &b, tx, funding, 400);
    let funding = coin(83, &payer, 10_000_000_000);
    let (tx, _) = collect(&b, &result, &current.sets, &payer, &[funding.clone()], 100_000).unwrap();
    replay(&mut observed, &a, &b, tx, funding, 410);
    let QueueStage::Finished { sets, .. } = observed else { panic!("observer did not settle") };
    assert!(sets.iter().all(Set::idle));
    assert!(sets.iter().all(|s| s.generation() == 1));
}
#[test]
fn movements_and_score_settlement_match_the_existing_physics() {
    let a = artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let guest = created(&b, 1, 1_000_000_000);
    let host = created(&b, 2, 1_000_000_000);
    let floor = recommended_reserve(DEFAULT_STAKE as u64).unwrap();
    assert_eq!(required_capital(&guest, DEFAULT_STAKE as u64).unwrap(), 800_000_000);
    let mut terms = make_offer(&guest, &host, false);
    terms.lobby.win_score = 1;
    let tx = reserve(&b, &guest, &terms, &key(1), guest.balance(), &[coin(84, &key(1), 10_000_000_000)], 100_000).unwrap();
    let (lobby, guest) = reserved(&tx, &guest, terms, guest.balance(), 110);
    let (refund, _) = cancel(&b, &lobby, &guest, &key(1), &[], 7_000_000).unwrap();
    assert_eq!(refund.inputs.len(), 4);
    assert_eq!(refund.outputs.iter().map(|out| out.value).sum::<u64>(), 993_000_000);
    let (tx, game, states) =
        start(&b, &lobby, &guest, &host, &key(2), host.balance(), &[coin(85, &key(2), 10_000_000_000)], 100_000).unwrap();
    let mut current = accepted_match(&tx, game, states, &guest, 120);
    let (side, until) = next_contact(&current.snapshot()).unwrap();
    let owner = key(side as u8 + 1);
    let lane_refs = current.sets[side].lanes.each_ref();
    for first_lane in 0..lane_refs.len() {
        let (amounts, wallet_spend) = fund_lanes(lane_refs, &[], 100_000, floor, DEFAULT_QUEUE_RESERVE, first_lane).unwrap();
        assert_eq!(wallet_spend, 0);
        assert_eq!(amounts, std::array::from_fn(|i| lane_refs[i].utxo.amount - if i == first_lane { 100_000 } else { 0 }));
    }
    let last_lane_capacity = lane_fee_capacity(lane_refs[2], floor);
    let (amounts, _) =
        fund_lanes(lane_refs, &[], last_lane_capacity + 100_000, floor, DEFAULT_QUEUE_RESERVE, 2).unwrap();
    assert_eq!(amounts, [
        lane_refs[0].utxo.amount - 100_000,
        lane_refs[1].utxo.amount,
        lane_refs[2].utxo.amount - last_lane_capacity,
    ]);
    let projection = project(&current.snapshot(), until as u64 - 1).unwrap();
    let incoming = projection.world.ball_y;
    let direction = if incoming < 16 { 27 } else { -27 };
    let lane = &current.sets[side].lanes[0];
    let capacity = lane_fee_capacity(lane, floor);
    assert!(command_with_reserve_target(&b, lane, direction, &owner, &[], capacity + 1_000_000, DEFAULT_QUEUE_RESERVE).is_err());
    let (fallback, drained) =
        command_with_reserve_target(&b, lane, direction, &owner, &[coin(86, &owner, 1_000_000)], capacity + 1_000_000, DEFAULT_QUEUE_RESERVE).unwrap();
    assert_eq!(fallback.inputs.len(), 2);
    assert_eq!(fallback.outputs[0].value, part(floor, 0) + lane.state.exposure);
    let drained = accepted_cell(&fallback, 0, until as u64 - 3, drained);
    let (partial, _) =
        command_with_reserve_target(&b, &drained, direction, &owner, &[coin(87, &owner, 20_000_000)], 1_000_000, DEFAULT_QUEUE_RESERVE).unwrap();
    assert_eq!((partial.inputs.len(), partial.outputs.len()), (2, 1));
    assert_eq!(partial.outputs[0].value, drained.utxo.amount + 19_000_000);
    let (refilled, _) =
        command_with_reserve_target(&b, &drained, direction, &owner, &[coin(88, &owner, 200_000_000)], 1_000_000, DEFAULT_QUEUE_RESERVE).unwrap();
    assert_eq!(refilled.outputs[0].value, part(DEFAULT_QUEUE_RESERVE, 0) + drained.state.exposure);
    let (tx, next) = command_with_reserve_target(&b, lane, direction, &owner, &[], 600_000, DEFAULT_QUEUE_RESERVE).unwrap();
    assert_eq!((tx.inputs.len(), tx.outputs.len()), (1, 1));
    assert_eq!(tx.outputs[0].value, lane.utxo.amount - 600_000);
    let move_mass = transaction_mass(&tx).unwrap();
    assert_eq!(move_mass.priority, move_mass.relay);
    eprintln!("lane-funded move: {move_mass:?}");
    let mut depleted = current.clone();
    depleted.sets[side].lanes[0] = drained;
    let (refilled, _, lanes) = checkpoint_with_reserve_target(
        &b, &depleted, side, until, &owner, &[coin(89, &owner, 200_000_000)], 6_000_000, DEFAULT_QUEUE_RESERVE,
    ).unwrap();
    assert_eq!(refilled.inputs.len(), 5);
    assert_eq!(refilled.outputs[1].value, part(DEFAULT_QUEUE_RESERVE, 0) + lanes[0].exposure);
    current.sets[side].lanes[0] = accepted_cell(&tx, 0, until as u64 - 1, next);
    let contact_fee = lane_fee_capacity(&current.sets[side].lanes[0], floor) + 1_000_000;
    let (tx, game, states) =
        checkpoint_with_reserve_target(&b, &current, side, until, &owner, &[], contact_fee, DEFAULT_QUEUE_RESERVE).unwrap();
    assert_eq!((tx.inputs.len(), tx.outputs.len()), (4, 4));
    assert_eq!(tx.outputs[0].value, current.court.utxo.amount);
    assert_eq!(tx.outputs[1].value, part(floor, 0) + states[0].exposure);
    assert!(tx.outputs[2].value < current.sets[side].lanes[1].utxo.amount);
    assert!(game.world.ended());
    assert!(tx.payload.is_empty());
    eprintln!("deciding contact: {:?}", transaction_mass(&tx).unwrap());
    let r = game.result_state(game.world.result());
    #[cfg(feature = "node")]
    {
        use crate::node::{audit::GameEvent, queue_tracker::QueueStage, tracker::AcceptedTx};
        let mut stage = QueueStage::Live(current.clone());
        let before = stage.display();
        let inputs = std::iter::once(current.court.utxo.clone())
            .chain(current.sets[side].lanes.iter().map(|lane| lane.utxo.clone()))
            .collect();
        let accepted = AcceptedTx { tx: tx.clone(), inputs };
        assert!(stage.apply(&a, &b, &accepted, until as u64).unwrap());
        let QueueStage::Collecting { result, .. } = &stage else { panic!("deciding contact did not start collection") };
        assert_eq!(result.state.collection.finish, r.collection.finish);
        assert_eq!(GameEvent::between(&before, &stage.display(), tx.id(), until as u64).action, "finish");
    }
    let result = accepted_cell(&tx, 0, until as u64, r);
    current.sets[side] = accepted_set(&tx, [1, 2, 3], states, until as u64);
    let winner = key((2 - side) as u8);
    let (tx, _) = collect(&b, &result, &current.sets, &winner, &[coin(89, &winner, 10_000_000_000)], 100_000).unwrap();
    eprintln!("score collection: {:?}", transaction_mass(&tx).unwrap());
}

#[test]
fn lazy_eight_ball_flights_match_eager_physics_and_preserve_separated_phases() {
    let artifact = artifact().unwrap();
    let builder = TxBuilder::new(&artifact).unwrap();
    let guest = created(&builder, 1, 800_000_000);
    let host = created(&builder, 2, 800_000_000);
    let mut terms = make_offer(&guest, &host, false);
    terms.lobby.ball_count = MAX_BALLS as i64;
    terms.lobby.win_score = 15;
    let reserve_tx = reserve(
        &builder, &guest, &terms, &key(1), guest.balance(), &[coin(100, &key(1), 10_000_000_000)], 100_000,
    ).unwrap();
    let (lobby, guest) = reserved(&reserve_tx, &guest, terms, guest.balance(), 110);
    let (tx, game, states) = start(
        &builder, &lobby, &guest, &host, &key(2), host.balance(), &[coin(101, &key(2), 10_000_000_000)], 100_000,
    ).unwrap();
    let opening = accepted_match(&tx, game, states, &guest, 120);
    let funding = coin(120, &key(1), 200_000_000);
    let mut lane = opening.sets[0].lanes[0].clone();
    for (index, direction) in [3, -4, 27, -27].into_iter().enumerate() {
        let accepted = 121 + index as u64;
        let (tx, state) = command(&builder, &lane, direction, &key(1), std::slice::from_ref(&funding), 100_000).unwrap();
        assert_eq!(state.queue.count, index + 1);
        assert_eq!(state.queue.commands[index], direction);
        assert_eq!(state.queue.times[index], -1);
        if index > 0 {
            assert_eq!(state.queue.times[index - 1], accepted as i64 - 1);
        }
        if index == 1 {
            let owner = key(1);
            let pool = lane.utxo.covenant_id.unwrap();
            let build_successor = |next: &Lane| {
                let ctx = TxContext::new()
                    .actor_input(
                        lane.state.actor(), lane.state.state(),
                        EntryCall::new("command").args_with(|tx, i| args![direction, sign(tx, i, &owner)]),
                        lane.outpoint, lane.utxo.clone(), 0,
                    )
                    .actor_output(next.actor(), next.state(), CovenantBinding::new(0, pool), tx.outputs[0].value);
                checked_build(&builder, &fund(ctx, std::slice::from_ref(&funding), &owner, 100_000)?)
            };
            assert!(build_successor(&state).is_ok());
            let mut backdated = state.clone();
            backdated.queue.times[0] -= 1;
            assert!(build_successor(&backdated).is_err(), "the signer cannot backdate a queued move");
        }
        lane = accepted_cell(&tx, 0, accepted, state);
    }
    assert!(command(&builder, &lane, 1, &key(1), &[funding], 100_000).is_err());
    let event = next_ball_contact(&opening.snapshot()).unwrap();
    let (_, receiver, deadline) = event;
    let owner = key(receiver as u8 + 1);
    let mut prefix_only = opening.clone();
    for (funding_id, offset, direction) in [(118, 5, 3), (119, 2, -4)] {
        let lane = &mut prefix_only.sets[receiver].lanes[0];
        let (tx, state) = command_with_reserve_target(
            &builder, lane, direction, &owner, &[coin(funding_id, &owner, 200_000_000)], 100_000, DEFAULT_QUEUE_RESERVE,
        ).unwrap();
        *lane = accepted_cell(&tx, 0, (deadline - offset) as u64, state);
    }
    let (tx, game, states) = checkpoint(
        &builder, &prefix_only, receiver, deadline - 3, &owner, &[coin(116, &owner, 10_000_000_000)], 100_000,
    ).unwrap();
    assert_eq!(game.pending_contact, Some(event));
    assert_eq!(game.flights, opening.court.state.flights);
    assert_eq!(states[0].queue.count, 1);
    assert_eq!(states[0].queue.commands, [-4, 0, 0, 0]);
    prefix_only.court = accepted_cell(&tx, 0, (deadline - 1) as u64, game);
    prefix_only.sets[receiver] = accepted_set(&tx, [1, 2, 3], states, (deadline - 1) as u64);
    let (_, timed_out) = timeout(&builder, &prefix_only, &owner, &[coin(117, &owner, 10_000_000_000)], 100_000).unwrap();
    assert_eq!(timed_out.collection.finish.winner, 1 - receiver as i64);

    let advance = |current: &mut Match, step: u8| {
        let snapshot = current.snapshot();
        let (index, side, deadline) = next_ball_contact(&snapshot).unwrap();
        let eager = checkpoint_plan(&snapshot, side, deadline).unwrap().world;
        let before = current.court.state.flights;
        let owner = key(side as u8 + 1);
        let (tx, game, states) = checkpoint(
            &builder, current, side, deadline, &owner, &[coin(step, &owner, 10_000_000_000)], 100_000,
        ).unwrap();
        assert_eq!(game.world, eager);
        let chosen = game.flights[index].unwrap();
        assert_eq!(chosen.origin, deadline);
        assert_eq!(chosen.ball, eager.ball(index));
        for other in (0..MAX_BALLS).filter(|&other| index != other) {
            assert_eq!(game.flights[other], before[other]);
        }
        current.court = accepted_cell(&tx, 0, deadline as u64, game);
        current.sets[side] = accepted_set(&tx, [1, 2, 3], states, deadline as u64);
        (index, deadline, before[index].is_some())
    };

    let mut live = opening.clone();
    let mut initialized = [false; MAX_BALLS];
    let mut revisited = false;
    let mut previous_deadline = 0;
    for step in 102..116 {
        let (index, deadline, existed) = advance(&mut live, step);
        assert!(deadline > previous_deadline);
        previous_deadline = deadline;
        initialized[index] = true;
        revisited |= existed;
    }
    assert!(initialized.into_iter().all(|value| value));
    assert!(revisited);

}
#[test]
fn host_cannot_burn_release_or_forge_reserved_guest_queues_during_start() {
    let a = artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let guest = created(&b, 1, 800_000_000);
    let host = created(&b, 2, 800_000_000);
    let terms = make_offer(&guest, &host, false);
    let tx = reserve(&b, &guest, &terms, &key(1), guest.balance(), &[coin(91, &key(1), 10_000_000_000)], 100_000).unwrap();
    let (lobby, guest) = reserved(&tx, &guest, terms, guest.balance(), 110);
    for attack in 0..3 {
        let (ctx, _, _) =
            start_context(&b, &lobby, &guest, &host, &key(2), host.balance(), &[coin(92, &key(2), 10_000_000_000)], 100_000).unwrap();
        let ctx = if attack == 0 {
            let c = &guest.lanes[1];
            ctx.actor_input("Lane", c.state.state(),
                EntryCall::new("coordinate").args(args![c.state.state_data()]),
                c.outpoint, c.utxo.clone(), 0)
        } else {
            let mut states = std::array::from_fn(|i| guest.lanes[i].state.clone());
            if attack == 1 {
                for s in &mut states {
                    *s = s.reset(s.game, s.generation + 1, 0, 0, false);
                }
            } else {
                states[0].queue.y = 27;
            }
            let (ctx, primary) = active_inputs(ctx, &guest, &states).unwrap();
            set_outputs(ctx, &guest, &states, std::array::from_fn(|i| guest.lanes[i].utxo.amount), primary).unwrap()
        };
        assert!(b.build(&ctx).is_err(), "host attack {attack} was accepted");
    }
}
#[test]
fn cancel_returns_escrow_and_preserves_the_set() {
    let a = artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let guest = created(&b, 1, 800_000_000);
    let host = created(&b, 2, 800_000_000);
    let offer = make_offer(&guest, &host, false);
    let coins = [coin(60, &key(1), 10_000_000_000)];
    let tx = reserve(&b, &guest, &offer, &key(1), guest.balance(), &coins, 100_000).unwrap();
    let (lobby, active) = reserved(&tx, &guest, offer, guest.balance(), 110);
    assert!(!active.idle());
    assert_eq!(active.balance() + lobby.utxo.amount, guest.balance());
    let (tx, states) = cancel(&b, &lobby, &active, &key(1), &[coin(61, &key(1), 10_000_000_000)], 100_000).unwrap();
    let idle = accepted_set(&tx, [0, 1, 2], states, 120);
    idle.validate(&b).unwrap();
    assert!(idle.idle());
    assert_eq!(idle.id().unwrap(), guest.id().unwrap());
    assert_eq!(idle.balance(), guest.balance());
    assert_eq!(idle.generation(), 1);
}
#[test]
fn full_lifecycle_timeout_releases_both_sets_without_loser_signature() {
    let a = artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let guest = created(&b, 1, 800_000_000);
    let host = created(&b, 2, 800_000_000);
    let offer = make_offer(&guest, &host, false);
    let tx = reserve(&b, &guest, &offer, &key(1), guest.balance(), &[coin(60, &key(1), 10_000_000_000)], 100_000).unwrap();
    let (lobby, guest_active) = reserved(&tx, &guest, offer, guest.balance(), 110);
    let (start_tx, game, states) =
        start(&b, &lobby, &guest_active, &host, &key(2), host.balance(), &[coin(61, &key(2), 10_000_000_000)], 100_000).unwrap();
    let mut current = accepted_match(&start_tx, game, states, &guest_active, 120);
    let side = next_contact(&current.snapshot()).unwrap().0;
    let winner = 1 - side;
    let (end, result) =
        timeout(&b, &current, &key(winner as u8 + 1), &[coin(62, &key(winner as u8 + 1), 10_000_000_000)], 100_000).unwrap();
    assert_eq!(result.collection.finish.winner, winner as i64);
    let (_, deadline) = next_contact(&current.snapshot()).unwrap();
    assert_eq!(result.collection.finish.end_daa, deadline + 110);
    // Use otherwise valid lane continuations in the timeout transaction.
    // The Court must refuse them, even though the matching game is co-spent.
    let game_id = current.court.utxo.covenant_id.unwrap();
    let court_age = (result.collection.finish.end_daa as u64).saturating_sub(current.court.utxo.block_daa_score);
    let ctx = TxContext::new()
        .actor_input("Court", current.court.state.state(), "timeout", current.court.outpoint, current.court.utxo.clone(), court_age)
        .actor_output("Collector", result.state(), CovenantBinding::new(0, game_id), current.court.utxo.amount);
    let queues = &current.sets[side];
    let unchanged = std::array::from_fn(|i| queues.lanes[i].state.clone());
    let (ctx, first) = active_inputs(ctx, queues, &unchanged).unwrap();
    let ctx = set_outputs(ctx, queues, &unchanged, std::array::from_fn(|i| queues.lanes[i].utxo.amount), first).unwrap();
    let owner = key(winner as u8 + 1);
    let ctx = fund(ctx, &[coin(67, &owner, 10_000_000_000)], &owner, 100_000).unwrap();
    assert!(b.build(&ctx).is_err(), "timeout must reject a coordinated queue spend");
    let result = accepted_cell(&end, 0, 400, result);
    let (tx, states) =
        collect(&b, &result, &current.sets, &key(winner as u8 + 1), &[coin(63, &key(winner as u8 + 1), 10_000_000_000)], 100_000)
            .unwrap();
    let sets =
        std::array::from_fn::<_, 2, _>(|side| accepted_set(&tx, std::array::from_fn(|i| side * 3 + i), states[side].clone(), 410));
    for side in 0..2 {
        sets[side].validate(&b).unwrap();
        assert!(sets[side].idle());
        assert_eq!(sets[side].id().unwrap(), current.sets[side].id().unwrap());
        assert_eq!(sets[side].balance(), if side == winner { 1_300_000_000 } else { 300_000_000 });
    }
    // Role reversal, with direct top-up if this wallet lost. No new queue IDs.
    let offer = make_offer(&sets[1], &sets[0], true);
    let guest_capital = sets[1].balance().max(required_capital(&sets[1], DEFAULT_STAKE as u64).unwrap());
    let tx = reserve(&b, &sets[1], &offer, &key(2), guest_capital, &[coin(64, &key(2), 10_000_000_000)], 100_000).unwrap();
    let (lobby, guest) = reserved(&tx, &sets[1], offer, guest_capital, 420);
    let host_capital = sets[0].balance().max(required_capital(&sets[0], DEFAULT_STAKE as u64).unwrap());
    let (tx, game, states) =
        start(&b, &lobby, &guest, &sets[0], &key(1), host_capital, &[coin(65, &key(1), 10_000_000_000)], 100_000).unwrap();
    current = accepted_match(&tx, game, states, &guest, 430);
    let side = next_contact(&current.snapshot()).unwrap().0;
    let until = next_contact(&current.snapshot()).unwrap().1;
    let owner = key(if side == 0 { 2 } else { 1 });
    let (tx, _, _) = checkpoint(&b, &current, side, until, &owner, &[coin(66, &owner, 10_000_000_000)], 100_000).unwrap();
    assert!(transaction_mass(&tx).unwrap().priority > 0);
    for (name, tx) in [("start", &start_tx), ("end", &end), ("resolve", &tx)] {
        eprintln!("{name}: {:?}", transaction_mass(tx).unwrap());
    }
}

#[test]
fn previous_queue_templates_remain_explicitly_closable(){
    let new=artifact().unwrap();let new_builder=TxBuilder::new(&new).unwrap();
    for old in [previous_artifact().unwrap(), v28_artifact().unwrap(), v29_artifact().unwrap(), v30_artifact().unwrap(), v31_artifact().unwrap(), v35_artifact().unwrap(), v36_artifact().unwrap()] {
        let old_builder = TxBuilder::new(&old).unwrap();
        let old_set = created(&old_builder, 1, 800_000_000);
        assert!(old_set.validate(&new_builder).is_err());
        let tx = close(&old_builder, &old_set, &key(1), &[], 100_000).unwrap();
        assert_eq!(tx.outputs[0].value, old_set.balance() - 100_000);
    }
}

#[test]
fn compact_start_requires_the_correct_disclosed_queue_proof_even_when_resigned(){
    let a=artifact().unwrap();let b=TxBuilder::new(&a).unwrap();let guest=created(&b,1,800_000_000);let host=created(&b,2,800_000_000);
    let mut offer=make_offer(&guest,&host,false);offer.pools[1]=Hash::from_bytes(pubkey(&key(2)).try_into().unwrap());offer.generations[1]=0;
    let tx=reserve(&b,&guest,&offer,&key(1),guest.balance(),&[coin(96,&key(1),10_000_000_000)],100_000).unwrap();
    let (lobby,guest)=reserved(&tx,&guest,offer,guest.balance(),110);
    for missing in [true,false]{
        let (ctx,_,_)=start_context(&b,&lobby,&guest,&host,&key(2),host.balance(),&[coin(97,&key(2),10_000_000_000)],100_000).unwrap();
        let payload=if missing{vec![]}else{let mut p=start_payload(&host).unwrap();let last=p.len()-1;p[last]^=1;p};
        assert!(b.build(&ctx.payload(payload)).is_err(),"a newly signed false or absent host proof was accepted");
    }
}
