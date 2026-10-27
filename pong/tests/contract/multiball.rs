//! Multiball scheduling and real covenant execution with disposable fixture coins.
use argent_runtime::{Artifact, ContextInput, TxBuilder, execute_transaction_with_covenants};
use kaspa_consensus_core::{Hash, config::params::MAINNET_PARAMS, mass::MassCalculator,
    tx::{Transaction, TransactionOutpoint, UtxoEntry}};
use kaspa_pong::engine::*;
use secp256k1::{Keypair, Secp256k1, SecretKey};

fn key(n: u8) -> Keypair {
    Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
}
fn artifact() -> Artifact { serde_json::from_str(include_str!("../../artifacts/artifact.json")).unwrap() }
fn funding() -> Funding {
    Funding { outpoint: TransactionOutpoint::new(Hash::from_bytes([90; 32]), 0),
        utxo: UtxoEntry::new(100_000_000_000, p2pk(&pubkey(&key(4))), 1, false, None) }
}
fn fixture(b: &TxBuilder<'_>, tick: i64, count: i64, score: i64) -> Snapshot {
    let mut world = World::opening(pubkey(&key(1)), pubkey(&key(2)));
    world.ball_tick_daa = tick; world.ball_count = count; world.win_score = score;
    let queues = empty_queues(&world.left, &world.right);
    let id = Some(Hash::from_bytes([77; 32]));
    Snapshot {
        court: Cell { outpoint: TransactionOutpoint::new(Hash::from_bytes([1; 32]), 0),
            utxo: b.covenant_utxo("Court", world.state(), COURT_VALUE, 100, false, id).unwrap(), state: world },
        players: std::array::from_fn(|i| Cell {
            outpoint: TransactionOutpoint::new(Hash::from_bytes([i as u8 + 2; 32]), 0),
            utxo: b.covenant_utxo("Player", queues[i].state(), QUEUE_DEPOSIT, 100, false, id).unwrap(),
            state: queues[i].clone(),
        }),
    }
}
fn verify(tx: &Transaction, entries: Vec<UtxoEntry>) {
    assert!(tx.inputs.iter().all(|input| input.signature_script.len() <= 32_768));
    execute_transaction_with_covenants(&mut tx.clone(), entries).unwrap();
    let mass = MassCalculator::new_with_consensus_params(&MAINNET_PARAMS).calc_non_contextual_masses(tx);
    let limit = MAINNET_PARAMS.block_mass_limits().after();
    assert!(mass.compute_mass <= limit.compute && mass.transient_mass <= limit.transient, "{mass}");
}

#[test]
fn multiball_launch_phases_keep_all_possible_contact_times_distinct() {
    let a = artifact(); let b = TxBuilder::new(&a).unwrap();
    let mut directions = std::collections::HashSet::new();
    let mut slopes = std::collections::HashSet::new();
    for tick in 1..=15 {
        for count in 1..=8 {
            let snapshot = fixture(&b, tick, count, 9);
            let w = project(&snapshot, 100).unwrap().world;
            let period = 27 * tick;
            assert_eq!(w.launch_epoch, 125);
            for i in 0..count as usize {
                let ball = w.ball(i);
                assert_eq!(ball.serve_until, 125 + (i as i64 * period + count - 1) / count);
                directions.insert(ball.vx); slopes.insert(ball.vy);
                for j in 0..i {
                    for left_ticks in [13, 14] {
                        for right_ticks in [13, 14] {
                            assert_ne!((w.launch_phase(i) + left_ticks * tick).rem_euclid(period),
                                (w.launch_phase(j) + right_ticks * tick).rem_euclid(period));
                        }
                    }
                }
            }
        }
    }
    assert_eq!(directions.len(), 2);
    assert!(slopes.len() > 1 && slopes.iter().any(|&s| s != 0));
}

#[test]
fn multiball_vm_contacts_respawns_configured_score_and_payout_agree() {
    let a = artifact(); let b = TxBuilder::new(&a).unwrap();
    // Repeated goals exercise phase retention; full 15-goal matches at every
    // configuration duplicate the opening/boundary checks below.
    for (tick, count, target) in [(1, 8, 6), (15, 7, 2), (7, 3, 3), (15, 1, 1)] {
        let mut s = fixture(&b, tick, count, target);
        let mut contacts = 0;
        while !s.court.state.ended() {
            contacts += 1;
            assert!(contacts <= 2 * target, "forced misses must finish the match");
            let (index, side, deadline) = next_ball_contact(&s).unwrap();
            let before = project(&s, deadline as u64 - 1).unwrap().world;
            let incoming = before.ball(index);
            let vertical = (incoming.y + incoming.vy).rem_euclid(62);
            let landing = if vertical <= 31 { vertical } else { 62 - vertical };
            let destination = if landing > 9 { PADDLE_MIN_Y } else { PADDLE_MAX_Y };
            let direction = destination - s.players[side].state.y;
            if direction != 0 {
                // Commands may share the Court acceptance DAA; eight-ball contacts can be two ticks apart.
                let accepted = s.court.utxo.block_daa_score;
                assert!(accepted < deadline as u64);
                let (tx, queue) = command_tx(&b, &s.players[side], direction, &key(side as u8 + 1),
                    &funding(), &key(4), 100_000).unwrap();
                verify(&tx, vec![s.players[side].utxo.clone(), funding().utxo]);
                s.players[side] = accepted_cell(&tx, 0, accepted, queue);
            }
            let projected = project(&s, deadline as u64).unwrap().world;
            let (tx, plan) = resolve_tx(&b, &s, &key(side as u8 + 1), &funding(), &key(4), 100_000, false).unwrap();
            let mut entries = vec![s.court.utxo.clone()];
            entries.extend([side, side + 2, side + 4].map(|i| s.players[i].utxo.clone()));
            entries.push(funding().utxo);
            verify(&tx, entries);
            assert_eq!(plan.world.left_score + plan.world.right_score, contacts);
            assert_eq!(plan.world.ball(index), projected.ball(index));
            assert_eq!(plan.world.left_score, projected.left_score);
            assert_eq!(plan.world.right_score, projected.right_score);
            let launch = plan.world.ball(index).serve_until;
            let phase = plan.world.launch_phase(index); let period = plan.world.court_period();
            assert!(launch >= deadline + SERVE_DELAY_DAA);
            if count == 1 {
                assert_eq!(launch, deadline + SERVE_DELAY_DAA);
            } else {
                assert_eq!((launch - phase).rem_euclid(period), 0);
                assert!(launch - period < deadline + SERVE_DELAY_DAA);
            }
            let mass = MassCalculator::new_with_consensus_params(&MAINNET_PARAMS).calc_non_contextual_masses(&tx);
            if contacts == 1 { println!("{count} balls, {tick} DAA/tick contact: {mass}"); }
            s = accepted_transition(&b, &s, &tx, plan, deadline as u64 + 1).unwrap();
        }
        assert_eq!(s.court.state.left_score.max(s.court.state.right_score), target);
        let finish = s.court.state.result();
        assert_eq!(finish.win_score, target);
        let settled = settle_tx(&b, &s, &funding(), &key(4), 100_000).unwrap();
        verify(&settled, vec![s.court.utxo.clone(), funding().utxo]);
        let collecting = accepted_collection(&s, &settled, finish.clone(), s.court.utxo.block_daa_score + 1);
        let paid = reclaim_tx(&b, &collecting, &funding(), &key(4), 100_000).unwrap();
        let mut entries = vec![collecting.collector.utxo.clone()];
        entries.extend(collecting.players.iter().map(|q| q.utxo.clone())); entries.push(funding().utxo);
        verify(&paid, entries);
        assert_eq!(paid.outputs[0].value, GAME_POT);
        assert_eq!(paid.outputs[0].script_public_key, p2pk(if finish.winner == 0 { &s.court.state.left } else { &s.court.state.right }));
    }
}

#[test]
fn multiball_timeout_uses_the_earliest_receiver_and_preserves_target() {
    let a = artifact(); let b = TxBuilder::new(&a).unwrap();
    for tick in [1, 5, 8, 15] {
        let s = fixture(&b, tick, 8, 15);
        let (_, side, deadline) = next_ball_contact(&s).unwrap();
        let finish = timeout_result(&s).unwrap();
        assert_eq!(finish.end_daa, deadline + RESPONSE_TIMEOUT_DAA as i64);
        assert_eq!(finish.win_score, 15);
        let (tx, actual) = timeout_tx(&b, &s, &key(2 - side as u8), &funding(), &key(4), 100_000).unwrap();
        assert_eq!(actual, finish);
        verify(&tx, vec![s.court.utxo.clone(), funding().utxo]);
        let mut early = timeout_context(&s, &key(2 - side as u8)).unwrap();
        if let ContextInput::Actor(input) = &mut early.inputs[0] { input.sequence -= 1; }
        let fee_coin = funding(); let payer = key(4);
        let early = funded(early, &fee_coin, &payer, 100_000);
        // Rebuild with fresh signatures so the sequence guard is what fails.
        assert!(build(&b, &early.unwrap()).is_err());
        assert!(timeout_tx(&b, &s, &key(side as u8 + 1), &funding(), &key(4), 100_000).is_err());
    }
}

#[test]
fn multiball_contact_cannot_change_other_balls_or_match_terms() {
    let a = artifact(); let b = TxBuilder::new(&a).unwrap();
    let s = fixture(&b, 1, 8, 15);
    let (_, side, deadline) = next_ball_contact(&s).unwrap();
    let plan = resolve(&s).unwrap();
    for attack in 0..11 {
        let mut forged = plan.clone();
        match attack {
            0 => forged.world.extra_balls[0].x += 1,
            1 => forged.world.extra_balls[1].serve_until += 1,
            2 => forged.world.extra_balls[2].vx *= -1,
            3 => forged.world.extra_balls[3].vy += 1,
            4 => forged.world.extra_balls[4].y += 1,
            5 => forged.world.extra_balls[5].x += 1,
            6 => forged.world.extra_balls[6].serve_until += 1,
            7 => forged.world.launch_epoch += 1,
            8 => forged.world.win_score = 9,
            9 => forged.world.ball_count = 1,
            _ => forged.queues[side].through += 1,
        }
        let owner = key(side as u8 + 1); let payer = key(4); let coin = funding();
        let context = checkpoint_context(&s, side, deadline, &forged, &owner).unwrap();
        assert!(build(&b, &funded(context, &coin, &payer, 100_000).unwrap()).is_err(), "attack {attack}");
    }
}

#[test]
fn multiball_vm_opening_contact_and_started_timeout_cover_boundaries() {
    let a = artifact(); let b = TxBuilder::new(&a).unwrap();
    for (tick, count) in [(1, 1), (15, 1), (7, 2), (6, 3), (1, 8), (15, 8)] {
        let s = fixture(&b, tick, count, 15);
        let (index, side, deadline) = next_ball_contact(&s).unwrap();
        assert_eq!(index, 0, "opening timeout shortcut: {count} balls, tick {tick}");
        let (tx, plan) = resolve_tx(&b, &s, &key(side as u8 + 1), &funding(), &key(4), 100_000, false).unwrap();
        let mut entries = vec![s.court.utxo.clone()];
        entries.extend([side, side + 2, side + 4].map(|i| s.players[i].utxo.clone()));
        entries.push(funding().utxo);
        verify(&tx, entries);
        let next = accepted_transition(&b, &s, &tx, plan, deadline as u64 + 1).unwrap();
        for state in [&s, &next] {
            let (_, receiver, _) = next_ball_contact(state).unwrap();
            let (timeout, _) = timeout_tx(&b, state, &key(2 - receiver as u8), &funding(), &key(4), 100_000).unwrap();
            verify(&timeout, vec![state.court.utxo.clone(), funding().utxo]);
        }
    }
}

#[test]
fn multiball_slots_seven_and_eight_resolve_and_authenticate_every_field() {
    let a = artifact(); let b = TxBuilder::new(&a).unwrap();
    for index in [6, 7] {
        let mut s = fixture(&b, 1, 8, 15);
        s.court.state.started = true;
        s.court.state.clock = 100;
        s.court.state.launch_epoch = 100;
        s.court.state.extra_balls[index - 1] = Ball { x: RIGHT_CONTACT_X - 2, y: 16, vx: 1, vy: 1, serve_until: 100 };
        s.court.utxo = b.covenant_utxo("Court", s.court.state.state(), COURT_VALUE, 100, false, s.court.utxo.covenant_id).unwrap();
        let (actual, side, deadline) = next_ball_contact(&s).unwrap();
        assert_eq!(actual, index);
        let (tx, plan) = resolve_tx(&b, &s, &key(side as u8 + 1), &funding(), &key(4), 100_000, false).unwrap();
        let mut entries = vec![s.court.utxo.clone()];
        entries.extend([side, side + 2, side + 4].map(|i| s.players[i].utxo.clone()));
        entries.push(funding().utxo);
        verify(&tx, entries);
        let projected = project(&s, deadline as u64).unwrap().world;
        assert_eq!(plan.world.ball(index), projected.ball(index));
        assert_ne!(plan.world.ball(index).vx, s.court.state.ball(index).vx);
        let (_, receiver, _) = next_ball_contact(&s).unwrap();
        let (timeout, _) = timeout_tx(&b, &s, &key(2 - receiver as u8), &funding(), &key(4), 100_000).unwrap();
        verify(&timeout, vec![s.court.utxo.clone(), funding().utxo]);
    }
    // Even inactive slots belong to the committed 320-byte state.
    for count in [1, 7, 8] {
        let s = fixture(&b, 1, count, 15);
        let (_, side, deadline) = next_ball_contact(&s).unwrap();
        let plan = resolve(&s).unwrap();
        for index in [6, 7] {
            for field in 0..5 {
                let mut forged = plan.clone();
                let ball = &mut forged.world.extra_balls[index - 1];
                match field {
                    0 => ball.x += 1,
                    1 => ball.y += 1,
                    2 => ball.vx *= -1,
                    3 => ball.vy += 1,
                    _ => ball.serve_until += 1,
                }
                let owner = key(side as u8 + 1); let payer = key(4); let coin = funding();
                let context = checkpoint_context(&s, side, deadline, &forged, &owner).unwrap();
                assert!(build(&b, &funded(context, &coin, &payer, 100_000).unwrap()).is_err(),
                    "count {count}, slot {}, field {field}", index + 1);
            }
        }
    }
}
