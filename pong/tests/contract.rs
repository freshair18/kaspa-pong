//! Covenant invariants, adversarial transitions, and response deadlines.

mod invariants {
    //! Current covenant security and integration checks using the generated scripts.
    use argent_runtime::{Artifact, EntryCall, TxBuilder, execute_input_with_covenants, execute_transaction_with_covenants};
    use kaspa_consensus_core::{
        Hash,
        mass::transaction_estimated_serialized_size,
        tx::{Transaction, TransactionOutpoint, UtxoEntry},
    };
    use kaspa_pong::engine::*;
    use secp256k1::{Keypair, Secp256k1, SecretKey};
    use std::sync::OnceLock;

    fn artifact() -> &'static Artifact {
        static A: OnceLock<Artifact> = OnceLock::new();
        A.get_or_init(|| serde_json::from_str(include_str!("../artifacts/artifact.json")).unwrap())
    }
    fn key(n: u8) -> Keypair {
        Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
    }
    fn out(n: u8) -> TransactionOutpoint {
        TransactionOutpoint::new(Hash::from_bytes([n; 32]), 0)
    }
    fn funding() -> Funding {
        Funding { outpoint: out(90), utxo: UtxoEntry::new(100_000_000_000, p2pk(&pubkey(&key(4))), 1, false, None) }
    }
    fn fixture() -> Snapshot {
        let b = TxBuilder::new(artifact()).unwrap();
        let id = Hash::from_bytes([77; 32]);
        let mut w = World::opening(pubkey(&key(1)), pubkey(&key(2)));
        w.started = true;
        w.clock = 100;
        w.ball_tick_daa = 2;
        // Keep timing/security fixtures at DAA 130 independently of court width.
        // Actual opening and post-goal center placement are checked separately.
        w.ball_x = RIGHT_CONTACT_X - 15 * BALL_SPEED;
        let qs = empty_queues(&w.left, &w.right);
        Snapshot {
            court: Cell {
                outpoint: out(1),
                utxo: b.covenant_utxo("Court", w.state(), COURT_VALUE, 100, false, Some(id)).unwrap(),
                state: w,
            },
            players: std::array::from_fn(|i| Cell {
                outpoint: out(i as u8 + 2),
                utxo: b.covenant_utxo(qs[i].actor(), qs[i].state(), QUEUE_DEPOSIT, 100, false, Some(id)).unwrap(),
                state: qs[i].clone(),
            }),
        }
    }
    fn materialize(s: &mut Snapshot) {
        let b = TxBuilder::new(artifact()).unwrap();
        s.court.utxo = b
            .covenant_utxo(
                "Court",
                s.court.state.state(),
                s.court.utxo.amount,
                s.court.utxo.block_daa_score,
                false,
                s.court.utxo.covenant_id,
            )
            .unwrap();
        for p in &mut s.players {
            p.utxo = b
                .covenant_utxo(p.state.actor(), p.state.state(), p.utxo.amount, p.utxo.block_daa_score, false, p.utxo.covenant_id)
                .unwrap();
        }
    }
    fn command(s: &mut Snapshot, i: usize, direction: i64, at: u64) -> Transaction {
        let b = TxBuilder::new(artifact()).unwrap();
        let (tx, q) = command_tx(&b, &s.players[i], direction, &key(i as u8 % 2 + 1), &funding(), &key(4), 100_000).unwrap();
        s.players[i] = accepted_cell(&tx, 0, at, q);
        tx
    }
    fn checkpoint(s: &mut Snapshot, side: usize, until: i64, at: u64) -> Transaction {
        let b = TxBuilder::new(artifact()).unwrap();
        let (tx, p) = checkpoint_tx(&b, s, side, until, &key(side as u8 + 1), &funding(), &key(4), 100_000).unwrap();
        *s = accepted_transition(&b, s, &tx, p, at).unwrap();
        tx
    }
    fn contact(s: &mut Snapshot, at: u64) -> Transaction {
        let b = TxBuilder::new(artifact()).unwrap();
        let (receiver, _) = next_contact(s).unwrap();
        let (tx, p) = resolve_tx(&b, s, &key(receiver as u8 + 1), &funding(), &key(4), 100_000, false).unwrap();
        *s = accepted_transition(&b, s, &tx, p, at).unwrap();
        tx
    }

    #[test]
    fn contact_protocol_checkpoint_excludes_exact_and_late_movements() {
        for move_at in [129, 130, 131] {
            let mut s = fixture();
            command(&mut s, 1, 10, move_at);
            let untouched: Vec<_> = [0, 2, 4].into_iter().map(|i| s.players[i].outpoint).collect();
            assert_eq!(next_contact(&s).unwrap(), (1, 130));
            let tx = contact(&mut s, 133);
            assert_eq!(s.players[1].state.y, if move_at < 130 { 26 } else { 16 });
            assert_eq!(tx.inputs.len(), 5, "ball, receiving player's three queues, fee only");
            assert_eq!(s.court.state.left_score, if move_at < 130 { 1 } else { 0 });
            assert_eq!(s.players[1].state.count, if move_at < 130 { 0 } else { 1 });
            assert_eq!(s.players[1].state.times[0], if move_at < 130 { 0 } else { move_at as i64 });
            for (j, i) in [0, 2, 4].into_iter().enumerate() {
                assert_eq!(s.players[i].outpoint, untouched[j]);
            }
        }
    }

    #[test]
    fn contact_protocol_rejects_forged_position_time_angle_and_omitted_lanes() {
        let mut s = fixture();
        command(&mut s, 3, 2, 120);
        let b = TxBuilder::new(artifact()).unwrap();
        let plan = checkpoint_plan(&s, 1, 130).unwrap();
        for field in 0..3 {
            let mut forged = plan.clone();
            match field {
                0 => forged.queues[1].y += 1,
                1 => forged.queues[1].through += 1,
                _ => forged.world.ball_y += 1,
            }
            assert!(b.build(&checkpoint_context(&s, 1, 130, &forged, &key(2)).unwrap()).is_err());
        }
        let mut ctx = checkpoint_context(&s, 1, 130, &plan, &key(2)).unwrap();
        ctx.inputs.remove(3);
        assert!(b.build(&ctx).is_err());
        let mut duplicate = s.clone();
        duplicate.players[5] = duplicate.players[3].clone();
        assert!(b.build(&checkpoint_context(&duplicate, 1, 130, &plan, &key(2)).unwrap()).is_err());
        let plan = resolve(&s).unwrap();
        for field in 0..4 {
            let mut forged = plan.clone();
            match field {
                0 => forged.world.ball_vy += 1,
                1 => forged.world.clock += 1,
                2 => forged.world.left_score += 1,
                _ => forged.queues[1].y += 1,
            }
            assert!(b.build(&resolve_context(&s, &forged, &key(2)).unwrap()).is_err());
        }
        let mut unaccepted = s.clone();
        unaccepted.players[1].utxo.block_daa_score = u64::MAX;
        assert!(resolve(&unaccepted).is_err());
        assert!(b.build(&resolve_context(&unaccepted, &plan, &key(2)).unwrap()).is_err());
    }

    #[test]
    fn contact_protocol_checkpoint_lock_and_safe_horizon_are_enforced() {
        let s = fixture();
        let b = TxBuilder::new(artifact()).unwrap();
        assert!(checkpoint_plan(&s, 1, 131).is_err());
        let p = checkpoint_plan(&s, 1, 130).unwrap();
        let mut ctx = checkpoint_context(&s, 1, 130, &p, &key(2)).unwrap();
        if let argent_runtime::ContextInput::Actor(input) = &mut ctx.inputs[0] {
            assert_eq!(input.sequence, 30);
            input.sequence = 29;
        }
        assert!(b.build(&ctx).is_err());
        assert!(resolve(&s).is_ok(), "position proof and contact are planned atomically; consensus enforces time");
    }

    mod goal_response_timeout {
        use super::*;
        use argent_runtime::{ContextInput, OutputOwner, OutputState};

        #[test]
        fn signed_empty_checkpoints_cannot_replace_court_without_progress() {
            let b = TxBuilder::new(artifact()).unwrap();
            let mut s = fixture();
            for side in [0, 1] {
                assert!(checkpoint_plan(&s, side, 0).is_err());
                let unchanged = Plan { world: s.court.state.clone(), queues: std::array::from_fn(|i| s.players[i].state.clone()) };
                let ctx = checkpoint_context(&s, side, 0, &unchanged, &key(side as u8 + 1)).unwrap();
                assert!(b.build(&ctx).is_err(), "even a valid owner signature cannot authorize an empty replacement");
            }
            // Non-receiving side may advance its watermark once, but cannot keep
            // recreating the same Court at the receiver's contact deadline.
            checkpoint(&mut s, 0, 130, 131);
            let expiry = timeout_result(&s).unwrap().end_daa;
            assert!(checkpoint_plan(&s, 0, 130).is_err());
            let unchanged = Plan { world: s.court.state.clone(), queues: std::array::from_fn(|i| s.players[i].state.clone()) };
            assert!(b.build(&checkpoint_context(&s, 0, 130, &unchanged, &key(1)).unwrap()).is_err());
            assert_eq!(timeout_result(&s).unwrap().end_daa, expiry);
            assert!(resolve(&s).is_ok(), "the receiver's real contact proof still progresses");
        }

        #[test]
        fn timeout_requires_expiry_and_opponent_signature_even_for_a_hit() {
            let s = fixture(); // The centered paddle would hit the horizontal ball.
            let b = TxBuilder::new(artifact()).unwrap();
            let result = timeout_result(&s).unwrap();
            assert_eq!((result.winner, result.left_score, result.right_score, result.end_daa), (0, 0, 0, 180));
            let mut ctx = timeout_context(&s, &key(1)).unwrap();
            if let ContextInput::Actor(input) = &mut ctx.inputs[0] {
                assert_eq!(input.sequence, 80);
                input.sequence = 79;
            }
            assert!(b.build(&ctx).is_err(), "one DAA early must fail the covenant guard");
            if let ContextInput::Actor(input) = &mut ctx.inputs[0] { input.sequence = 80 | (1u64 << 63); }
            assert!(b.build(&ctx).is_err(), "sequence disable bit must not bypass the timeout age guard");
            for claimant in [2, 8] {
                assert!(b.build(&timeout_context(&s, &key(claimant)).unwrap()).is_err());
            }
            for claimant in [1, 8] {
                let p = resolve(&s).unwrap();
                assert!(b.build(&resolve_context(&s, &p, &key(claimant)).unwrap()).is_err(), "only the receiver signs contact");
            }
            assert!(resolve_tx(&b, &s, &key(1), &funding(), &key(4), 100_000, true).is_err());
            let mut forged = timeout_context(&s, &key(1)).unwrap();
            let mut fake = result.clone();
            fake.left_score = WIN_SCORE;
            if let OutputOwner::Actor { state, .. } = &mut forged.outputs[0].owner {
                *state = OutputState::Static(Collection {
                    left: s.court.state.left.clone(), right: s.court.state.right.clone(), finish: fake,
                }.state());
            }
            assert!(b.build(&forged).is_err(), "timeout cannot fabricate a winning score");
            let (tx, finish) = timeout_tx(&b, &s, &key(1), &funding(), &key(4), 100_000).unwrap();
            assert_eq!(tx.inputs.len(), 2, "timeout consumes Court plus fee input only");
            let collecting = accepted_collection(&s, &tx, finish, 180);
            let refund = reclaim_tx(&b, &collecting, &funding(), &key(4), 100_000).unwrap();
            assert_eq!(refund.outputs[0].value, GAME_POT);
            assert_eq!(refund.outputs[0].script_public_key, p2pk(&s.court.state.left));
        }

        #[test]
        fn consumed_receiver_prefix_renews_timeout_and_proof_invalidates_old_claim() {
            let mut s = fixture();
            for (i, t) in [(1,101), (3,102), (5,103), (1,104), (3,105), (5,106)] {
                command(&mut s, i, 1, t);
            }
            let expiry = timeout_result(&s).unwrap().end_daa;
            let b = TxBuilder::new(artifact()).unwrap();
            let old = s.court.clone();
            let (old_claim, _) = timeout_tx(&b, &s, &key(1), &funding(), &key(4), 100_000).unwrap();
            checkpoint(&mut s, 1, 130, 150);
            assert!(s.players[1].state.through < 130);
            assert_eq!(timeout_result(&s).unwrap().end_daa, 200);
            let ctx = timeout_context(&s, &key(1)).unwrap();
            if let ContextInput::Actor(input) = &ctx.inputs[0] {
                assert_eq!(input.sequence, 50, "consumed receiver history gets fifty DAA from acceptance at 150");
            }
            b.build(&ctx).unwrap();
            let pre_proof = s.clone();
            checkpoint(&mut s, 1, 130, 181); // A late valid proof may still win the UTXO race.
            assert_eq!(s.court.state.resolves, 1);
            assert_ne!(s.court.outpoint, old.outpoint);
            assert_eq!(old_claim.inputs[0].previous_outpoint, old.outpoint);
            assert!(timeout_result(&s).unwrap().end_daa > expiry, "the next contact has a new obligation");
            let new_claimant = timeout_result(&s).unwrap().winner as u8 + 1;
            let mut stale_deadline = timeout_context(&s, &key(new_claimant)).unwrap();
            if let ContextInput::Actor(input) = &mut stale_deadline.inputs[0] { input.sequence = 0; }
            assert!(b.build(&stale_deadline).is_err());
            assert_eq!(timeout_result(&pre_proof).unwrap().end_daa, 200);
        }

        #[test]
        fn opening_compaction_preserves_deterministic_deadline() {
            let mut s = fixture();
            s.court.state.started = false;
            s.court.state.clock = 0;
            s.court.state.ball_x = CENTER_X;
            materialize(&mut s);
            let expiry = timeout_result(&s).unwrap().end_daa;
            checkpoint(&mut s, 0, 110, 115);
            assert!(s.court.state.started);
            assert_eq!(timeout_result(&s).unwrap().end_daa, expiry);
        }
    }

    #[test]
    fn contact_protocol_partial_checkpoints_preserve_order_and_timestamps() {
        let mut s = fixture();
        for (i, t, d) in [(1, 101, 27), (3, 101, -27), (5, 101, 3), (1, 102, -1), (3, 103, 1), (5, 104, 2), (1, 105, 1), (3, 106, 2)] {
            command(&mut s, i, d, t);
        }
        let expected = project(&s, 130).unwrap().world;
        let other = s.players[0].outpoint;
        checkpoint(&mut s, 1, 130, 130);
        assert!(s.players[1].state.through < 130, "four timestamp groups require another checkpoint");
        checkpoint(&mut s, 1, 130, 131);
        assert_eq!(s.players[1].state.through, 130);
        assert_eq!(s.court.state.ball_vy, expected.ball_vy);
        assert_eq!(s.court.state.left_score, expected.left_score);
        assert_eq!(s.players[0].outpoint, other);
    }

    #[test]
    fn contact_protocol_contact_stamps_newest_command_without_retiming() {
        let mut s = fixture();
        checkpoint(&mut s, 1, 120, 120);
        command(&mut s, 1, 1, 131);
        assert_eq!(s.players[1].state.times[0], -1);
        contact(&mut s, 140);
        assert_eq!(s.players[1].state.times[0], 131);
        assert_eq!(s.players[1].state.count, 1);
    }

    #[test]
    fn contact_protocol_launch_requires_guest_and_has_no_live_cancel() {
        let b = TxBuilder::new(artifact()).unwrap();
        let f = funding();
        let lobby = Lobby {
            left: pubkey(&key(1)),
            right: pubkey(&key(2)),
            refund: pubkey(&key(3)),
            control_value: SIDE_DEPOSIT as i64,
            ball_tick_daa: DEFAULT_BALL_TICK_DAA,
            stake_per_player: DEFAULT_STAKE,
        };
        let genesis = genesis_tx(&b, &lobby, &f, &key(4), 100_000).unwrap();
        let lobby = accepted_cell(&genesis, 0, 99, lobby);
        assert!(start_tx(&b, &lobby, &key(1), &f, &key(4), 100_000).is_err());
        let (start, p) = start_tx(&b, &lobby, &key(2), &f, &key(4), 100_000).unwrap();
        accepted_plan(&start, p, 100).unwrap();
        assert!(artifact().sil_abi.contract("Court").unwrap().entry("cancel").is_none());
        assert!(artifact().sil_abi.contract("Lobby").unwrap().entry("cancel").is_some());
    }

    mod wager_payout {
        use super::*;

        #[test]
        fn unjoined_lobby_returns_only_host_principal_and_live_contact_preserves_the_pot() {
            assert_eq!(DEFAULT_STAKE, 500_000_000, "fixed 5 KAS per player");
            let b = TxBuilder::new(artifact()).unwrap();
            let lobby = Lobby { left: pubkey(&key(1)), right: pubkey(&key(2)), refund: pubkey(&key(3)),
                control_value: SIDE_DEPOSIT as i64, ball_tick_daa: DEFAULT_BALL_TICK_DAA, stake_per_player: DEFAULT_STAKE };
            let genesis = genesis_tx(&b, &lobby, &funding(), &key(4), 100_000).unwrap();
            let cell = accepted_cell(&genesis, 0, 99, lobby);
            let ctx = argent_runtime::TxContext::new().actor_input("Lobby", cell.state.state(),
                EntryCall::new("cancel").args_with(|tx, input| argent_runtime::args![sign(tx, input, &key(3))]),
                cell.outpoint, cell.utxo.clone(), 0).output(p2pk(&pubkey(&key(3))), None, cell.utxo.amount);
            let cancel = build(&b, &funded(ctx, &funding(), &key(4), 100_000).unwrap()).unwrap();
            assert_eq!(cancel.inputs.len(), 2, "no guest coin was ever locked");
            assert_eq!(cancel.outputs[0].value, 500_000_000, "host receives its 5 KAS stake");
            // Bypass the client helper to prove Lobby.start itself rejects
            // overfunded principal, even with the invited guest's valid signature.
            let mut overfunded = cell.clone(); overfunded.utxo.amount += 1;
            let plan = cell.state.opening();
            let id = cell.utxo.covenant_id.unwrap();
            let mut forged = argent_runtime::TxContext::new().actor_input("Lobby", overfunded.state.state(),
                EntryCall::new("start").args_with(|tx, input| argent_runtime::args![sign(tx, input, &key(2))]),
                overfunded.outpoint, overfunded.utxo, 0)
                .actor_output("Court", plan.world.state(), kaspa_consensus_core::tx::CovenantBinding::new(0,id), COURT_VALUE);
            for q in &plan.queues {
                forged = forged.actor_output("Player", q.state(), kaspa_consensus_core::tx::CovenantBinding::new(0,id), QUEUE_DEPOSIT);
            }
            assert!(b.build(&funded(forged, &funding(), &key(4), DEFAULT_STAKE as u64 + 100_000).unwrap()).is_err());
            let s = fixture();
            let mut changed = resolve(&s).unwrap(); changed.world.stake_per_player += 1;
            assert!(b.build(&resolve_context(&s, &changed, &key(2)).unwrap()).is_err());
            let plan = resolve(&s).unwrap();
            let mut ctx = resolve_context(&s, &plan, &key(2)).unwrap();
            assert_eq!(ctx.outputs[0].value, COURT_VALUE, "Court preserves its 4 KAS share of the pot");
            ctx.outputs[0].value -= 1;
            assert!(b.build(&ctx).is_err(), "a live transition cannot consume any escrow principal");
        }

        fn check_payout(s: &Snapshot, tx: &Transaction, finish: Finish, winner: i64) {
            let b = TxBuilder::new(artifact()).unwrap();
            let closed = accepted_collection(s, tx, finish, 300);
            let payout = reclaim_tx(&b, &closed, &funding(), &key(4), 100_000).unwrap();
            let entries = || std::iter::once(closed.collector.utxo.clone())
                .chain(closed.players.iter().map(|q| q.utxo.clone())).chain(std::iter::once(funding().utxo)).collect::<Vec<_>>();
            assert_eq!(closed.collector.utxo.amount, COURT_VALUE);
            assert_eq!(payout.outputs[0].value, GAME_POT, "winner receives the whole 10 KAS pot");
            assert_eq!(payout.outputs[0].script_public_key, p2pk(if winner == 0 { &s.court.state.left } else { &s.court.state.right }));
            assert_eq!(entries().iter().map(|u| u.amount).sum::<u64>() - payout.outputs.iter().map(|o| o.value).sum::<u64>(), 100_000);
            assert!(payout.storage_mass() <= 500_000);
            let mut extra_principal = entries(); extra_principal[1].amount += 1;
            assert!(execute_input_with_covenants(&payout, extra_principal, 0).is_err(), "Collector requires exactly the pot, not extra collateral");
            for index in [0] {
                let mut stolen = payout.clone();
                stolen.outputs[index].value -= 1;
                assert!(execute_input_with_covenants(&stolen, entries(), 0).is_err(), "principal cannot become fees");
                stolen = payout.clone();
                stolen.outputs[index].script_public_key = p2pk(&pubkey(&key(8)));
                assert!(execute_input_with_covenants(&stolen, entries(), 0).is_err(), "payout cannot be redirected");
            }
        }

        #[test]
        fn both_score_winners_receive_the_whole_pot_without_a_collateral_output() {
            for winner in [0, 1] {
                let mut s = fixture();
                if winner == 0 { s.court.state.left_score = WIN_SCORE; } else { s.court.state.right_score = WIN_SCORE; }
                materialize(&mut s);
                let b = TxBuilder::new(artifact()).unwrap();
                let tx = settle_tx(&b, &s, &funding(), &key(4), 100_000).unwrap();
                check_payout(&s, &tx, s.court.state.result(), winner);
            }
        }

        #[test]
        fn timeout_pays_the_actual_winner_without_fabricating_a_score() {
            for winner in [0, 1] {
                let mut s = fixture();
                s.court.state.ball_vx = if winner == 0 { 1 } else { -1 };
                materialize(&mut s);
                let b = TxBuilder::new(artifact()).unwrap();
                let (tx, finish) = timeout_tx(&b, &s, &key(winner as u8 + 1), &funding(), &key(4), 100_000).unwrap();
                assert_eq!((finish.winner, finish.left_score, finish.right_score), (winner, 0, 0));
                check_payout(&s, &tx, finish, winner);
            }
        }

        #[test]
        fn collector_rejects_the_removed_cancellation_result() {
            let s = fixture();
            let b = TxBuilder::new(artifact()).unwrap();
            let (tx, finish) = timeout_tx(&b, &s, &key(1), &funding(), &key(4), 100_000).unwrap();
            let mut closed = accepted_collection(&s, &tx, finish, 300);
            closed.collector.state.finish.winner = -1;
            closed.collector.utxo = b.covenant_utxo("Collector", closed.collector.state.state(), COURT_VALUE, 300, false, closed.collector.utxo.covenant_id).unwrap();
            assert!(reclaim_tx(&b, &closed, &funding(), &key(4), 100_000).is_err());
            let mut ctx = argent_runtime::TxContext::new().actor_input("Collector", closed.collector.state.state(), "collect", closed.collector.outpoint, closed.collector.utxo.clone(), 0);
            for q in &closed.players { ctx = ctx.actor_input("Player", q.state.state(), "collect", q.outpoint, q.utxo.clone(), 0); }
            ctx = ctx.output(p2pk(&closed.collector.state.left), None, DEFAULT_STAKE as u64)
                .output(p2pk(&closed.collector.state.right), None, DEFAULT_STAKE as u64);
            assert!(b.build(&funded(ctx, &funding(), &key(4), 100_000).unwrap()).is_err(), "covenant itself rejects winner=-1");
        }
    }

    #[test]
    fn collection_split_authorization_contact_and_refund_security() {
        contact_protocol_launch_requires_guest_and_has_no_live_cancel();
        contact_protocol_idle_match_finishes_and_collects_all_principal();
        let b = TxBuilder::new(artifact()).unwrap();
        let s = fixture();
        let (tx, finish) = timeout_tx(&b, &s, &key(1), &funding(), &key(4), 100_000).unwrap();
        assert_eq!(tx.inputs.len(), 2, "timeout does not gather queues");
        let closed = accepted_collection(&s, &tx, finish, 101);
        let refund = reclaim_tx(&b, &closed, &funding(), &key(4), 100_000).unwrap();
        assert_eq!(refund.inputs.len(), 8, "collector, all six queues, fee");
        let entries = || std::iter::once(closed.collector.utxo.clone())
            .chain(closed.players.iter().map(|q| q.utxo.clone())).chain(std::iter::once(funding().utxo)).collect::<Vec<_>>();
        let mut forged = refund.clone();
        forged.outputs[0].value -= 1;
        // Check Collector itself: an invalid fee signature must not mask a missing
        // covenant refund restriction after changing a signed transaction's output.
        assert!(execute_input_with_covenants(&forged, entries(), 0).is_err());
        let mut forged = refund.clone();
        forged.outputs[0].script_public_key = p2pk(&pubkey(&key(4)));
        assert!(execute_input_with_covenants(&forged, entries(), 0).is_err());
        let mut missing = closed.clone();
        missing.players[5] = missing.players[3].clone();
        assert!(reclaim_tx(&b, &missing, &funding(), &key(4), 100_000).is_err());
        let mut foreign = closed.clone();
        foreign.players[5].utxo.covenant_id = Some(Hash::from_bytes([88; 32]));
        assert!(reclaim_tx(&b, &foreign, &funding(), &key(4), 100_000).is_err());
        let mut wrong_owner = closed.clone();
        wrong_owner.players[5].state.owner = pubkey(&key(1));
        wrong_owner.players[5].utxo = b.covenant_utxo("Player", wrong_owner.players[5].state.state(), QUEUE_DEPOSIT, 100, false,
            wrong_owner.players[5].utxo.covenant_id).unwrap();
        assert!(reclaim_tx(&b, &wrong_owner, &funding(), &key(4), 100_000).is_err());
        // Queue appends already in flight remain collectible after closing.
        let mut late = closed.clone();
        let (command, q) = command_tx(&b, &late.players[0], 1, &key(1), &funding(), &key(4), 100_000).unwrap();
        late.players[0] = accepted_cell(&command, 0, 102, q);
        assert_eq!(reclaim_tx(&b, &late, &funding(), &key(4), 100_000).unwrap().outputs[0].value, GAME_POT);
    }

    #[test]
    fn contact_protocol_idle_match_finishes_and_collects_all_principal() {
        let mut s = fixture();
        s.court.state.ball_y = 0;
        materialize(&mut s);
        let b = TxBuilder::new(artifact()).unwrap();
        assert!(settle_tx(&b, &s, &funding(), &key(4), 100_000).is_err());
        for _ in 0..30 {
            let (side, t) = next_contact(&s).unwrap();
            let w = &s.court.state;
            let ticks = (if w.ball_vx == 1 { RIGHT_CONTACT_X - w.ball_x } else { w.ball_x - LEFT_CONTACT_X } + 1) / 2;
            let phase = (w.ball_y + w.ball_vy * ticks).rem_euclid(62);
            let arrival = if phase <= 31 { phase } else { 62 - phase };
            let target = if arrival < 16 { PADDLE_MAX_Y } else { PADDLE_MIN_Y };
            let movement = target - s.players[side].state.y;
            if movement != 0 {
                command(&mut s, side, movement, (t - 1) as u64);
            }
            let before = s.clone();
            let mut tx = contact(&mut s, t as u64 + 2);
            execute_transaction_with_covenants(
                &mut tx,
                vec![
                    before.court.utxo.clone(),
                    before.players[side].utxo.clone(),
                    before.players[side + 2].utxo.clone(),
                    before.players[side + 4].utxo.clone(),
                    funding().utxo,
                ],
            )
            .unwrap();
            if s.court.state.ended() {
                let settle = settle_tx(&b, &s, &funding(), &key(4), 100_000).unwrap();
                assert_eq!(settle.inputs.len(), 2, "Court authorization and fee only");
                assert_eq!(settle.outputs[0].value, COURT_VALUE);
                let f = accepted_collection(&s, &settle, s.court.state.result(), t as u64 + 3);
                let refund = reclaim_tx(&b, &f, &funding(), &key(4), 100_000).unwrap();
                assert_eq!(refund.outputs[0].value, GAME_POT);
                println!(
                    "contact {} bytes, authorization {} bytes, collection/refund {} bytes",
                    transaction_estimated_serialized_size(&tx),
                    transaction_estimated_serialized_size(&settle),
                    transaction_estimated_serialized_size(&refund)
                );
                return;
            }
        }
        panic!("idle match did not finish");
    }

    #[test]
    fn geometry_58x32_contacts_walls_scoring_and_center_serves_agree() {
        assert_eq!((COURT_WIDTH, COURT_HEIGHT, LEFT_CONTACT_X, RIGHT_CONTACT_X, CENTER_X), (58, 32, 2, 55, 29));
        assert_eq!((PADDLE_RADIUS, BALL_SPEED, DEFAULT_BALL_TICK_DAA, OPENING_DELAY_DAA, SERVE_DELAY_DAA), (4, 2, 7, 25, 20));
        assert_eq!((PADDLE_MIN_Y, PADDLE_MAX_Y), (4, 27));
        let b = TxBuilder::new(artifact()).unwrap();
        let f = funding();
        let lobby = Lobby {
            left: pubkey(&key(1)), right: pubkey(&key(2)), refund: pubkey(&key(3)),
            control_value: SIDE_DEPOSIT as i64, ball_tick_daa: DEFAULT_BALL_TICK_DAA,
            stake_per_player: DEFAULT_STAKE,
        };
        let genesis = genesis_tx(&b, &lobby, &f, &key(4), 100_000).unwrap();
        let lobby = accepted_cell(&genesis, 0, 99, lobby);
        let (tx, p) = start_tx(&b, &lobby, &key(2), &f, &key(4), 100_000).unwrap();
        let opening = accepted_plan(&tx, p, 100).unwrap();
        assert_eq!(opening.court.state.ball_x, CENTER_X);
        let serving = project(&opening, 125).unwrap().world;
        assert_eq!((serving.ball_x, serving.serve_until), (CENTER_X, 125));
        let (side, deadline) = next_contact(&opening).unwrap();
        assert_eq!(deadline, 125 + if side == 1 { 13 } else { 14 } * DEFAULT_BALL_TICK_DAA);

        // Center-to-contact flight, both hits, both goals, and top/bottom reflections.
        for (side, y, vy, paddle, miss) in [
            (0, 16, 0, 16, false), (1, 16, 0, 16, false),
            (0, 2, 0, 16, true), (1, 2, 0, 16, true),
            (0, 1, -2, PADDLE_MIN_Y, false), (1, 30, 2, PADDLE_MAX_Y, false),
        ] {
            let mut s = fixture();
            let reflection = vy != 0;
            let w = &mut s.court.state;
            w.ball_x = if reflection { if side == 0 { LEFT_CONTACT_X + BALL_SPEED } else { RIGHT_CONTACT_X - BALL_SPEED } } else { CENTER_X };
            w.ball_vx = if side == 0 { -1 } else { 1 };
            w.ball_y = y;
            w.ball_vy = vy;
            s.players[side].state.y = paddle;
            materialize(&mut s);
            let (_, t) = next_contact(&s).unwrap();
            assert_eq!(t - 100, if reflection { 2 } else if side == 1 { 26 } else { 28 });
            let expected = project(&s, t as u64).unwrap().world;
            let p = resolve(&s).unwrap();
            assert_eq!((p.world.ball_x, p.world.ball_y, p.world.ball_vx, p.world.ball_vy),
                (expected.ball_x, expected.ball_y, expected.ball_vx, expected.ball_vy));
            let tx = contact(&mut s, t as u64 + 1);
            assert_eq!(tx.inputs.len(), 5);
            if miss {
                assert_eq!((s.court.state.ball_x, s.court.state.serve_until), (CENTER_X, t + SERVE_DELAY_DAA));
                assert_eq!((s.court.state.left_score, s.court.state.right_score), if side == 1 { (1, 0) } else { (0, 1) });
            } else {
                assert_eq!(s.court.state.ball_x, if side == 0 { LEFT_CONTACT_X } else { RIGHT_CONTACT_X });
                assert_eq!(next_contact(&s).unwrap(), (1 - side, t + 54));
                if reflection { assert_eq!(s.court.state.ball_y, y); }
            }
        }
        let board = kaspa_pong::ascii::frame(&opening, 125, "geometry").unwrap();
        assert!(board.contains(&format!("+{}+", "-".repeat(COURT_WIDTH as usize))));
    }

    #[test]
    fn contact_protocol_matches_independent_tick_oracle() {
        // Tick-by-tick oracle deliberately avoids the analytic flight/reflection
        // implementation and checks both travel directions, every slope, boundary
        // reflections, all nine paddle cells, finite-ball edge hits, misses and serves.
        let b = TxBuilder::new(artifact()).unwrap();
        for sample in 0..160 {
            let mut s = fixture();
            let w = &mut s.court.state;
            w.ball_vx = if sample % 2 == 0 { 1 } else { -1 };
            w.ball_vy = (sample / 2 % 5) as i64 - 2;
            w.ball_y = (sample * 17 % 32) as i64;
            w.ball_tick_daa = [2, 8, 16, 64][sample % 4];
            w.serve_until = if sample % 3 == 0 { 107 } else { 0 };
            let (side, t) = next_contact(&s).unwrap();
            let mut oracle = s.court.state.clone();
            let mut arrival = oracle.ball_y;
            let mut vx = oracle.ball_vx;
            let mut vy = oracle.ball_vy;
            let mut x = oracle.ball_x;
            for at in oracle.clock + 1..=t {
                if at > oracle.serve_until && (at - oracle.serve_until) % oracle.ball_tick_daa == 0 {
                    x = (x + vx * BALL_SPEED).clamp(LEFT_CONTACT_X, RIGHT_CONTACT_X);
                    arrival += vy;
                    if arrival < 0 {
                        arrival = -arrival;
                        vy = -vy;
                    }
                    if arrival > 31 {
                        arrival = 62 - arrival;
                        vy = -vy;
                    }
                    if arrival == 0 {
                        vy = vy.abs();
                    }
                    if arrival == 31 {
                        vy = -vy.abs();
                    }
                }
            }
            let paddle = (arrival + (sample as i64 % 13 - 6)).clamp(PADDLE_MIN_Y, PADDLE_MAX_Y);
            s.players[side].state.y = paddle;
            s.players[side].state.through = t;
            s.players[side].utxo.block_daa_score = t as u64;
            let offset = arrival - paddle;
            // The unchanged four-dot-high sprite has a one-cell vertical radius.
            if offset.abs() > PADDLE_RADIUS + BALL_RADIUS {
                if side == 1 {
                    oracle.left_score += 1;
                } else {
                    oracle.right_score += 1;
                }
                x = CENTER_X;
                oracle.serve_until = t + SERVE_DELAY_DAA;
                let choice = serve_choice(t, oracle.left_score + oracle.right_score, &oracle.left, &oracle.right);
                arrival = 2 + choice / 10;
                vx = if choice % 2 == 0 { -1 } else { 1 };
                vy = choice / 2 % 5 - 2;
            } else {
                vx = -vx;
                vy = offset.clamp(-2, 2);
            }
            materialize(&mut s);
            let p = resolve(&s).unwrap();
            assert_eq!(
                (p.world.ball_x, p.world.ball_y, p.world.ball_vx, p.world.ball_vy, p.world.clock),
                (x, arrival, vx, vy, t),
                "sample {sample}"
            );
            assert_eq!(
                (p.world.left_score, p.world.right_score, p.world.serve_until),
                (oracle.left_score, oracle.right_score, oracle.serve_until)
            );
            b.build(&resolve_context(&s, &p, &key(side as u8 + 1)).unwrap()).unwrap();
        }
    }

    #[test]
    fn current_artifact_compilation_is_reproducible_and_identity_is_pinned() {
        let a = artifact();
        let scratch = tempfile::tempdir().unwrap();
        let rebuilt = argent::build_file(concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/pong.ag"), scratch.path()).unwrap();
        assert_eq!(a.id, rebuilt.id);
        assert_eq!(a.id.to_string(), GAME_ARTIFACT_ID);
    }

    #[test]
    fn contact_protocol_security_signatures_values_and_foreign_lanes() {
        let s = fixture();
        let b = TxBuilder::new(artifact()).unwrap();
        for i in 0..QUEUE_COUNT {
            assert!(command_tx(&b, &s.players[i], 1, &key(8), &funding(), &key(4), 100_000).is_err());
            for d in [-28, 0, 28] {
                assert!(command_tx(&b, &s.players[i], d, &key(i as u8 % 2 + 1), &funding(), &key(4), 100_000).is_err());
            }
        }
        let p = checkpoint_plan(&s, 1, 130).unwrap();
        for i in 0..4 {
            let mut ctx = checkpoint_context(&s, 1, 130, &p, &key(2)).unwrap();
            ctx.outputs[i].value -= 1;
            assert!(b.build(&ctx).is_err(), "checkpoint output {i} must preserve principal");
        }
        let mut foreign = s.clone();
        foreign.players[5].utxo.covenant_id = Some(Hash::from_bytes([99; 32]));
        assert!(b.build(&checkpoint_context(&foreign, 1, 130, &p, &key(2)).unwrap()).is_err());
        let p = resolve(&s).unwrap();
        for i in 0..2 {
            let mut ctx = resolve_context(&s, &p, &key(2)).unwrap();
            ctx.outputs[i].value -= 1;
            assert!(b.build(&ctx).is_err(), "contact output {i} must preserve principal");
        }
    }

    #[test]
    fn contact_protocol_security_opponent_history_survives_unrelated_contact() {
        let mut s = fixture();
        command(&mut s, 0, 10, 110);
        let second_contact = 130 + ((RIGHT_CONTACT_X - LEFT_CONTACT_X + BALL_SPEED - 1) / BALL_SPEED) * s.court.state.ball_tick_daa;
        let expected = project(&s, second_contact as u64).unwrap().world;
        let original = s.players[0].clone();
        contact(&mut s, 131);
        assert_eq!(s.players[0].outpoint, original.outpoint);
        assert_eq!(s.players[0].utxo.block_daa_score, 110);
        assert_eq!(next_contact(&s).unwrap(), (0, second_contact));
        contact(&mut s, second_contact as u64 + 1);
        assert_eq!((s.court.state.left_score, s.court.state.right_score), (expected.left_score, expected.right_score));
        assert_eq!(
            (s.court.state.ball_x, s.court.state.ball_y, s.court.state.ball_vx, s.court.state.ball_vy),
            (expected.ball_x, expected.ball_y, expected.ball_vx, expected.ball_vy)
        );
        assert_eq!(s.players[0].state.y, 26);
    }
}

mod timeouts {
    //! Accepted progress freshness, bounded receiver catch-up, and exact replay.

    use argent_runtime::{Artifact, ContextInput, TxBuilder};
    use kaspa_consensus_core::{Hash, tx::{Transaction, TransactionOutpoint, UtxoEntry}};
    use kaspa_pong::engine::*;
    use secp256k1::{Keypair, Secp256k1, SecretKey};
    use std::sync::OnceLock;

    fn artifact() -> &'static Artifact {
        static A: OnceLock<Artifact> = OnceLock::new();
        A.get_or_init(|| serde_json::from_str(include_str!("../artifacts/artifact.json")).unwrap())
    }
    fn key(n: u8) -> Keypair {
        Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
    }
    fn hash(n: u8) -> Hash { Hash::from_bytes([n; 32]) }
    fn out(n: u8) -> TransactionOutpoint { TransactionOutpoint::new(hash(n), 0) }
    fn funding() -> Funding {
        Funding { outpoint: out(90), utxo: UtxoEntry::new(100_000_000_000, p2pk(&pubkey(&key(4))), 1, false, None) }
    }
    fn fixture(receiver: usize, deadline: i64) -> Snapshot {
        let b = TxBuilder::new(artifact()).unwrap();
        let mut w = World::opening(pubkey(&key(1)), pubkey(&key(2)));
        w.started = true;
        w.clock = deadline - 30;
        w.serve_until = w.clock; // Align the two-DAA tick phase for odd absolute deadlines too.
        w.ball_tick_daa = 2;
        w.ball_vx = if receiver == 1 { 1 } else { -1 };
        w.ball_x = if receiver == 1 { RIGHT_CONTACT_X - 30 } else { LEFT_CONTACT_X + 30 };
        w.left_score = 2;
        w.right_score = 2;
        let qs = empty_queues(&w.left, &w.right);
        let born = w.clock as u64;
        Snapshot {
            court: Cell { outpoint: out(1), utxo: b.covenant_utxo("Court", w.state(), COURT_VALUE,
                born, false, Some(hash(77))).unwrap(), state: w },
            players: std::array::from_fn(|i| Cell { outpoint: out(i as u8 + 2),
                utxo: b.covenant_utxo("Player", qs[i].state(), QUEUE_DEPOSIT, born, false, Some(hash(77))).unwrap(),
                state: qs[i].clone() }),
        }
    }
    fn command(s: &mut Snapshot, lane: usize, direction: i64, at: u64) {
        let b = TxBuilder::new(artifact()).unwrap();
        let (tx, q) = command_tx(&b, &s.players[lane], direction, &key((lane % 2 + 1) as u8), &funding(), &key(4), 100_000).unwrap();
        s.players[lane] = accepted_cell(&tx, 0, at, q);
    }
    fn checkpoint(s: &mut Snapshot, side: usize, until: i64, at: u64) -> Transaction {
        let b = TxBuilder::new(artifact()).unwrap();
        let (tx, plan) = checkpoint_tx(&b, s, side, until, &key(side as u8 + 1), &funding(), &key(4), 100_000).unwrap();
        *s = accepted_transition(&b, s, &tx, plan, at).unwrap();
        tx
    }
    fn queued(s: &Snapshot, side: usize) -> usize {
        (side..QUEUE_COUNT).step_by(2).map(|i| s.players[i].state.count).sum()
    }
    fn prefix_history(s: &mut Snapshot, side: usize, count: usize, deadline: i64) {
        for n in 0..count {
            command(s, side + 2 * (n % 3), if n % 2 == 0 { 1 } else { -1 }, (deadline - count as i64 + n as i64) as u64);
        }
    }
    fn assert_timeout_guards(s: &Snapshot, expiry: i64) {
        let b = TxBuilder::new(artifact()).unwrap();
        let result = timeout_result(s).unwrap();
        assert_eq!(result.end_daa, expiry);
        let owner = key(result.winner as u8 + 1);
        let age = (expiry - s.court.utxo.block_daa_score as i64).max(0) as u64;
        assert!(age > 0);
        for sequence in [age - 1, age | (1_u64 << 63)] {
            let mut ctx = timeout_context(s, &owner).unwrap();
            if let ContextInput::Actor(input) = &mut ctx.inputs[0] { input.sequence = sequence; }
            assert!(b.build(&ctx).is_err(), "early/disabled age cannot claim accepted progress");
        }
        for wrong in [key(2 - result.winner as u8), key(8)] {
            assert!(b.build(&timeout_context(s, &wrong).unwrap()).is_err());
        }
        let (tx, finish) = timeout_tx(&b, s, &owner, &funding(), &key(4), 100_000).unwrap();
        assert_eq!((finish.left_score, finish.right_score), (2, 2));
        assert_eq!(tx.inputs[0].sequence, age);
        let closed = accepted_collection(s, &tx, finish, expiry as u64);
        let payout = reclaim_tx(&b, &closed, &funding(), &key(4), 100_000).unwrap();
        assert_eq!(payout.outputs[0].value, GAME_POT);
        assert_eq!(payout.outputs[0].script_public_key, p2pk(&pubkey(&owner)));
    }

    #[test]
    fn observed_receiver_prefix_keeps_precontact_027_and_late_030_041_without_forfeit_at_098() {
        let base = 603_785_000;
        let mut s = fixture(1, base + 29);
        for (lane, offset, direction) in [(1,1,1), (3,6,-1), (5,11,1), (1,16,-1), (3,27,1), (5,30,-1), (1,41,1)] {
            command(&mut s, lane, direction, (base + offset) as u64);
        }
        let b = TxBuilder::new(artifact()).unwrap();
        let old_claim = timeout_tx(&b, &s, &key(1), &funding(), &key(4), 100_000).unwrap().0;
        checkpoint(&mut s, 1, base + 29, (base + 66) as u64);
        assert_eq!(s.court.state.resolves, 0);
        assert_eq!(queued(&s, 1), 3);
        let mut remaining: Vec<_> = [1,3,5].into_iter().flat_map(|i| s.players[i].state.times[..s.players[i].state.count].to_vec()).collect();
        remaining.sort();
        assert_eq!(remaining, [base + 27, base + 30, base + 41]);
        assert_eq!(s.court.state.response_at, -1);
        assert_timeout_guards(&s, base + 116);
        let mut too_early = timeout_context(&s, &key(1)).unwrap();
        if let ContextInput::Actor(input) = &mut too_early.inputs[0] { input.sequence = 98 - 66; }
        assert!(b.build(&too_early).is_err(), "observed acceptance at 098 no longer forfeits an active receiver");
        assert_ne!(old_claim.inputs[0].previous_outpoint, s.court.outpoint);
        checkpoint(&mut s, 1, base + 29, (base + 100) as u64);
        assert_eq!(s.court.state.resolves, 1);
        assert_eq!(queued(&s, 1), 2, "post-contact commands remain queued for the next flight");
        assert_eq!((s.court.state.left_score, s.court.state.right_score), (2, 2));
    }

    #[test]
    fn all_twelve_precontact_batches_drain_in_finite_prefixes_for_both_receivers() {
        for side in [0, 1] {
            let mut s = fixture(side, 130);
            prefix_history(&mut s, side, 12, 130);
            for (at, remaining) in [(167,8), (201,4), (235,0)] {
                checkpoint(&mut s, side, 130, at);
                assert_eq!(queued(&s, side), remaining);
                assert_eq!(s.court.state.response_at, -1);
                if remaining > 0 {
                    assert_eq!(s.court.state.resolves, 0);
                    assert_timeout_guards(&s, at as i64 + 50);
                }
            }
            assert_eq!(s.court.state.resolves, 1);
            assert_eq!(next_contact(&s).unwrap().0, 1 - side);
        }
    }

    #[test]
    fn nonreceiver_consumption_watermark_noop_and_late_commands_do_not_renew_anchor() {
        let b = TxBuilder::new(artifact()).unwrap();
        for receiver in [0, 1] {
            let mut s = fixture(receiver, 130);
            let other = 1 - receiver;
            prefix_history(&mut s, receiver, 6, 130);
            command(&mut s, other, 1, 120);
            checkpoint(&mut s, receiver, 130, 150);
            assert_eq!(timeout_result(&s).unwrap().end_daa, 200);
            let mut forged = checkpoint_plan(&s, other, 130).unwrap();
            assert_eq!(forged.world.response_at, 150);
            forged.world.response_at = -1;
            assert!(b.build(&checkpoint_context(&s, other, 130, &forged, &key(other as u8 + 1)).unwrap()).is_err(),
                "even real nonreceiver consumption cannot reset freshness");
            checkpoint(&mut s, other, 130, 170);
            assert_eq!(s.court.state.response_at, 150);
            assert_eq!(timeout_result(&s).unwrap().end_daa, 200);
            assert!(checkpoint_plan(&s, other, 130).is_err());
            let unchanged = Plan { world: s.court.state.clone(), queues: std::array::from_fn(|i| s.players[i].state.clone()) };
            assert!(b.build(&checkpoint_context(&s, other, 130, &unchanged, &key(other as u8 + 1)).unwrap()).is_err());
            command(&mut s, receiver + 4, 1, 175);
            assert_eq!(timeout_result(&s).unwrap().end_daa, 200, "accepted late input is not pre-contact progress");
            assert_timeout_guards(&s, 200);

            // Receiver watermark-only progress is legal but must not buy freshness.
            let mut empty = fixture(receiver, 130);
            let mut forged = checkpoint_plan(&empty, receiver, 110).unwrap();
            forged.world.response_at = -1;
            assert!(b.build(&checkpoint_context(&empty, receiver, 110, &forged, &key(receiver as u8 + 1)).unwrap()).is_err());
            checkpoint(&mut empty, receiver, 110, 190);
            assert_eq!(empty.court.state.response_at, 100);
            assert_eq!(timeout_result(&empty).unwrap().end_daa, 180);
            command(&mut empty, receiver, 1, 191);
            assert_eq!(timeout_result(&empty).unwrap().end_daa, 180);
            let mut unaccepted = empty.clone();
            unaccepted.court.utxo.block_daa_score = u64::MAX;
            assert!(timeout_result(&unaccepted).is_err());
        }
    }

    #[test]
    fn delayed_previous_contact_grants_new_receiver_fifty_daa_from_confirmed_flight() {
        for receiver in [0, 1] {
            let mut s = fixture(receiver, 130);
            checkpoint(&mut s, receiver, 130, 500);
            let (next_receiver, deadline) = next_contact(&s).unwrap();
            assert_eq!(next_receiver, 1 - receiver);
            assert!(deadline + 50 < 500, "entire projected next flight expired before its confirmation");
            assert_eq!(s.court.state.response_at, -1);
            assert_timeout_guards(&s, 550);
            checkpoint(&mut s, receiver, deadline, 510);
            assert_eq!(s.court.state.response_at, 500);
            assert_timeout_guards(&s, 550);
            checkpoint(&mut s, next_receiver, deadline, 560);
            assert_eq!(s.court.state.resolves, 2, "a valid late proof remains possible");
            assert!(timeout_result(&s).unwrap().end_daa >= 610);
        }
    }

    #[test]
    fn opening_anchor_survives_unrelated_replacement_and_forged_anchor_is_rejected() {
        let b = TxBuilder::new(artifact()).unwrap();
        let mut s = fixture(1, 130);
        s.court.state.started = false;
        s.court.state.clock = 0;
        s.court.state.ball_x = CENTER_X;
        s.court.utxo = b.covenant_utxo("Court", s.court.state.state(), s.court.utxo.amount, 100, false, s.court.utxo.covenant_id).unwrap();
        let (receiver, deadline) = next_contact(&s).unwrap();
        let expiry = timeout_result(&s).unwrap().end_daa;
        checkpoint(&mut s, 1 - receiver, 110, (expiry + 10) as u64);
        assert_eq!(s.court.state.response_at, 100);
        assert_eq!(next_contact(&s).unwrap(), (receiver, deadline));
        assert_eq!(timeout_result(&s).unwrap().end_daa, expiry);
        for response_at in [-2, expiry + 11] {
            let mut bad = s.clone();
            bad.court.state.response_at = response_at;
            assert!(timeout_result(&bad).is_err());
        }
    }

    #[cfg(feature = "node")]
    mod client {
        use super::*;
        use kaspa_pong::node::{self, client::{Action, Cache, action, receiving_contact_due}, tracker::*};

        fn invite() -> Invitation {
            let lobby = Lobby { left: pubkey(&key(1)), right: pubkey(&key(2)), refund: pubkey(&key(3)),
                control_value: SIDE_DEPOSIT as i64, ball_tick_daa: DEFAULT_BALL_TICK_DAA, stake_per_player: DEFAULT_STAKE };
            kaspa_pong::node::invitations::create(artifact(), "devnet", hash(0), &lobby, &funding(), &key(1), &key(4), 100_000).unwrap()
        }
        fn cache(snapshot: Snapshot) -> (Invitation, Cache) {
            let invite = invite();
            let mut cache = Cache::new(&invite);
            cache.tracker.stage = Stage::Live(snapshot);
            cache.tracker.daa = 125;
            (invite, cache)
        }
        fn live(cache: &Cache) -> &Snapshot {
            let Stage::Live(s) = &cache.tracker.stage else { panic!("expected live game") };
            s
        }
        fn movement(cache: &Cache, lane: usize, direction: i64) -> AcceptedTx {
            let s = live(cache);
            let f = funding();
            let (tx, _) = command_tx(&TxBuilder::new(artifact()).unwrap(), &s.players[lane], direction,
                &key((lane % 2 + 1) as u8), &f, &key(4), 100_000).unwrap();
            AcceptedTx { tx, inputs: vec![s.players[lane].utxo.clone(), f.utxo] }
        }
        fn prefix(cache: &Cache, side: usize, until: i64) -> AcceptedTx {
            let s = live(cache);
            let f = funding();
            let (tx, _) = checkpoint_tx(&TxBuilder::new(artifact()).unwrap(), s, side, until,
                &key(side as u8 + 1), &f, &key(4), 100_000).unwrap();
            AcceptedTx { tx, inputs: vec![s.court.utxo.clone(), s.players[side].utxo.clone(),
                s.players[side + 2].utxo.clone(), s.players[side + 4].utxo.clone(), f.utxo] }
        }
        fn accept(cache: &mut Cache, invite: &Invitation, id: u8, daa: u64, tx: AcceptedTx) -> Vec<node::audit::TransactionRecord> {
            cache.tracker.apply_with_events(artifact(), invite, &[], &[ChainBlock { hash: hash(id), daa, transactions: vec![tx] }]).unwrap()
        }

        #[test]
        fn due_receiver_stops_move_tail_and_defers_prepared_moves_without_charging_or_reserving() {
            for receiver in [0, 1] {
                let (invite, mut cache) = cache(fixture(receiver, 130));
                let config = node::Config::default();
                let first = movement(&cache, receiver, 1);
                let prepared = movement(&cache, receiver + 2, -1);
                cache.authorize(first.tx.clone(), first.inputs.clone(), "move".into(), &config).unwrap();
                assert!(cache.available_lane(receiver as i64).is_some());
                assert_eq!(cache.next_action(receiver as i64, 129, Some(1)).unwrap(), Some(Action::Move(1)));
                assert_eq!(cache.next_action(receiver as i64, 130, Some(1)).unwrap(), None);
                assert!(receiving_contact_due(&cache.tracker.stage, receiver as i64, 130).unwrap());
                let fees = cache.authorized_fees;
                cache.tracker.daa = 130;
                assert!(cache.authorize(prepared.tx.clone(), prepared.inputs.clone(), "move".into(), &config).is_err());
                assert_eq!(cache.authorized_fees, fees);
                assert_eq!(cache.pending.len(), 1);
                assert_eq!(cache.pending[0].transaction.id(), first.tx.id());
                assert!(cache.draft_current(&prepared.tx, &prepared.inputs), "deferral does not reserve an unauthorized draft");
                let events = accept(&mut cache, &invite, 10, 131, first);
                let tracker = cache.tracker.clone();
                cache.commit_sync(tracker, vec![], events, true, None);
                // The normal validated publication/reconciliation clears accepted pending work.
                cache.discard_conflicting_pending();
                assert!(cache.pending.is_empty());
                assert_eq!(cache.next_action(receiver as i64, 131, Some(-1)).unwrap(), Some(Action::Resolve(false)));
                assert_eq!(action(&cache.tracker.stage, (1 - receiver) as i64, 180, None).unwrap(), Some(Action::Timeout));
            }
        }

        #[test]
        fn reorg_retimes_only_real_progress_and_cache_timeout_late_command_payout_remain_coherent() {
            let mut s = fixture(1, 130);
            prefix_history(&mut s, 1, 6, 130);
            let (invite, mut cache) = cache(s);
            let initial = live(&cache).clone();
            let progress = prefix(&cache, 1, 130);
            let events = accept(&mut cache, &invite, 10, 150, progress.clone());
            assert_eq!((events[0].event.action.as_str(), events[0].event.side, events[0].event.accepted_daa),
                ("checkpoint", Some(1), Some(150)));
            assert_eq!(timeout_result(live(&cache)).unwrap().end_daa, 200);
            let old_metadata = live(&cache).court.utxo.clone();
            let unrelated = prefix(&cache, 0, 130);
            accept(&mut cache, &invite, 11, 160, unrelated);
            assert_eq!(live(&cache).court.state.response_at, 150);
            let restored: Cache = serde_json::from_slice(&serde_json::to_vec(&cache).unwrap()).unwrap();
            restored.validate(artifact(), &invite).unwrap();
            assert_eq!(timeout_result(live(&restored)).unwrap().end_daa, 200);

            cache.tracker.apply(artifact(), &invite, &[hash(11), hash(10)], &[]).unwrap();
            assert_eq!(live(&cache).court.state, initial.court.state);
            assert_eq!(live(&cache).court.utxo.block_daa_score, initial.court.utxo.block_daa_score);
            accept(&mut cache, &invite, 12, 175, progress);
            assert_ne!(live(&cache).court.utxo.block_daa_score, old_metadata.block_daa_score);
            assert_eq!(timeout_result(live(&cache)).unwrap().end_daa, 225);
            let mut wrong_source = prefix(&cache, 0, 130);
            wrong_source.inputs[0].block_daa_score = 150;
            assert!(cache.tracker.apply(artifact(), &invite, &[], &[ChainBlock { hash: hash(13), daa: 180,
                transactions: vec![wrong_source] }]).is_err(), "retimed source metadata cannot be silently substituted");
            let unrelated = prefix(&cache, 0, 130);
            accept(&mut cache, &invite, 13, 180, unrelated);
            assert_eq!(live(&cache).court.state.response_at, 175);
            assert_timeout_guards(live(&cache), 225);
            let b = TxBuilder::new(artifact()).unwrap();
            let before_close = live(&cache).clone();
            let late = movement(&cache, 1, 1);
            let f = funding();
            let (tx, finish) = timeout_tx(&b, live(&cache), &key(1), &f, &key(4), 100_000).unwrap();
            let closing = AcceptedTx { tx, inputs: vec![live(&cache).court.utxo.clone(), f.utxo] };
            let events = accept(&mut cache, &invite, 14, 225, closing);
            assert_eq!((events[0].event.action.as_str(), events[0].event.side), ("timeout", Some(0)));
            accept(&mut cache, &invite, 15, 226, late.clone());
            let Stage::Collecting(c) = &cache.tracker.stage else { panic!() };
            assert_eq!(c.players[1].outpoint.transaction_id, late.tx.id());
            assert_eq!(c.players[1].utxo.block_daa_score, 226);
            assert_eq!(c.collector.state.finish, finish);
            let f = funding();
            let payout = reclaim_tx(&b, c, &f, &key(4), 100_000).unwrap();
            assert_eq!(payout.outputs[0].value, GAME_POT);
            assert_eq!(payout.outputs[0].script_public_key, p2pk(&pubkey(&key(1))));
            let inputs = std::iter::once(c.collector.utxo.clone()).chain(c.players.iter().map(|p| p.utxo.clone()))
                .chain(std::iter::once(f.utxo)).collect();
            accept(&mut cache, &invite, 16, 227, AcceptedTx { tx: payout, inputs });
            let Stage::Reclaimed(result) = &cache.tracker.stage else { panic!() };
            assert_eq!(result, &finish);
            cache.validate(artifact(), &invite).unwrap();
            cache.tracker.apply(artifact(), &invite, &[hash(16), hash(15), hash(14)], &[]).unwrap();
            assert_eq!(live(&cache).court.state, before_close.court.state);
            assert_eq!(live(&cache).court.utxo.block_daa_score, before_close.court.utxo.block_daa_score);
            let recovered: Cache = serde_json::from_slice(&serde_json::to_vec(&cache).unwrap()).unwrap();
            recovered.validate(artifact(), &invite).unwrap();
            assert_eq!(timeout_result(live(&recovered)).unwrap(), finish);
        }
    }
}
