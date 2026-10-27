//! Pending resolve outputs may start one command per lane without inventing time.
use argent_runtime::{Artifact, EntryCall, TxBuilder, TxContext, args,
    execute_transaction_with_covenants};
use kaspa_consensus_core::{Hash, constants::UNACCEPTED_DAA_SCORE,
    tx::{CovenantBinding, Transaction, TransactionOutpoint, UtxoEntry}};
use kaspa_pong::engine::*;
use secp256k1::{Keypair, Secp256k1, SecretKey};

fn key(n: u8) -> Keypair {
    Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
}
fn funding() -> Funding {
    Funding { outpoint: TransactionOutpoint::new(Hash::from_bytes([90; 32]), 0),
        utxo: UtxoEntry::new(100_000_000_000, p2pk(&pubkey(&key(4))), 1, false, None) }
}
fn fixture(b: &TxBuilder<'_>, side: usize) -> Snapshot {
    let mut world = World::opening(pubkey(&key(1)), pubkey(&key(2)));
    world.started = true;
    world.clock = 100;
    world.ball_tick_daa = 2;
    world.ball_vx = if side == 1 { 1 } else { -1 };
    world.ball_x = if side == 1 { RIGHT_CONTACT_X - 15 * BALL_SPEED } else { LEFT_CONTACT_X + 15 * BALL_SPEED };
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
}
// Deliberately bypass Queue::push so negative checks exercise the covenant itself.
fn raw_command(b: &TxBuilder<'_>, parent: &Cell<Queue>, next: &Queue, owner: &Keypair) -> Result<Transaction> {
    let ctx = TxContext::new().actor_input("Player", parent.state.state(),
        EntryCall::new("command").args_with(|tx, i| args![1_i64, sign(tx, i, owner)]),
        parent.outpoint, parent.utxo.clone(), 0)
        .actor_output("Player", next.state(), CovenantBinding::new(0, parent.utxo.covenant_id.unwrap()), parent.utxo.amount);
    build(b, &funded(ctx, &funding(), &key(4), 100_000)?)
}

#[test]
fn pending_resolve_all_owned_lanes_allow_one_child_then_require_real_acceptance() {
    let a: Artifact = serde_json::from_str(include_str!("../../artifacts/artifact.json")).unwrap();
    let b = TxBuilder::new(&a).unwrap();
    assert_eq!(UNACCEPTED_DAA_SCORE, u64::MAX);
    for side in [0, 1] {
        for retained in [false, true] {
            let mut s = fixture(&b, side);
            let owner = key(side as u8 + 1);
            assert_eq!(next_contact(&s).unwrap(), (side, 130));
            if retained {
                for i in [side, side + 2, side + 4] {
                    let (tx, q) = command_tx(&b, &s.players[i], 1, &owner, &funding(), &key(4), 100_000).unwrap();
                    verify(&tx, vec![s.players[i].utxo.clone(), funding().utxo]);
                    s.players[i] = accepted_cell(&tx, 0, 130, q);
                }
            }
            let (resolve_tx, plan) = resolve_tx(&b, &s, &owner, &funding(), &key(4), 100_000, false).unwrap();
            let mut entries = vec![s.court.utxo.clone()];
            entries.extend([side, side + 2, side + 4].map(|i| s.players[i].utxo.clone()));
            entries.push(funding().utxo);
            verify(&resolve_tx, entries);
            for (output, i) in [side, side + 2, side + 4].into_iter().enumerate() {
                let q = plan.queues[i].clone();
                let old_count = usize::from(retained);
                assert_eq!(q.count, old_count);
                assert!(q.times[..q.count].iter().all(|&t| t == 130));
                // Output order is Court, lane 0, lane 1, lane 2, fee change.
                let parent = accepted_cell(&resolve_tx, output + 1, UNACCEPTED_DAA_SCORE, q);
                let (child_tx, child_q) = command_tx(&b, &parent, 1, &owner, &funding(), &key(4), 100_000).unwrap();
                verify(&child_tx, vec![parent.utxo.clone(), funding().utxo]);
                assert_eq!(child_q.count, old_count + 1);
                assert_eq!(&child_q.times[..old_count], &parent.state.times[..old_count]);
                assert_eq!(child_q.times[old_count], -1);
                let child = accepted_cell(&child_tx, 0, UNACCEPTED_DAA_SCORE, child_q.clone());
                assert!(command_tx(&b, &child, 1, &owner, &funding(), &key(4), 100_000).is_err());
                let mut forged = child_q.clone();
                forged.count += 1;
                forged.times[old_count] = 137;
                forged.times[old_count + 1] = -1;
                forged.commands[old_count + 1] = 1;
                assert!(raw_command(&b, &child, &forged, &owner).is_err(), "VM cannot stamp an unaccepted command");

                let accepted_child = accepted_cell(&child_tx, 0, 137, child_q.clone());
                let (grandchild, stamped) = command_tx(&b, &accepted_child, 1, &owner, &funding(), &key(4), 100_000).unwrap();
                verify(&grandchild, vec![accepted_child.utxo.clone(), funding().utxo]);
                assert_eq!(stamped.times[old_count], 137);
                assert_eq!(stamped.times[old_count + 1], -1);
                for fake_time in [130, 136, 138, -1] {
                    let mut forged = stamped.clone();
                    forged.times[old_count] = fake_time;
                    assert!(raw_command(&b, &accepted_child, &forged, &owner).is_err());
                }
                assert!(raw_command(&b, &parent, &child_q, &key(8)).is_err());
                let mut forged = child_q.clone();
                forged.owner = pubkey(&key(8));
                assert!(raw_command(&b, &parent, &forged, &owner).is_err());
                let mut forged = child_q.clone();
                forged.times[old_count] = 137;
                assert!(raw_command(&b, &parent, &forged, &owner).is_err(), "new movement must remain unstamped");
                if retained {
                    let mut forged = child_q.clone();
                    forged.times[0] = 129;
                    assert!(raw_command(&b, &parent, &forged, &owner).is_err(), "explicit history cannot be retimed");
                }
            }
        }
    }
}

#[test]
fn pending_child_cannot_move_before_or_at_the_old_contact() {
    let a: Artifact = serde_json::from_str(include_str!("../../artifacts/artifact.json")).unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let s = fixture(&b, 1);
    let owner = key(2);
    let (parent_tx, plan) = resolve_tx(&b, &s, &owner, &funding(), &key(4), 100_000, false).unwrap();
    let parent = accepted_cell(&parent_tx, 1, UNACCEPTED_DAA_SCORE, plan.queues[1].clone());
    let (child_tx, child_q) = command_tx(&b, &parent, 1, &owner, &funding(), &key(4), 100_000).unwrap();
    for at in [129, 130, 131] {
        // Reuse the old Court to prove the child cannot change that contact.
        let mut old = s.clone();
        for (output, i) in [1, 3, 5].into_iter().enumerate() {
            old.players[i] = accepted_cell(&parent_tx, output + 1, 130, plan.queues[i].clone());
        }
        old.players[1] = accepted_cell(&child_tx, 0, at, child_q.clone());
        if at < 130 {
            assert!(resolve(&old).is_err(), "movement cannot precede certified watermark");
            assert!(b.build(&resolve_context(&old, &plan, &owner).unwrap()).is_err());
            continue;
        }
        let (tx, unchanged) = resolve_tx(&b, &old, &owner, &funding(), &key(4), 100_000, false).unwrap();
        verify(&tx, vec![old.court.utxo.clone(), old.players[1].utxo.clone(), old.players[3].utxo.clone(),
            old.players[5].utxo.clone(), funding().utxo]);
        assert_eq!(unchanged.world, plan.world);
        assert_eq!(unchanged.queues[1].y, plan.queues[1].y);
        assert_eq!(unchanged.queues[1].count, 1);
        assert_eq!(unchanged.queues[1].times[0], at as i64);
        let mut forged = unchanged.clone();
        forged.queues[1].count = 0;
        forged.queues[1].times[0] = 0;
        forged.queues[1].commands[0] = 0;
        forged.queues[1].y += 1;
        assert!(b.build(&resolve_context(&old, &forged, &owner).unwrap()).is_err(), "equal/late DAA cannot be applied retroactively");
    }
}

#[test]
fn timestamp_guard_depends_on_placeholder_and_preserves_queue_bounds() {
    let a: Artifact = serde_json::from_str(include_str!("../../artifacts/artifact.json")).unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let s = fixture(&b, 1);
    // The guard also permits empty queues without inspecting the parent transaction type.
    for accepted in [100, UNACCEPTED_DAA_SCORE] {
        let mut parent = s.players[1].clone();
        parent.utxo.block_daa_score = accepted;
        let (tx, q) = command_tx(&b, &parent, 1, &key(2), &funding(), &key(4), 100_000).unwrap();
        verify(&tx, vec![parent.utxo.clone(), funding().utxo]);
        assert_eq!(q.times[0], -1);
    }
    for count in 0..=4 {
        let mut q = s.players[1].state.clone();
        q.count = count;
        q.times = [110, 111, 112, 113];
        let explicit = q.clone();
        q.stamp(UNACCEPTED_DAA_SCORE).unwrap();
        assert_eq!(q, explicit);
        if count < 4 {
            let mut parent = s.players[1].clone();
            parent.state = explicit.clone();
            parent.state.commands = [1; 4];
            parent.utxo = b.covenant_utxo("Player", parent.state.state(), QUEUE_DEPOSIT,
                UNACCEPTED_DAA_SCORE, false, parent.utxo.covenant_id).unwrap();
            let owner = key(2);
            let (tx, next) = command_tx(&b, &parent, 1, &owner, &funding(), &key(4), 100_000).unwrap();
            verify(&tx, vec![parent.utxo.clone(), funding().utxo]);
            assert_eq!(next.times[count], -1);
            if count > 0 {
                parent.state.times[count - 1] = -1;
                parent.utxo = b.covenant_utxo("Player", parent.state.state(), QUEUE_DEPOSIT,
                    UNACCEPTED_DAA_SCORE, false, parent.utxo.covenant_id).unwrap();
                assert!(command_tx(&b, &parent, 1, &owner, &funding(), &key(4), 100_000).is_err());
                assert!(raw_command(&b, &parent, &next, &owner).is_err());
                parent.utxo.block_daa_score = 137;
                let (tx, stamped) = command_tx(&b, &parent, 1, &owner, &funding(), &key(4), 100_000).unwrap();
                verify(&tx, vec![parent.utxo.clone(), funding().utxo]);
                assert_eq!(stamped.times[count - 1], 137);
                assert_eq!(stamped.times[count], -1);
            }
        }
        if count > 0 {
            q.times[count - 1] = -1;
            assert!(q.stamp(UNACCEPTED_DAA_SCORE).is_err());
            q.stamp(137).unwrap();
            assert_eq!(q.times[count - 1], 137);
        }
        if count == 4 { assert!(q.push(1, 137).is_err()); }
    }
    let mut invalid = s.players[1].state.clone();
    invalid.count = 5;
    assert!(invalid.stamp(100).is_err());
    assert!(invalid.stamp(UNACCEPTED_DAA_SCORE).is_err());
}
