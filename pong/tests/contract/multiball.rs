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
    for tick in [5, 6, 7, 8] {
        for count in 1..=3 {
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
    for (tick, count, target) in [(5, 3, 6), (6, 2, 5), (7, 1, 1), (8, 3, 9)] {
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
                let accepted = s.court.utxo.block_daa_score + 1;
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
            assert_eq!((launch - phase).rem_euclid(period), 0);
            assert!(launch - period < deadline + SERVE_DELAY_DAA);
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
    for tick in [5, 6, 7, 8] {
        let s = fixture(&b, tick, 3, 9);
        let (_, side, deadline) = next_ball_contact(&s).unwrap();
        let finish = timeout_result(&s).unwrap();
        assert_eq!(finish.end_daa, deadline + RESPONSE_TIMEOUT_DAA as i64);
        assert_eq!(finish.win_score, 9);
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
    let s = fixture(&b, 5, 3, 6);
    let (_, side, deadline) = next_ball_contact(&s).unwrap();
    let plan = resolve(&s).unwrap();
    for attack in 0..6 {
        let mut forged = plan.clone();
        match attack {
            0 => forged.world.extra_balls[0].x += 1,
            1 => forged.world.extra_balls[1].serve_until += 1,
            2 => forged.world.launch_epoch += 1,
            3 => forged.world.win_score = 9,
            4 => forged.world.ball_count = 1,
            _ => forged.queues[side].through += 1,
        }
        let owner = key(side as u8 + 1); let payer = key(4); let coin = funding();
        let context = checkpoint_context(&s, side, deadline, &forged, &owner).unwrap();
        assert!(build(&b, &funded(context, &coin, &payer, 100_000).unwrap()).is_err(), "attack {attack}");
    }
}
